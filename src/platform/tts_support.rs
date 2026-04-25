//! Per-OS capability predicates for the TTS stack. Centralizes the
//! "which backends can we actually use on this host" question so
//! `setup/steps/tts.rs` and the ensurers don't each repeat
//! `#[cfg(target_os = "…")]` branches.
//!
//! **Native** = a TTS engine the OS ships by default with no user
//! install step. macOS has `say`; Windows has the WinRT
//! `SpeechSynthesizer` (OneCore voices, same engine Narrator uses).
//! Linux has no consensus shipped TTS engine, so we don't advertise
//! a "native" option — Kokoro-local or remote only.
//!
//! **Kokoro-local auto-install**: needs both `espeak-ng` and
//! `onnxruntime` on the host. On macOS + Linux we drive Homebrew /
//! MacPorts / apt / dnf / pacman / zypper / apk. On Windows neither
//! pipeline is wired up (per-machine MSIs trigger UAC, winget lags
//! on espeak-ng versions), so it's a manual advanced-opt-in.

/// Which native (zero-install) TTS backend the host OS ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeTtsKind {
    /// macOS `say` shell-out.
    Say,
    /// Windows WinRT `Windows.Media.SpeechSynthesis.SpeechSynthesizer`.
    WinRt,
}
impl NativeTtsKind {
    /// Backend-kind string matching `BackendConfig::kind_str()`.
    #[must_use]
    pub const fn kind_str(self) -> &'static str {
        match self {
            Self::Say => "say",
            Self::WinRt => "winrt",
        }
    }

    /// User-facing short label for wizard UI.
    #[must_use]
    pub const fn display(self) -> &'static str {
        match self {
            Self::Say => "macOS say",
            Self::WinRt => "Windows WinRT SpeechSynthesizer",
        }
    }
}

/// Returns the native TTS engine shipped with the host OS, or `None`
/// if no such engine is callable without user install work (Linux).
#[must_use]
pub const fn native_tts_kind() -> Option<NativeTtsKind> {
    #[cfg(target_os = "macos")]
    {
        Some(NativeTtsKind::Say)
    }
    #[cfg(target_os = "windows")]
    {
        Some(NativeTtsKind::WinRt)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// `true` iff the host OS has a working auto-install path for the
/// Kokoro-local TTS dependencies. Advanced-mode users can still pick
/// the backend and point `ORT_DYLIB_PATH` at a hand-installed DLL,
/// so this gates auto-install behavior, not the manual Advanced option.
#[must_use]
pub const fn kokoro_local_auto_install_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates_are_consistent_with_target_os() {
        #[cfg(target_os = "macos")]
        {
            assert_eq!(native_tts_kind(), Some(NativeTtsKind::Say));
            assert!(kokoro_local_auto_install_supported());
        }
        #[cfg(target_os = "linux")]
        {
            assert_eq!(native_tts_kind(), None);
            assert!(kokoro_local_auto_install_supported());
        }
        #[cfg(target_os = "windows")]
        {
            assert_eq!(native_tts_kind(), Some(NativeTtsKind::WinRt));
            assert!(!kokoro_local_auto_install_supported());
        }
    }
}
