//! Named-pipe backend. Invariants 9 / 17 (rewritten) + invariant 20.
//!
//! # Peer-auth invariant (20)
//!
//! The ONLY way this backend verifies a peer is via
//! `ImpersonateNamedPipeClient` + `OpenThreadToken(TOKEN_QUERY)` +
//! `GetTokenInformation(TokenUser)` + `EqualSid`. We explicitly do
//! **not** call `GetNamedPipeClientProcessId` — its PID is spoofable
//! (Project Zero 2019-09, "Windows Exploitation Tricks: Spoofing
//! Named Pipe Client PID"). The impersonation token is kernel-verified.
//!
//! # DACL (invariant 17-equivalent)
//!
//! `CreateNamedPipe` is called with a `SECURITY_ATTRIBUTES` whose
//! `SECURITY_DESCRIPTOR` comes from
//! `windows_auth::owner_only_sddl(&our_sid_str, "GA")` —
//! `D:P(A;;GA;;;<OUR_SID>)`:
//! - `D:P` — protected DACL; parent (i.e. `\\.\pipe\`) inheritance can
//!   never widen access.
//! - `A;;GA;;;<OUR_SID>` — single Allow ACE granting `GENERIC_ALL` to
//!   our user's SID; no other principals have any permission.
//!
//! Three-layer defense collapses into two on Windows: the DACL is set
//! at pipe creation (`umask`/chmod have no NT analogue), and peer auth
//! happens on every accept.

use std::ffi::{OsString, c_void};
use std::io;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::sync::Mutex;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use windows::Win32::Foundation::{FALSE, HANDLE};
use windows::Win32::Security::{EqualSid, PSID, RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_QUERY};
use windows::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

use crate::platform::windows_auth::{
    HandleGuard, OwnedSecurityDescriptor, current_user_sid, owner_only_sddl, sid_to_string, to_io,
    token_user_sid,
};

pub type Conn = NamedPipeServer;

pub struct Listener {
    /// Pipe name kept as OsString so each `ServerOptions::create_with_*`
    /// call gets a fresh `&OsStr` without re-allocating.
    pipe_name: OsString,
    /// Our user's SID (owned heap copy). Compared via `EqualSid`
    /// against every accepted peer's impersonation-token SID.
    our_sid: Vec<u8>,
    /// Security descriptor that backs `security_attrs.lpSecurityDescriptor`.
    /// Must outlive every pipe instance.
    _descriptor: OwnedSecurityDescriptor,
    /// Raw `SECURITY_ATTRIBUTES` the pipe creation syscalls reference.
    /// Box-allocated so the pointer we hand into
    /// `create_with_security_attributes_raw` stays stable for the
    /// Listener's lifetime.
    security_attrs: Box<SECURITY_ATTRIBUTES>,
    /// Next pipe instance waiting for a client. Taken + replaced on
    /// each `accept()` call so `listener.accept().await` twice in a
    /// row always has a pending instance for clients to connect to.
    pending: Mutex<Option<NamedPipeServer>>,
}

// SAFETY: The raw pointers inside `OwnedSecurityDescriptor` (a
// LocalAlloc'd SECURITY_DESCRIPTOR) and inside `SECURITY_ATTRIBUTES`
// (`lpSecurityDescriptor` aliasing the same allocation) have no thread
// affinity — they point into a process-wide heap allocation that
// `LocalFree` can run from any thread. The `Listener` owns both
// exclusively and exposes no raw-pointer API, so moving it across
// threads is safe. `Sync` follows from the interior mutability being
// limited to `Mutex<Option<NamedPipeServer>>` (itself `Sync`).
unsafe impl Send for Listener {}
unsafe impl Sync for Listener {}

