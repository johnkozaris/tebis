//! Single-instance guard via advisory lock + pidfile.
//!
//! `File::try_lock` is `flock(2)` on Unix and `LockFileEx` on Windows
//! (stabilized in 1.89). Both release the lock automatically when the
//! last file handle is dropped, so a crashed tebis frees its slot.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct LockFile {
    path: PathBuf,
    _file: File,
}

#[derive(Debug)]
pub enum AcquireError {
    Io(std::io::Error),
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

/// Unix: `$XDG_RUNTIME_DIR/tebis.lock`, else `/tmp/tebis-$USER.lock`.
/// Windows: `%LOCALAPPDATA%\tebis\tebis.lock`.
#[cfg(unix)]
pub fn default_path() -> PathBuf {
    use std::env;
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("tebis.lock");
    }
    let user = env::var("USER").unwrap_or_else(|_| "unknown".into());
    PathBuf::from(format!("/tmp/tebis-{user}.lock"))
}

#[cfg(windows)]
pub fn default_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "tebis")
        .map(|p| p.data_local_dir().join("tebis.lock"))
        .unwrap_or_else(|| std::env::temp_dir().join("tebis.lock"))
}

/// Exclusive non-blocking lock. On Unix `mode(0o600)` avoids the
/// open→chmod TOCTOU; on Windows the file inherits DACLs from
/// `%LOCALAPPDATA%\tebis\`, which is user-private by default.
pub fn acquire(path: &Path) -> Result<LockFile, AcquireError> {
    // `$XDG_RUNTIME_DIR` can be GC'd at logout; create it so errors are clear.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(AcquireError::Io)?;
    }

    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path).map_err(AcquireError::Io)?;

    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let pid = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse().ok());
            return Err(AcquireError::Locked {
                path: path.to_path_buf(),
                pid,
            });
        }
        Err(TryLockError::Error(e)) => return Err(AcquireError::Io(e)),
    }

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
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Pid of a tebis actually holding the lock. Probes the lock directly —
/// a stale pidfile from a crashed run returns `None`.
pub fn active_holder(path: &Path) -> Option<u32> {
    let file = OpenOptions::new().read(true).open(path).ok()?;
    match file.try_lock() {
        Ok(()) => None,
        Err(TryLockError::WouldBlock) => std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse().ok()),
        Err(TryLockError::Error(_)) => None,
    }
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
    fn active_holder_returns_none_for_unlocked_file() {
        let path = unique_tmp_path("stale-probe");
        std::fs::write(&path, "99999\n").expect("write stale pidfile");
        assert!(path.exists());
        assert_eq!(active_holder(&path), None, "no one holds the lock");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn active_holder_reports_pid_when_held() {
        let path = unique_tmp_path("held");
        let _guard = acquire(&path).expect("acquire");
        assert_eq!(active_holder(&path), Some(std::process::id()));
        assert!(path.exists(), "held lockfile must still be present");
    }
}
