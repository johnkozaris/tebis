//! UDS backend. Invariants 9, 10, 11, 17 — see module docs.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

pub type Conn = UnixStream;

pub struct Listener {
    inner: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Bind a UDS at `path` with owner-only perms. Any pre-existing
    /// socket file at that path is unlinked first (symlink-safe —
    /// `remove_file` doesn't follow symlinks, and `/tmp`'s sticky bit
    /// blocks clobbering another user's file).
    ///
    /// Three-layer defense per invariant 17:
    /// - (a) `umask(0o177)` around bind so the filesystem entry is
    ///   created with mode `0600` at the syscall level.
    /// - (b) Explicit `chmod 0600` after bind (belt-and-suspenders
    ///   against unusual init umasks / ACL layers).
    /// - (c) `peer_cred` uid check on every accept — see
    ///   [`Self::is_trusted_peer`].
    pub fn bind(path: &Path) -> io::Result<Self> {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        // SAFETY: `umask(2)` is async-signal-safe; prior mask restored
        // immediately after the bind call.
        let prior_umask = unsafe { libc::umask(0o177) };
        let bind_result = UnixListener::bind(path);
        unsafe {
            libc::umask(prior_umask);
        }
        let inner = bind_result?;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

        Ok(Self {
            inner,
            path: path.to_path_buf(),
        })
    }

    pub async fn accept(&self) -> io::Result<Conn> {
        let (stream, _addr) = self.inner.accept().await?;
        Ok(stream)
    }

    /// Kernel-authenticated peer check via `SO_PEERCRED`.
    /// `peer_cred`'s pid field is racy — trust only `uid`. A
    /// `peer_cred` failure rejects, so a misbehaving stack can't
    /// silently bypass the gate.
    pub fn is_trusted_peer(&self, conn: &Conn) -> bool {
        // SAFETY: `geteuid` is async-signal-safe and infallible.
        let our_euid = unsafe { libc::geteuid() };
        match conn.peer_cred() {
            Ok(cred) if cred.uid() == our_euid => true,
            Ok(cred) => {
                tracing::warn!(
                    peer_uid = cred.uid(),
                    our_euid,
                    "peer_listener: rejecting connection from different uid"
                );
                false
            }
            Err(e) => {
                tracing::warn!(err = %e, "peer_listener: peer_cred failed, rejecting connection");
                false
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Clean up the socket file so a restart doesn't have to deal
        // with a stale entry. Best-effort — we already unlink on bind.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    //! Integration tests for the Unix peer-auth path. Exercises the
    //! same `peer_cred` → uid-compare code that the notify listener
    //! gates every hook connection on (invariant 17c).
    //!
    //! `bind()` flips `libc::umask` process-globally (invariant 17a),
    //! so these tests serialize on the same mutex the env-mutating
    //! `agent_hooks` tests use — a parallel `fs::create_dir_all`
    //! inside the umask window would get mode 0o600 and break that
    //! test.

    use super::*;
    use crate::agent_hooks::test_support::env_lock;

    /// UDS paths are capped at `SUN_LEN` (~104 bytes on macOS, 108 on
    /// Linux), so temp-dir on some hosts (`/var/folders/xx/...` on
    /// macOS) only leaves a handful of bytes for the basename. Use a
    /// short tag + the last 6 hex digits of the ns clock.
    fn unique_socket_path(tag: &str) -> PathBuf {
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("tp-{tag}-{:x}.s", ns & 0xff_ffff))
    }

    /// Bind while holding `env_lock`, then drop the lock before any
    /// `await` — `std::sync::Mutex` + await would be a footgun, and
    /// the umask-sensitive window closes as soon as `bind()` returns.
    fn locked_bind(path: &Path) -> Listener {
        let _lock = env_lock();
        Listener::bind(path).expect("bind")
    }

    #[tokio::test]
    async fn bind_creates_0600_socket() {
        let path = unique_socket_path("mode");
        let listener = locked_bind(&path);
        let meta = std::fs::metadata(&path).expect("stat socket");
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "socket should be mode 0600 (invariant 17b)"
        );
        drop(listener);
        assert!(
            !path.exists(),
            "listener Drop should clean up the socket file"
        );
    }

    #[tokio::test]
    async fn bind_replaces_pre_existing_socket_file() {
        let path = unique_socket_path("repl");
        std::fs::write(&path, b"stale").expect("seed stale file");
        // A stale file at the bind path should be silently unlinked,
        // not cause bind to fail with EADDRINUSE.
        let _listener = locked_bind(&path);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn same_process_peer_is_trusted() {
        let path = unique_socket_path("self");
        let listener = locked_bind(&path);

        let connect_path = path.clone();
        let client_task =
            tokio::spawn(async move { tokio::net::UnixStream::connect(&connect_path).await });

        let conn = listener.accept().await.expect("accept");
        let _client = client_task.await.expect("join").expect("connect");

        let our_euid = unsafe { libc::geteuid() };
        let peer_cred = conn.peer_cred().expect("peer_cred");
        assert_eq!(
            peer_cred.uid(),
            our_euid,
            "peer_cred must report our uid for a same-process connection"
        );
        assert!(
            listener.is_trusted_peer(&conn),
            "same-process connection must pass peer_cred uid check"
        );
    }
}
