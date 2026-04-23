//! UDS backend. Invariants 9, 10, 11, 17 — see module docs.

use std::io;
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

        use std::os::unix::fs::PermissionsExt;
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
