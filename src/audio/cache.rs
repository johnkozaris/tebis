//! Filesystem layout for downloaded models.
//!
//! Everything lives under `$XDG_DATA_HOME/tebis/` (or
//! `$HOME/.local/share/tebis/` as fallback) — same base dir as
//! `agent_hooks::data_dir` so we don't scatter tebis-owned files.
//!
//! ```text
//! $XDG_DATA_HOME/tebis/
//! ├── models/
//! │   ├── ggml-base.en.bin        (0644, only present if STT enabled)
//! │   ├── kokoro-v1.0.onnx        (0644, only present if TTS enabled)
//! │   └── voices-v1.0.bin         (0644, only present if TTS enabled)
//! ├── installed.json              (existing — agent_hooks)
//! └── ...
//! ```
//!
//! Dirs are created 0700, files 0644. Mirrors the dual-enforcement
//! pattern from `env_file::atomic_write_0600`: set the mode via
//! `OpenOptions::mode()` at creation AND `set_permissions` afterwards,
//! in case an ACL / umask / network filesystem widens the bits later.

use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const MODELS_SUBDIR: &str = "models";

/// Tmp-file suffix used during verified downloads.
/// Mirrors `agent_hooks/jsonfile.rs`'s `.tebis.tmp` convention.
pub(crate) const TMP_SUFFIX: &str = ".tebis.tmp";

/// Base directory for tebis-owned data — same as `agent_hooks` uses.
/// Re-exports the existing function so we have exactly one XDG-lookup
/// implementation in the crate.
pub fn base_dir() -> Result<PathBuf> {
    crate::agent_hooks::data_dir()
}

/// `$XDG_DATA_HOME/tebis/models/` — created (0700) if missing.
pub fn models_dir() -> Result<PathBuf> {
    let dir = base_dir()?.join(MODELS_SUBDIR);
    ensure_dir_0700(&dir)?;
    Ok(dir)
}

/// Final on-disk path for a named model file (e.g. `ggml-base.en.bin`).
/// Caller is responsible for ensuring the parent `models_dir` exists —
/// `install_atomic` does this for you.
pub fn model_path(file_name: &str) -> Result<PathBuf> {
    Ok(base_dir()?.join(MODELS_SUBDIR).join(file_name))
}

/// Tmp path used during download — same dir as the final file so `rename`
/// stays within a single filesystem (hard requirement for atomicity).
pub fn tmp_path_for(final_path: &Path) -> PathBuf {
    let name = final_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("model");
    final_path.with_file_name(format!("{name}{TMP_SUFFIX}"))
}

/// Create `dir` (and parents) with mode 0700. Idempotent — existing dir
/// is tightened to 0700 too.
pub fn ensure_dir_0700(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Open a file at `path` for writing, mode 0644 from creation, truncating
/// any existing content. Mirrors the pattern in `env_file::atomic_write_0600`
/// but for read-world-readable model files (they're public artifacts —
/// no privacy value in tightening models to 0600).
pub fn open_model_tmp(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(path)
}

/// Move a completed `.tebis.tmp` into place as `dst`, re-chmodding to 0644
/// post-rename. Renames within-dir, so atomic on any POSIX filesystem.
///
/// `tmp` and `dst` must be on the same filesystem — which they are if
/// `tmp` was `tmp_path_for(dst)`.
pub fn install_model_atomic(tmp: &Path, dst: &Path) -> Result<()> {
    fs::set_permissions(tmp, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("chmod 0644 {}", tmp.display()))?;
    fs::rename(tmp, dst).with_context(|| format!("renaming {} → {}", tmp.display(), dst.display()))
}

/// Walk `dir` and remove any file ending in `.tebis.tmp`. Best-effort —
/// logs warnings but never errors out, because a stuck `.tmp` from a
/// crashed download should not block startup.
///
/// Call at `AudioSubsystem::new` time, before any fetch runs.
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

    #[test]
    fn ensure_dir_0700_creates_with_mode() {
        let tmp = unique_tmpdir("mkdir");
        let nested = tmp.join("a/b/c");
        ensure_dir_0700(&nested).unwrap();
        let meta = fs::metadata(&nested).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn ensure_dir_0700_tightens_existing() {
        let tmp = unique_tmpdir("tighten");
        let d = tmp.join("loose");
        fs::create_dir(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o777)).unwrap();
        ensure_dir_0700(&d).unwrap();
        assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn open_model_tmp_creates_0644() {
        let tmp = unique_tmpdir("open0644");
        let p = tmp.join("m.bin");
        let _f = open_model_tmp(&p).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn install_model_atomic_renames_and_sets_0644() {
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
        assert!(tmp.join("keep.bin").exists(), "real file should survive");
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
