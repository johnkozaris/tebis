//! Display-only host name lookup. Used by the inspect dashboard; not a
//! security surface (the auth gate is numeric user id, invariant 1).

#[cfg(unix)]
mod unix {
    #[must_use]
    pub fn current() -> String {
        let mut buf = [0u8; 256];
        // SAFETY: `gethostname` writes at most `buf.len()` bytes into our
        // buffer; no preconditions.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            return "(unknown)".to_string();
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    }
}

#[cfg(windows)]
mod windows {
    #[must_use]
    pub fn current() -> String {
        // `COMPUTERNAME` is the NetBIOS name — fine for a dashboard
        // display. The DNS-form name via `GetComputerNameExW` requires
        // the `windows` crate, which we'll pull in when it's justified
        // by a security-relevant call; invariant 1 is numeric uid, not
        // hostname, so display-only is all we need here.
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "(unknown)".to_string())
    }
}

#[cfg(unix)]
pub use unix::current;
#[cfg(windows)]
pub use windows::current;