impl Listener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let our_sid = current_user_sid().map_err(to_io)?;
        let sid_str = sid_to_string(&our_sid).map_err(to_io)?;
        let sddl = owner_only_sddl(&sid_str, "GA");
        let descriptor = OwnedSecurityDescriptor::from_sddl(&sddl).map_err(to_io)?;

        let pipe_name = path.as_os_str().to_os_string();

        let mut security_attrs = Box::new(SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.as_ptr(),
            bInheritHandle: FALSE,
        });

        let first = unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .access_inbound(true)
                .access_outbound(true)
                .create_with_security_attributes_raw(
                    pipe_name.as_os_str(),
                    &mut *security_attrs as *mut SECURITY_ATTRIBUTES as *mut c_void,
                )?
        };

        Ok(Self {
            pipe_name,
            our_sid,
            _descriptor: descriptor,
            security_attrs,
            pending: Mutex::new(Some(first)),
        })
    }

    pub async fn accept(&self) -> io::Result<Conn> {
        // NOTE: there is a brief window between `take()` and the next
        // instance creation below during which no pipe instance is
        // pending. Clients that connect in that window get
        // ERROR_PIPE_BUSY. For our traffic pattern (one hook at a time,
        // serialized by the agent's event loop) this is harmless, but a
        // burst of hooks fired simultaneously could drop some. If that
        // shows up in practice, keep a 2-deep pending pool here.
        let server = self
            .pending
            .lock()
            .expect("peer_listener pending mutex poisoned")
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "listener drained"))?;

        server.connect().await?;

        // Queue up the next pending instance so the following accept
        // call already has a pipe for clients to connect to.
        let next = unsafe {
            ServerOptions::new()
                .access_inbound(true)
                .access_outbound(true)
                .create_with_security_attributes_raw(
                    self.pipe_name.as_os_str(),
                    &*self.security_attrs as *const SECURITY_ATTRIBUTES as *mut c_void,
                )?
        };
        *self
            .pending
            .lock()
            .expect("peer_listener pending mutex poisoned") = Some(next);

        Ok(server)
    }

    pub fn is_trusted_peer(&self, conn: &Conn) -> bool {
        let pipe_handle = HANDLE(conn.as_raw_handle());
        match peer_sid_via_impersonation(pipe_handle) {
            Ok(peer_sid) => {
                // SAFETY: `our_sid` and `peer_sid` are both valid
                // NT SID byte buffers (non-null, returned by
                // `GetTokenInformation(TokenUser)`).
                let equal = unsafe {
                    EqualSid(
                        PSID(self.our_sid.as_ptr() as *mut c_void),
                        PSID(peer_sid.as_ptr() as *mut c_void),
                    )
                }
                .is_ok();
                if !equal {
                    tracing::warn!(
                        "peer_listener: peer SID does not match our SID, rejecting connection"
                    );
                }
                equal
            }
            Err(e) => {
                tracing::warn!(err = %e, "peer_listener: impersonation-based peer check failed, rejecting");
                false
            }
        }
    }
}

/// For an impersonation-state thread token, extract the peer's SID.
/// Runs under a Drop guard that calls `RevertToSelf` so the thread
/// token always returns to our identity even on error paths.
fn peer_sid_via_impersonation(pipe: HANDLE) -> windows::core::Result<Vec<u8>> {
    unsafe { ImpersonateNamedPipeClient(pipe)? };
    let _revert = RevertGuard;

    let mut thread_token = HANDLE::default();
    unsafe {
        // Identification-level clients require `OpenAsSelf`; the token is still the peer's.
        OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut thread_token)?;
    }
    let _tok = HandleGuard(thread_token);
    token_user_sid(thread_token)
}

struct RevertGuard;

impl Drop for RevertGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = RevertToSelf();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::Duration;

    fn unique_pipe_name(tag: &str) -> String {
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("tebis-test-{tag}-{}-{ns:x}", std::process::id())
    }

    #[tokio::test]
    async fn same_user_identification_client_is_trusted() {
        let pipe_name = unique_pipe_name("ident");
        let pipe_path = PathBuf::from(format!(r"\\.\pipe\{pipe_name}"));
        let listener = Listener::bind(&pipe_path).expect("bind pipe");

        let script = format!(
            r#"
$pipe = [System.IO.Pipes.NamedPipeClientStream]::new(
    '.',
    '{pipe_name}',
    [System.IO.Pipes.PipeDirection]::InOut,
    [System.IO.Pipes.PipeOptions]::None,
    [System.Security.Principal.TokenImpersonationLevel]::Identification
)
$pipe.Connect(5000)
Start-Sleep -Milliseconds 500
$pipe.Dispose()
"#
        );

        let mut child = tokio::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn powershell.exe");

        let conn = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("accept timed out")
            .expect("accept");

        assert!(
            listener.is_trusted_peer(&conn),
            "same-user Identification client must pass SID peer auth"
        );

        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("client wait timed out")
            .expect("client wait");
        assert!(status.success(), "PowerShell pipe client failed: {status}");
    }
}
