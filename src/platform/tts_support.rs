//! Per-OS capability predicates for the TTS stack. Centralizes the
//! "which backends can we actually auto-install on this host" question
//! so `setup/steps/tts.rs` and the ensurers don't each repeat
//! `#[cfg(target_os = "…")]` branches.
//!
//! Today Kokoro-local needs both `espeak-ng` and `onnxruntime` on the
//! host. On macOS + Linux we know how to drive Homebrew / MacPorts /
//! apt / dnf / pacman / zypper / apk to install them and we ship
//! default `candidate_paths()` for the shared library. On Windows
//! neither the package-manager driver nor a default ORT path list is
//! wired up — the user would have to install manually, so we surface
//! `kokoro-remote` instead of dead-ending the Simple flow.

/// `true` iff the host OS has a working auto-install path for the
/// Kokoro-local TTS dependencies. Advanced-mode users can still pick
/// the backend and point `ORT_DYLIB_PATH` at a hand-installed DLL,
/// so this only gates the Simple flow and the Advanced option list.
#[must_use]
pub const fn kokoro_local_auto_install_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

/// `true` iff the `say` command ships with the host OS (macOS only).
#[must_use]
pub const fn say_backend_supported() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates_are_consistent_with_target_os() {
        #[cfg(target_os = "macos")]
        {
            assert!(kokoro_local_auto_install_supported());
            assert!(say_backend_supported());
        }
        #[cfg(target_os = "linux")]
        {
            assert!(kokoro_local_auto_install_supported());
            assert!(!say_backend_supported());
        }
        #[cfg(target_os = "windows")]
        {
            assert!(!kokoro_local_auto_install_supported());
            assert!(!say_backend_supported());
        }
    }
}
