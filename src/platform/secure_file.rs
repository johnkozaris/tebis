//! Atomic write of a private file (mode 0600 on Unix, DACL-restricted
//! on Windows).
//!
//! # Threat model
//!
//! The target file holds user-private state — typically secrets (bot
//! tokens, OAuth credentials in `~/.config/tebis/env`). The write must
//! be atomic (no partial file visible on crash) and the resulting file
//! must not be readable by other local users.
//!
//! # Platform contract
//!
//! ## Unix
//! 1. Tmp file is opened with `O_CREAT | O_WRONLY | O_TRUNC` and
//!    `mode(0o600)`, so the mode is set on creation without any umask
//!    window.
//! 2. After the content is written and `fsync`'d, a belt-and-suspenders
//!    `chmod 0o600` runs in case an ACL layer stripped the creation mode.
//! 3. Atomic `rename(tmp, target)`.
//! 4. Best-effort `fsync` on the containing dir (POSIX requires this
//!    for rename durability; NFS/tmpfs may reject it).
//!
//! ## Windows
//! **This backend is Phase-3 incomplete.** It currently writes with
//! default permissions inherited from the containing directory. That's
//! safe when the caller places the target in a user-scoped location
//! (`%APPDATA%`, `%LOCALAPPDATA%` — both have owner-only DACLs by
//! default on modern Windows).
//!
//! Phase 3 of the Windows port will replace this with an explicit DACL
//! (owner-only ACE, `SE_DACL_PROTECTED` so parent inheritance can't
//! widen access) via the `windows` crate. Do **not** write secrets
//! through this function on Windows to a shared path until that lands.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io::{self, Write as _};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    pub fn atomic_write_private(path: &Path, content: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_file_name(format!(
            "{}.tmp",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("env")
        ));

        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(content)?;
            f.sync_all()?;
        }
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent()
            && let Ok(dir) = fs::File::open(parent)
            && let Err(e) = dir.sync_all()
        {
            tracing::debug!(err = %e, dir = %parent.display(), "secure_file: parent dir fsync failed");
        }
        Ok(())
    }

    pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)?;
        // Tightens an existing looser dir (e.g. pre-tebis `mkdir -m 0755`).
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }

    /// Mark an existing file owner-executable (mode 0700). Used for hook
    /// scripts — the agent needs to invoke them directly.
    pub fn set_owner_executable(path: &Path) -> io::Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
}

#[cfg(windows)]
mod windows {
    use std::fs;
    use std::io::{self, Write as _};
    use std::path::Path;

    pub fn atomic_write_private(path: &Path, content: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_file_name(format!(
            "{}.tmp",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("env")
        ));

        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(content)?;
            f.sync_all()?;
        }
        // Windows has no rename-over-existing-file guarantee from
        // `fs::rename`; if the target exists, remove it first. This
        // is NOT atomic — Phase 3 will switch to `MoveFileEx` with
        // `MOVEFILE_REPLACE_EXISTING`.
        if path.exists() {
            let _ = fs::remove_file(path);
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
        // Phase-3 TODO: set an explicit DACL (owner-only ACE +
        // SE_DACL_PROTECTED so parent inheritance can't widen access)
        // via the `windows` crate's Win32_Security_Authorization.
        // Until then the directory inherits DACLs from %LOCALAPPDATA%,
        // which is user-owned on modern Windows — acceptable for
        // personal-daemon deployments but not for shared hosts.
    }

    /// No-op on Windows. Hook scripts on Windows are `.ps1` files
    /// invoked via `powershell.exe` (or `pwsh.exe`); they don't need
    /// an executable bit and the Phase-2 Windows hook writer emits
    /// them with default DACLs inheriting from `%LOCALAPPDATA%`.
    pub fn set_owner_executable(_path: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// Atomic write of a file whose contents should only be readable by the
/// owner. See module docs for the per-platform contract and the Windows
/// Phase 3 caveat.
#[cfg(unix)]
pub use unix::{atomic_write_private, ensure_private_dir, set_owner_executable};
#[cfg(windows)]
pub use windows::{atomic_write_private, ensure_private_dir, set_owner_executable};
