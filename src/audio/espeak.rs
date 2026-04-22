//! Runtime `espeak-ng` probe.

use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EspeakInfo {
    pub path: PathBuf,
}

/// `espeak-ng --version` exit-0 + resolve on `$PATH`.
pub fn probe() -> Option<EspeakInfo> {
    let out = Command::new("espeak-ng").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    which_in_path("espeak-ng").map(|path| EspeakInfo { path })
}

/// Walk `$PATH`, return first file-existing hit.
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
        assert!(which_in_path("sh").is_some());
    }

    #[test]
    fn which_rejects_impossible_name() {
        assert!(which_in_path("__tebis_absolutely_not_a_real_binary__").is_none());
    }
}
