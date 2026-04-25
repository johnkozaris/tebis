//! Model-file cache — `<data_dir>/models/`.
//!
//! Dirs are owner-only (0700 on Unix, inherited DACL on Windows —
//! `%LOCALAPPDATA%` is user-private by default). Model files themselves
//! are world-readable on Unix (0644) since they're just downloaded
//! ONNX/Whisper blobs with no secret material; on Windows they inherit
//! parent permissions.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::platform::secure_file;

const MODELS_SUBDIR: &str = "models";

const TMP_SUFFIX: &str = ".tebis.tmp";

/// Same dir as `agent_hooks::data_dir` — single `platform::paths` lookup.
fn base_dir() -> Result<PathBuf> {
    crate::agent_hooks::data_dir()
}

/// `<data_dir>/models/` — created with owner-only perms if missing.
pub fn models_dir() -> Result<PathBuf> {
    let dir = base_dir()?.join(MODELS_SUBDIR);
    secure_file::ensure_private_dir(&dir)?;
    Ok(dir)
}

pub fn model_path(file_name: &str) -> Result<PathBuf> {
    Ok(base_dir()?.join(MODELS_SUBDIR).join(file_name))
}

/// Same-dir tmp path for atomic rename.
pub fn tmp_path_for(final_path: &Path) -> PathBuf {
    let name = final_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("model");
    final_path.with_file_name(format!("{name}{TMP_SUFFIX}"))
}

/// Open a fresh, truncated model tmp file for writing. Mode 0644 on
/// Unix (models are public blobs — no 0600 required); default DACL on
/// Windows.
pub fn open_model_tmp(path: &Path) -> io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    set_world_readable_create_mode(&mut opts);
    opts.open(path)
}

/// Atomic install. `tmp` and `dst` must be on the same FS
/// (`tmp_path_for` ensures this). On Unix the file is chmodded to 0644
/// pre-rename so a restrictive umask doesn't leave it owner-only; on
/// Windows no mode is set (Windows files inherit the parent DACL).
pub fn install_model_atomic(tmp: &Path, dst: &Path) -> Result<()> {
    set_world_readable_existing_mode(tmp)
        .with_context(|| format!("setting world-readable mode on {}", tmp.display()))?;
    fs::rename(tmp, dst).with_context(|| format!("renaming {} → {}", tmp.display(), dst.display()))
}

#[cfg(unix)]
fn set_world_readable_create_mode(opts: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o644);
}

#[cfg(windows)]
fn set_world_readable_create_mode(_opts: &mut fs::OpenOptions) {}

#[cfg(unix)]
fn set_world_readable_existing_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))
}

#[cfg(windows)]
fn set_world_readable_existing_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Best-effort cleanup of `.tebis.tmp` leftovers. Must not block startup.
pub fn reap_stale_tmps(dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(TMP_SUFFIX))
        {
            if let Err(e) = fs::remove_file(&path) {
                tracing::warn!(?path, error = %e, "failed to reap stale tmp file");
            } else {
                tracing::debug!(?path, "reaped stale tmp file from prior download");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tebis-audio-cache-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    // Mode-bit assertions are Unix-only. The Windows secure_file
    // backend uses DACL inheritance — no mode concept to check.
    #[cfg(unix)]
    #[test]
    fn models_dir_helper_tightens_to_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = unique_tmpdir("mkdir");
        let nested = tmp.join("a/b/c");
        secure_file::ensure_private_dir(&nested).unwrap();
        assert_eq!(
            fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(&tmp).ok();
    }

    #[cfg(unix)]
    #[test]
    fn models_dir_helper_tightens_existing_looser_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = unique_tmpdir("tighten");
        let d = tmp.join("loose");
        fs::create_dir(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o777)).unwrap();
        secure_file::ensure_private_dir(&d).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(&tmp).ok();
    }

    #[cfg(unix)]
    #[test]
    fn open_model_tmp_creates_0644() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = unique_tmpdir("open0644");
        let p = tmp.join("m.bin");
        let _f = open_model_tmp(&p).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o644
        );
        fs::remove_dir_all(&tmp).ok();
    }

    #[cfg(unix)]
    #[test]
    fn install_model_atomic_renames_and_sets_0644() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = unique_tmpdir("install");
        let tmp_file = tmp.join("m.bin.tebis.tmp");
        fs::write(&tmp_file, b"payload").unwrap();
        fs::set_permissions(&tmp_file, fs::Permissions::from_mode(0o600)).unwrap();
        let dst = tmp.join("m.bin");
        install_model_atomic(&tmp_file, &dst).unwrap();
        assert!(!tmp_file.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"payload");
        assert_eq!(
            fs::metadata(&dst).unwrap().permissions().mode() & 0o777,
            0o644
        );
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn reap_stale_tmps_removes_only_tmp_files() {
        let tmp = unique_tmpdir("reap");
        fs::write(tmp.join("keep.bin"), b"A").unwrap();
        fs::write(tmp.join("a.bin.tebis.tmp"), b"B").unwrap();
        fs::write(tmp.join("b.onnx.tebis.tmp"), b"C").unwrap();
        reap_stale_tmps(&tmp).unwrap();
        assert!(tmp.join("keep.bin").exists());
        assert!(!tmp.join("a.bin.tebis.tmp").exists());
        assert!(!tmp.join("b.onnx.tebis.tmp").exists());
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn reap_stale_tmps_tolerates_missing_dir() {
        let missing = std::env::temp_dir().join(format!(
            "tebis-audio-reap-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        assert!(reap_stale_tmps(&missing).is_ok());
    }

    #[test]
    fn tmp_path_for_uses_same_dir() {
        let dst = PathBuf::from("/some/dir/model.bin");
        let tmp = tmp_path_for(&dst);
        assert_eq!(tmp, PathBuf::from("/some/dir/model.bin.tebis.tmp"));
    }
}
