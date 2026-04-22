//! Single-instance guard via advisory `flock` on a PID file.
//!
//! `acquire` returns an RAII guard that holds an exclusive non-blocking
//! lock for the lifetime of the daemon. If another tebis is already
//! running, `acquire` returns `Locked { pid }` so the caller can print
//! a clear "already running as pid N" message and exit cleanly.
//!
//! The lock is released automatically when the process exits (kernel
//! closes the fd), so crashes / SIGKILL don't leave the lock stuck.
//! The file itself is removed on clean drop.

#![cfg(unix)]

use std::env;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub struct LockFile {
    path: PathBuf,
    // Kept alive so `flock` persists for the lifetime of this struct.
    _file: File,
}

#[derive(Debug)]
pub enum AcquireError {
    Io(std::io::Error),
    /// Another tebis already holds the lock. `pid` is what that process
    /// wrote into the file (may be `None` if we couldn't read it).
    Locked {
        path: PathBuf,
        pid: Option<u32>,
    },
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "lockfile I/O error: {e}"),
            Self::Locked { path, pid } => match pid {
                Some(pid) => write!(
                    f,
                    "another tebis is already running (pid {pid}, lock: {})",
                    path.display()
                ),
                None => write!(
                    f,
                    "another tebis is already running (lock: {})",
                    path.display()
                ),
            },
        }
    }
}

impl std::error::Error for AcquireError {}

/// `$XDG_RUNTIME_DIR/tebis.lock` on Linux / systemd, else
/// `/tmp/tebis-$USER.lock`. Matches the notify-socket path policy so
/// there's one consistent runtime dir per user.
pub fn default_path() -> PathBuf {
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("tebis.lock");
    }
    let user = env::var("USER").unwrap_or_else(|_| "unknown".into());
    PathBuf::from(format!("/tmp/tebis-{user}.lock"))
}

/// Acquire an exclusive non-blocking lock at `path`. On success, writes
/// the current pid into the file for diagnostics.
///
/// The file is opened with `mode(0o600)` at creation so the perms are
/// correct atomically — no TOCTOU window between `open` and `chmod`.
pub fn acquire(path: &Path) -> Result<LockFile, AcquireError> {
    // Ensure the parent dir exists — on Linux `$XDG_RUNTIME_DIR` is
    // normally created by systemd at login, but it can be GC'd at
    // logout. macOS falls back to `/tmp` which always exists. Without
    // this, the caller sees an opaque ENOENT that looks like a bug in
    // the lockfile code.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(AcquireError::Io)?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(AcquireError::Io)?;

    // SAFETY: flock(2) with a valid fd and valid flags is sound.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            let pid = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse().ok());
            return Err(AcquireError::Locked {
                path: path.to_path_buf(),
                pid,
            });
        }
        return Err(AcquireError::Io(err));
    }

    // Lock held — publish our pid.
    let mut handle = &file;
    handle.set_len(0).map_err(AcquireError::Io)?;
    writeln!(handle, "{}", std::process::id()).map_err(AcquireError::Io)?;

    Ok(LockFile {
        path: path.to_path_buf(),
        _file: file,
    })
}

impl Drop for LockFile {
    fn drop(&mut self) {
        // Kernel releases the flock on fd close. Remove the file so the
        // next startup finds a clean slate.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Return the pid of a tebis **that actually holds the flock**, if any.
/// Used by `tebis status` to report the running foreground.
///
/// Probes the lock directly (non-blocking exclusive request); only if
/// that fails with `EWOULDBLOCK` do we read the pid from the file. Just
/// checking `kill(pid, 0)` on a pid-from-file would produce false
/// positives when the pid has been recycled to an unrelated process.
pub fn active_holder(path: &Path) -> Option<u32> {
    let file = OpenOptions::new().read(true).open(path).ok()?;

    // SAFETY: flock(2) with a valid fd and valid flags is sound.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // We acquired the lock → nobody was holding it. Remove the
        // stale file so a future `tebis status` doesn't see an
        // orphaned pid from a crashed prior run. Safe here: we held
        // (briefly) the exclusive flock, and any parallel checker
        // racing with us would also acquire-and-then-lose the lock,
        // reaching the same conclusion.
        let _ = std::fs::remove_file(path);
        return None;
    }
    let errno = std::io::Error::last_os_error().raw_os_error();
    if errno != Some(libc::EWOULDBLOCK) {
        return None;
    }
    // Lock is held by someone; read their pid.
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp_path(tag: &str) -> PathBuf {
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!(
            "tebis-lockfile-test-{tag}-{}-{ns:x}.lock",
            std::process::id()
        ))
    }

    #[test]
    fn acquire_then_drop_removes_file() {
        let path = unique_tmp_path("acquire-drop");
        {
            let _guard = acquire(&path).expect("acquire");
            assert!(path.exists(), "lockfile should exist while held");
        }
        assert!(!path.exists(), "drop should remove the lockfile");
    }

    #[test]
    fn second_acquire_returns_locked_with_pid() {
        let path = unique_tmp_path("double-acquire");
        let _guard = acquire(&path).expect("first acquire");
        match acquire(&path) {
            Err(AcquireError::Locked { pid, .. }) => {
                assert_eq!(pid, Some(std::process::id()));
            }
            Err(AcquireError::Io(e)) => panic!("expected Locked, got Io: {e}"),
            Ok(_) => panic!("expected Locked, got Ok"),
        }
    }

    #[test]
    fn active_holder_cleans_stale_file() {
        let path = unique_tmp_path("stale-cleanup");
        // Create an unlocked file with a fake pid — simulates a crashed
        // prior run.
        std::fs::write(&path, "99999\n").expect("write stale pidfile");
        assert!(path.exists());
        assert_eq!(active_holder(&path), None, "no one holds the lock");
        assert!(
            !path.exists(),
            "active_holder should sweep the stale pidfile"
        );
    }

    #[test]
    fn active_holder_reports_pid_when_held() {
        let path = unique_tmp_path("held");
        let _guard = acquire(&path).expect("acquire");
        assert_eq!(active_holder(&path), Some(std::process::id()));
        assert!(path.exists(), "held lockfile must still be present");
    }
}
