//! Runtime `espeak-ng` probe for the audio subsystem.
//!
//! Non-interactive, dependency-free. The wizard's interactive
//! install flow (`setup::phonemizer`) re-uses [`probe`] to check
//! whether an install is needed; from the audio side we only care
//! whether the binary exists right now.

use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EspeakInfo {
    pub path: PathBuf,
}

/// `espeak-ng --version` exit-0 + resolve the binary on `$PATH`.
/// Returns `None` if the binary is missing or doesn't run.
pub fn probe() -> Option<EspeakInfo> {
    let out = Command::new("espeak-ng").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    which_in_path("espeak-ng").map(|path| EspeakInfo { path })
}

/// Minimal `which` — walk `$PATH` for `name`, return first file-existing
/// hit. Avoids pulling in the `which` crate for one call site.
pub(crate) fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_sh() {
        // `/bin/sh` exists on every POSIX host the tests run on.
        assert!(which_in_path("sh").is_some());
    }

    #[test]
    fn which_rejects_impossible_name() {
        assert!(which_in_path("__tebis_absolutely_not_a_real_binary__").is_none());
    }
}
