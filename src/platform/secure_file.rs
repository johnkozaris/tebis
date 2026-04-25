//! Atomic write of a private file (mode 0600 on Unix, owner-only DACL
//! on Windows).
//!
//! # Threat model
//!
//! The target file holds user-private state — typically secrets (bot
//! tokens, OAuth credentials in the tebis env file). The write must
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
//! 1. Tmp file is created via `CreateFileW` with a `SECURITY_ATTRIBUTES`
//!    whose `SECURITY_DESCRIPTOR` comes from
//!    `owner_only_sddl(&sid, "FA")` — `D:P(A;;FA;;;<OUR_SID>)`:
//!    - `D:P` — protected DACL; parent inheritance cannot widen access.
//!    - `A;;FA;;;<SID>` — single Allow ACE granting `FILE_ALL_ACCESS`
//!      to the current user's SID; no other principals.
//! 2. The handle is wrapped as a `std::fs::File` via
//!    `File::from_raw_handle`, so standard `write_all` + `sync_all`
//!    give us fsynced durable content.
//! 3. Atomic replace via `MoveFileExW(tmp, target,
//!    MOVEFILE_REPLACE_EXISTING)`. Per MSDN, this is atomic on NTFS.
//!
//! The DACL is explicit rather than relying on `%APPDATA%`
//! inheritance, so a caller placing the target in an unusual path
//! (test harness, user-overridden config dir, etc.) still gets
//! owner-only protection.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io::{self, Write as _};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    pub fn atomic_write_private(path: &Path, content: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            ensure_private_dir(parent)?;
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
    use std::fs::{self, File};
    use std::io::{self, Write as _};
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;

    use windows::Win32::Foundation::{FALSE, GENERIC_WRITE};
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::Storage::FileSystem::{
        CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE,
        MOVEFILE_REPLACE_EXISTING, MoveFileExW,
    };
    use windows::core::PCWSTR;

    use crate::platform::windows_auth::{
        OwnedSecurityDescriptor, current_user_sid, owner_only_sddl, sid_to_string, to_io,
    };

    fn to_wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn atomic_write_private(path: &Path, content: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            ensure_private_dir(parent)?;
        }
        let tmp = path.with_file_name(format!(
            "{}.tmp",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("env")
        ));

        let our_sid = current_user_sid().map_err(to_io)?;
        let sid_str = sid_to_string(&our_sid).map_err(to_io)?;
        let sddl = owner_only_sddl(&sid_str, "FA");
        let descriptor = OwnedSecurityDescriptor::from_sddl(&sddl).map_err(to_io)?;

        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.as_ptr(),
            bInheritHandle: FALSE,
        };

        let tmp_wide = to_wide(&tmp);

        let handle = unsafe {
            CreateFileW(
                PCWSTR(tmp_wide.as_ptr()),
                GENERIC_WRITE.0,
                FILE_SHARE_MODE(0), // no sharing while we write
                Some(&sa),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(to_io)?;

        {
            // SAFETY: `handle` is a valid kernel object just returned
            // from CreateFileW; `File::from_raw_handle` takes ownership
            // and closes it on drop.
            let mut file = unsafe { File::from_raw_handle(handle.0 as _) };
            file.write_all(content)?;
            file.sync_all()?;
        }

        let dst_wide = to_wide(path);
        unsafe {
            MoveFileExW(
                PCWSTR(tmp_wide.as_ptr()),
                PCWSTR(dst_wide.as_ptr()),
                MOVEFILE_REPLACE_EXISTING,
            )
        }
        .map_err(to_io)?;

        Ok(())
    }

    pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)?;
        tighten_dir_dacl(path)
    }

    /// Apply an owner-only protected DACL to `path` via
    /// `SetNamedSecurityInfoW`. Called by [`ensure_private_dir`] after
    /// `create_dir_all` so whether the directory was just created with
    /// default inheritance or pre-existed with looser permissions, we
    /// end up with the same narrow ACL (`D:P(A;;FA;;;<OUR_SID>)`).
    ///
    /// We only touch `DACL_SECURITY_INFORMATION` + `PROTECTED_DACL_SECURITY_INFORMATION`,
    /// leaving owner/group alone. Setting owner requires `SeRestorePrivilege`,
    /// which a user-mode daemon doesn't hold; the default owner on
    /// newly-created dirs is already the current user, so leaving it
    /// alone is both correct and safer.
    fn tighten_dir_dacl(path: &Path) -> io::Result<()> {
        use windows::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
        use windows::Win32::Security::{
            ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
            PROTECTED_DACL_SECURITY_INFORMATION,
        };
        use windows::core::BOOL;

        let our_sid = current_user_sid().map_err(to_io)?;
        let sid_str = sid_to_string(&our_sid).map_err(to_io)?;
        let sddl = owner_only_sddl(&sid_str, "FA");
        let descriptor = OwnedSecurityDescriptor::from_sddl(&sddl).map_err(to_io)?;

        let mut dacl_present = BOOL(0);
        let mut dacl_ptr: *mut ACL = std::ptr::null_mut();
        let mut dacl_defaulted = BOOL(0);
        // SAFETY: `descriptor` is a well-formed self-relative SD from SDDL;
        // we pass addresses of stack locals for the out params.
        unsafe {
            GetSecurityDescriptorDacl(
                windows::Win32::Security::PSECURITY_DESCRIPTOR(descriptor.as_ptr()),
                &mut dacl_present,
                &mut dacl_ptr,
                &mut dacl_defaulted,
            )
            .map_err(to_io)?;
        }
        if dacl_present.0 == 0 || dacl_ptr.is_null() {
            return Err(io::Error::other(
                "secure_file: SDDL did not produce a present DACL (internal error)",
            ));
        }

        let path_wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: `path_wide` is NUL-terminated; `dacl_ptr` is valid for
        // the lifetime of `descriptor`, which outlives this call.
        let rc = unsafe {
            SetNamedSecurityInfoW(
                PCWSTR(path_wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(dacl_ptr as *const ACL),
                None,
            )
        };
        if rc.0 != 0 {
            return Err(io::Error::from_raw_os_error(rc.0 as i32));
        }
        Ok(())
    }

    /// No-op on Windows. Hook scripts on Windows are `.ps1` files
    /// invoked via `powershell.exe` (or `pwsh.exe`); they don't need
    /// an executable bit.
    pub fn set_owner_executable(_path: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
pub use unix::{atomic_write_private, ensure_private_dir, set_owner_executable};
#[cfg(windows)]
pub use windows::{atomic_write_private, ensure_private_dir, set_owner_executable};
