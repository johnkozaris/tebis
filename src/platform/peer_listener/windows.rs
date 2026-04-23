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
//! `SECURITY_DESCRIPTOR` is built from the SDDL `D:P(A;;GA;;;<OUR_SID>)`:
//! - `D:P` — protected DACL; parent (i.e. `\\.\pipe\`) inheritance can
//!   never widen access.
//! - `A;;GA;;;<OUR_SID>` — single Allow ACE granting `GENERIC_ALL` to
//!   our user's SID; no other principals have any permission.
//!
//! Three-layer defense collapses into two on Windows: the DACL is set
//! at pipe creation (`umask`/chmod have no NT analogue), and peer auth
//! happens on every accept.

use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::sync::Mutex;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    EqualSid, GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows::Win32::System::Pipes::{ImpersonateNamedPipeClient, RevertToSelf};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};
use windows::core::{PCWSTR, PWSTR};

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
    /// Box-allocated so the pointer we hand into `create_with_security_attributes_raw`
    /// stays stable for the Listener's lifetime.
    security_attrs: Box<SECURITY_ATTRIBUTES>,
    /// Next pipe instance waiting for a client. Taken + replaced on
    /// each `accept()` call so `listener.accept().await` twice in a
    /// row always has a pending instance for clients to connect to.
    pending: Mutex<Option<NamedPipeServer>>,
}

impl Listener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let our_sid = current_user_sid().map_err(to_io)?;
        let sid_str = sid_to_string(&our_sid).map_err(to_io)?;
        // D: discretionary ACL; P: protected (SE_DACL_PROTECTED);
        // A: Access Allowed; ;; (flags/object guid empty);
        // GA: generic all; ;; (inherit object guid / mask empty);
        // <SID>: the current user's SID.
        let sddl = format!("D:P(A;;GA;;;{sid_str})");
        let descriptor = OwnedSecurityDescriptor::from_sddl(&sddl).map_err(to_io)?;

        let pipe_name = path.as_os_str().to_os_string();

        let mut security_attrs = Box::new(SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.as_ptr(),
            bInheritHandle: windows::Win32::Foundation::BOOL(0),
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

// ---- Helpers -----------------------------------------------------------

fn to_io(e: windows::core::Error) -> io::Error {
    io::Error::other(format!("win32: {e}"))
}

/// `OpenProcessToken(current_process)` → `GetTokenInformation(TokenUser)`.
/// Returns the raw SID bytes copied into a `Vec` so the caller owns them.
fn current_user_sid() -> windows::core::Result<Vec<u8>> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
    }
    let _guard = HandleGuard(token);
    token_user_sid(token)
}

/// For an impersonation-state thread token, extract the peer's SID.
/// Runs under a Drop guard that calls `RevertToSelf` so the thread
/// token always returns to our identity even on error paths.
fn peer_sid_via_impersonation(pipe: HANDLE) -> windows::core::Result<Vec<u8>> {
    unsafe { ImpersonateNamedPipeClient(pipe)? };
    let _revert = RevertGuard;

    let mut thread_token = HANDLE::default();
    unsafe {
        // `OpenAsSelf = false` → we want the *impersonated* (peer's) token.
        OpenThreadToken(
            GetCurrentThread(),
            TOKEN_QUERY,
            false,
            &mut thread_token,
        )?;
    }
    let _tok = HandleGuard(thread_token);
    token_user_sid(thread_token)
}

fn token_user_sid(token: HANDLE) -> windows::core::Result<Vec<u8>> {
    // First call with size=0 returns ERROR_INSUFFICIENT_BUFFER and the
    // real size. Standard pattern.
    let mut needed = 0u32;
    unsafe {
        // Intentionally ignore the error; we only want `needed`.
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    }
    if needed == 0 {
        return Err(windows::core::Error::from_win32());
    }
    let mut buf = vec![0u8; needed as usize];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )?;
    }
    let user = buf.as_ptr().cast::<TOKEN_USER>();
    // SAFETY: `buf` is a well-formed TOKEN_USER from GetTokenInformation;
    // its `User.Sid` field points into the same buffer.
    let sid_ptr = unsafe { (*user).User.Sid };
    let sid_len = unsafe { windows::Win32::Security::GetLengthSid(sid_ptr) } as usize;
    let sid_start = sid_ptr.0 as *const u8;
    // SAFETY: GetLengthSid returns the valid length of the SID bytes
    // in buf; we copy them into a standalone Vec the caller owns.
    let sid_bytes = unsafe { std::slice::from_raw_parts(sid_start, sid_len) }.to_vec();
    Ok(sid_bytes)
}

fn sid_to_string(sid_bytes: &[u8]) -> windows::core::Result<String> {
    let mut sid_str = PWSTR::null();
    // SAFETY: sid_bytes is a valid SID from token_user_sid; PSID is a
    // non-owning pointer, and ConvertSidToStringSidW writes a freshly
    // LocalAlloc'd wide string into sid_str that we LocalFree below.
    unsafe {
        ConvertSidToStringSidW(
            PSID(sid_bytes.as_ptr() as *mut c_void),
            &mut sid_str,
        )?;
    }
    if sid_str.is_null() {
        return Err(windows::core::Error::from_win32());
    }
    let result = unsafe { wide_to_string(sid_str) };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid_str.0 as *mut c_void)));
    }
    Ok(result)
}

unsafe fn wide_to_string(p: PWSTR) -> String {
    let mut len = 0isize;
    while unsafe { *p.0.offset(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(p.0, len as usize) };
    String::from_utf16_lossy(slice)
}

// ---- RAII guards -------------------------------------------------------

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct RevertGuard;

impl Drop for RevertGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = RevertToSelf();
        }
    }
}

struct OwnedSecurityDescriptor {
    raw: PSECURITY_DESCRIPTOR,
}

impl OwnedSecurityDescriptor {
    fn from_sddl(sddl: &str) -> windows::core::Result<Self> {
        let wide: Vec<u16> = OsStr::new(sddl)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut raw = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut raw,
                None,
            )?;
        }
        if raw.0.is_null() {
            return Err(windows::core::Error::from_win32());
        }
        Ok(Self { raw })
    }

    fn as_ptr(&self) -> *mut c_void {
        self.raw.0
    }
}

impl Drop for OwnedSecurityDescriptor {
    fn drop(&mut self) {
        if !self.raw.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.raw.0)));
            }
        }
    }
}
