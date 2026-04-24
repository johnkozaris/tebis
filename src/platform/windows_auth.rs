//! Windows-only helpers for building owner-only security descriptors,
//! extracting user SIDs, and driving RAII over a few Win32 handles.
//!
//! Consumed by both:
//! - `platform::peer_listener::windows` — named-pipe DACL at creation,
//!   plus `ImpersonateNamedPipeClient`-based peer SID extraction.
//! - `platform::secure_file::windows` — `CreateFileW` with an
//!   owner-only DACL, for secret writes that shouldn't depend on
//!   `%APPDATA%` inheritance alone.
//!
//! Split out so the two callers share one tested path for the
//! SID-string-to-SDDL-to-SECURITY_DESCRIPTOR chain; a bug in there
//! would otherwise affect both security-sensitive surfaces silently.

#![cfg(windows)]

use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

/// Collapse a `windows::core::Error` into a plain `io::Error` for
/// callers that don't care about the HRESULT.
pub fn to_io(e: windows::core::Error) -> io::Error {
    io::Error::other(format!("win32: {e}"))
}

/// `OpenProcessToken(current_process)` → `GetTokenInformation(TokenUser)`.
/// Returns the SID bytes copied into an owned `Vec`.
pub fn current_user_sid() -> windows::core::Result<Vec<u8>> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
    }
    let _guard = HandleGuard(token);
    token_user_sid(token)
}

/// Extract the `TokenUser` SID from an already-opened token handle.
///
/// Performs the standard two-call `GetTokenInformation` pattern: first
/// call with `size=0` returns `ERROR_INSUFFICIENT_BUFFER` and the real
/// size in `needed`; second call writes into a sized buffer.
pub fn token_user_sid(token: HANDLE) -> windows::core::Result<Vec<u8>> {
    let mut needed = 0u32;
    unsafe {
        // Intentionally ignore the error; we only want `needed`.
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    }
    if needed == 0 {
        return Err(windows::core::Error::from_thread());
    }
    let mut buf = vec![0u8; needed as usize];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )?;
    }
    let user = buf.as_ptr().cast::<TOKEN_USER>();
    // SAFETY: `buf` is a well-formed TOKEN_USER from GetTokenInformation;
    // its `User.Sid` field points into the same buffer.
    let sid_ptr = unsafe { (*user).User.Sid };
    let sid_len = unsafe { windows::Win32::Security::GetLengthSid(sid_ptr) } as usize;
    let sid_start = sid_ptr.0 as *const u8;
    // SAFETY: GetLengthSid returns the valid length of the SID bytes
    // in buf; we copy them into a standalone Vec the caller owns.
    let sid_bytes = unsafe { std::slice::from_raw_parts(sid_start, sid_len) }.to_vec();
    Ok(sid_bytes)
}

/// Convert a raw SID byte buffer to its standard string form
/// (e.g. `S-1-5-21-…`) via `ConvertSidToStringSidW`.
pub fn sid_to_string(sid_bytes: &[u8]) -> windows::core::Result<String> {
    let mut sid_str = PWSTR::null();
    // SAFETY: sid_bytes is a valid SID; `ConvertSidToStringSidW` writes
    // a LocalAlloc'd wide string into `sid_str` that we LocalFree below.
    unsafe {
        ConvertSidToStringSidW(
            PSID(sid_bytes.as_ptr() as *mut c_void),
            &mut sid_str,
        )?;
    }
    if sid_str.is_null() {
        return Err(windows::core::Error::from_thread());
    }
    let result = unsafe { wide_to_string(sid_str) };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid_str.0 as *mut c_void)));
    }
    Ok(result)
}

unsafe fn wide_to_string(p: PWSTR) -> String {
    let mut len = 0isize;
    while unsafe { *p.0.offset(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(p.0, len as usize) };
    String::from_utf16_lossy(slice)
}

/// A `SECURITY_DESCRIPTOR` allocated by
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`. `Drop` calls
/// `LocalFree` so callers can't leak the descriptor.
pub struct OwnedSecurityDescriptor {
    raw: PSECURITY_DESCRIPTOR,
}

impl OwnedSecurityDescriptor {
    /// Parse SDDL into a fresh security descriptor. Typical input for
    /// "owner-only, protected from parent inheritance":
    /// `format!("D:P(A;;GA;;;{our_sid})")`.
    pub fn from_sddl(sddl: &str) -> windows::core::Result<Self> {
        let wide: Vec<u16> = OsStr::new(sddl)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut raw = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut raw,
                None,
            )?;
        }
        if raw.0.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        Ok(Self { raw })
    }

    /// Pointer hand-off for `SECURITY_ATTRIBUTES.lpSecurityDescriptor`.
    /// Callers must ensure the `OwnedSecurityDescriptor` outlives any
    /// syscall that reads through this pointer.
    pub fn as_ptr(&self) -> *mut c_void {
        self.raw.0
    }
}

impl Drop for OwnedSecurityDescriptor {
    fn drop(&mut self) {
        if !self.raw.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.raw.0)));
            }
        }
    }
}

/// RAII `CloseHandle` for an owned Win32 HANDLE.
pub struct HandleGuard(pub HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Build the SDDL string for "owner-only, DACL protected from parent
/// inheritance" given a SID string. Used for both named-pipe DACLs
/// (`GA` = GENERIC_ALL) and file DACLs (callers can substitute `FA`
/// = FILE_ALL_ACCESS if they prefer the narrower grant).
#[must_use]
pub fn owner_only_sddl(sid_str: &str, rights_abbrev: &str) -> String {
    // D: discretionary ACL; P: protected (SE_DACL_PROTECTED);
    // A: Access Allowed; ;; (flags/object guid empty);
    // <rights>: GA / FA / etc.; ;; (inherit object guid / mask empty);
    // <SID>: the given SID.
    format!("D:P(A;;{rights_abbrev};;;{sid_str})")
}
