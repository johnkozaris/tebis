//! Atomic binary replacement for `tebis upgrade`.
//!
//! Both backends accept (`new_binary`, `target`) and arrange for `target`
//! to contain the contents of `new_binary` after the call returns. The
//! tricky case is replacing a binary that's currently running:
//!
//! - **Unix**: `rename(2)` is atomic at the inode level. The running
//!   process still has the old inode mapped (and continues to execute
//!   it), but new `exec(2)` calls on the path resolve to the new file.
//!   No special handling.
//!
//! - **Windows**: the loader holds a mandatory lock on the running
//!   `.exe`. `rename` over it fails with sharing-violation. The
//!   workaround is `MoveFileExW(target, target.old, REPLACE_EXISTING)`
//!   first — Windows permits renaming a running .exe within the same
//!   directory — then `MoveFileExW(new_binary, target)`. The `.old`
//!   copy can be scheduled for boot-time delete with
//!   `MOVEFILE_DELAY_UNTIL_REBOOT` if `remove_file` rejects it.

use std::path::Path;

#[cfg(unix)]
pub use unix::atomic_replace;
#[cfg(windows)]
pub use windows::atomic_replace;

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use anyhow::{Context, Result};

    /// Make `target` execute from `new_binary`. On Unix, `rename(2)`
    /// over the path while the old inode is still running is the
    /// canonical pattern — existing processes keep their mapping, new
    /// `execve` calls see the new file.
    pub fn atomic_replace(new_binary: &Path, target: &Path) -> Result<()> {
        // Ensure 0755 — `rename` preserves the source's permissions and
        // the tmp file may have inherited the umask.
        fs::set_permissions(new_binary, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod 0755 {}", new_binary.display()))?;
        fs::rename(new_binary, target)
            .with_context(|| format!("renaming {} → {}", new_binary.display(), target.display()))
    }
}

#[cfg(windows)]
mod windows {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, anyhow};

    /// Windows-specific replace. Renames the running exe to a sibling
    /// `.old`, moves the new file into place, then attempts to delete
    /// the `.old`. The `.old` is still mapped by the loader so the
    /// delete may fail — that's fine; it's harmless on disk and gets
    /// removed on the next successful upgrade.
    pub fn atomic_replace(new_binary: &Path, target: &Path) -> Result<()> {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use std::ptr;

        use ::windows::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MoveFileExW,
        };
        use ::windows::core::PCWSTR;

        // Stage path: `<target>.old`. Must be on the same volume — it
        // is, since we're appending to `target`'s path.
        let mut old_path: PathBuf = target.to_path_buf();
        let new_name = format!(
            "{}.old",
            target
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("tebis.exe")
        );
        old_path.set_file_name(new_name);

        let target_w: Vec<u16> = target.as_os_str().encode_wide().chain([0]).collect();
        let old_w: Vec<u16> = old_path.as_os_str().encode_wide().chain([0]).collect();
        let new_w: Vec<u16> = new_binary.as_os_str().encode_wide().chain([0]).collect();

        // Step 1: rename running exe → sibling .old (only if target
        // exists — fresh install skips this).
        if target.exists() {
            // Best-effort cleanup of any leftover .old from a prior
            // upgrade. Ignore errors; if it's still locked, the next
            // MoveFileExW with REPLACE_EXISTING handles it.
            let _ = std::fs::remove_file(&old_path);
            // SAFETY: PCWSTR pointers point to null-terminated UTF-16
            // strings we own and outlive the call. `MoveFileExW` is a
            // documented Win32 API; thread-safe.
            let res = unsafe {
                MoveFileExW(
                    PCWSTR(target_w.as_ptr()),
                    Some(PCWSTR(old_w.as_ptr())),
                    MOVEFILE_REPLACE_EXISTING,
                )
            };
            res.map_err(|e| {
                anyhow!(
                    "MoveFileExW({} → {}): {e}",
                    target.display(),
                    old_path.display()
                )
            })?;
        }

        // Step 2: move new binary into target.
        // SAFETY: same as above.
        let res = unsafe {
            MoveFileExW(
                PCWSTR(new_w.as_ptr()),
                Some(PCWSTR(target_w.as_ptr())),
                MOVEFILE_REPLACE_EXISTING,
            )
        };
        res.map_err(|e| {
            anyhow!(
                "MoveFileExW({} → {}): {e}",
                new_binary.display(),
                target.display()
            )
        })
        .with_context(|| "moving new binary into place")?;

        // Step 3: best-effort delete of .old. The running process still
        // has it mapped; the unlink may fail with sharing-violation. If
        // so, leave it — the next upgrade's step 1 will clean it up,
        // and it's not on PATH so it has no runtime effect.
        let _ = std::fs::remove_file(&old_path);

        Ok(())
    }
}

/// Return value for `atomic_replace` callers that want to surface what
/// happened. Currently unused outside the module but kept for future
/// integration with `tebis upgrade`'s status display.
#[allow(dead_code)]
pub struct ReplaceReport {
    pub target: std::path::PathBuf,
}

impl ReplaceReport {
    #[allow(dead_code)]
    pub fn new(target: impl AsRef<Path>) -> Self {
        Self {
            target: target.as_ref().to_path_buf(),
        }
    }
}
