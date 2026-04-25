//! Atomic file writes: unique-tmp `O_CREAT|O_EXCL` open → fsync → rename → best-effort parent fsync.
//!
//! Cross-platform: on Unix the `mode` is set at creation (umask-bypass) and
//! re-asserted via `chmod`. On Windows the `mode` argument is ignored — NTFS
//! DACL inheritance from the parent dir controls access, and atomic replace
//! uses `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`. For owner-only private
//! writes (env file, invariant 20), use `platform::secure_file::atomic_write_private`
//! instead — that primitive sets an explicit owner-only DACL on Windows.

use std::fs;
use std::io::{self, Write as _};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

/// Max retries on the (extremely rare) tmp-name collision from
/// pid+nanos+counter. Two is plenty — a third failure is a real problem.
const TMP_RETRIES: usize = 3;

/// Atomic write with POSIX `mode` on Unix via `O_CREAT|O_EXCL`; pre-existing tmp paths
/// can't be reused. On Windows, `mode` is ignored and atomic replace uses `MoveFileExW`.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }

    let tmp = open_unique_tmp(path, mode)?;
    {
        let mut f = tmp.file;
        f.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.path.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.path.display()))?;
    }
    // Defence against ACL layers that drop OpenOptions mode.
    #[cfg(unix)]
    fs::set_permissions(&tmp.path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", tmp.path.display()))?;
    #[cfg(not(unix))]
    let _ = mode;

    atomic_rename(&tmp.path, path)?;

    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
        && let Err(e) = dir.sync_all()
    {
        tracing::debug!(err = %e, dir = %parent.display(), "atomic_write: parent dir fsync failed");
    }
    Ok(())
}

#[cfg(unix)]
fn atomic_rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).with_context(|| format!("renaming {} → {}", from.display(), to.display()))
}

#[cfg(windows)]
fn atomic_rename(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW};
    use windows::core::PCWSTR;

    let from_w: Vec<u16> = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let to_w: Vec<u16> = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // std::fs::rename on Windows is fine, but MoveFileExW with REPLACE_EXISTING
    // matches the explicit NTFS-atomic replace semantics we want for config files.
    unsafe {
        MoveFileExW(
            PCWSTR(from_w.as_ptr()),
            PCWSTR(to_w.as_ptr()),
            MOVEFILE_REPLACE_EXISTING,
        )
    }
    .with_context(|| format!("renaming {} → {}", from.display(), to.display()))
}

struct TmpFile {
    path: PathBuf,
    file: fs::File,
}

fn open_unique_tmp(final_path: &Path, mode: u32) -> Result<TmpFile> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut last_err: Option<io::Error> = None;
    for _ in 0..TMP_RETRIES {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let name = format!(
            "{}.tebis.tmp.{}.{nanos}.{seq}",
            final_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unnamed"),
            std::process::id(),
        );
        let tmp = final_path.with_file_name(name);
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(mode);
        #[cfg(not(unix))]
        let _ = mode;
        match opts.open(&tmp) {
            Ok(file) => return Ok(TmpFile { path: tmp, file }),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("opening {}", tmp.display())));
            }
        }
    }
    Err(anyhow::anyhow!(
        "atomic_write: exhausted {TMP_RETRIES} unique-tmp attempts near {}: {:?}",
        final_path.display(),
        last_err
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tebis-fsutil-{tag}-{}", std::process::id()))
    }

    #[cfg(unix)]
    #[test]
    fn creates_with_requested_mode_0600() {
        let p = tmp("0600");
        let _ = fs::remove_file(&p);
        atomic_write(&p, b"x\n", 0o600).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read(&p).unwrap(), b"x\n");
        let _ = fs::remove_file(&p);
    }

    #[cfg(unix)]
    #[test]
    fn creates_with_requested_mode_0644() {
        let p = tmp("0644");
        let _ = fs::remove_file(&p);
        atomic_write(&p, b"y", 0o644).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let _ = fs::remove_file(&p);
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_tightens_perms() {
        let p = tmp("tight");
        fs::write(&p, "old").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write(&p, b"new\n", 0o600).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn tempfile_is_cleaned_up_on_success() {
        let p = tmp("tempclean");
        atomic_write(&p, b"z", 0o600).unwrap();
        let parent = p.parent().unwrap();
        let stem = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let prefix = format!("{stem}.tebis.tmp.");
        for entry in fs::read_dir(parent).unwrap().flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(!name.starts_with(&prefix), "stale tmp: {name}");
        }
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn overwrites_existing_file_atomically() {
        let p = tmp("overwrite");
        let _ = fs::remove_file(&p);
        atomic_write(&p, b"a", 0o600).unwrap();
        atomic_write(&p, b"b", 0o600).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"b");
        let _ = fs::remove_file(&p);
    }
}
