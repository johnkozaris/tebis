//! `espeak-ng` detection, package-manager probe, interactive install.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use console::style;
use dialoguer::Confirm;
use dialoguer::theme::ColorfulTheme;

pub use crate::audio::espeak::{EspeakInfo, probe as probe_espeak_ng};

/// Platform package managers we drive. Priority: see [`detect_package_manager`].
#[allow(dead_code, reason = "Linux-only variants are compiled on macOS for testing")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Brew,
    MacPorts,
    Apt,
    Dnf,
    Pacman,
    Zypper,
    Apk,
}

impl PackageManager {
    /// Display string — must mirror [`Self::install_argv`] exactly.
    pub const fn install_command(self) -> &'static str {
        match self {
            Self::Brew => "brew install espeak-ng",
            Self::MacPorts => "sudo port install espeak-ng",
            Self::Apt => "sudo apt install -y espeak-ng",
            Self::Dnf => "sudo dnf install -y espeak-ng",
            Self::Pacman => "sudo pacman -S --noconfirm espeak-ng",
            Self::Zypper => "sudo zypper install -y espeak-ng",
            Self::Apk => "sudo apk add espeak-ng",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Brew => "brew",
            Self::MacPorts => "port",
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Pacman => "pacman",
            Self::Zypper => "zypper",
            Self::Apk => "apk",
        }
    }

    /// Argv for spawning the install. Linux managers prepend `sudo`; brew doesn't.
    const fn install_argv(self) -> &'static [&'static str] {
        match self {
            Self::Brew => &["brew", "install", "espeak-ng"],
            Self::MacPorts => &["sudo", "port", "install", "espeak-ng"],
            Self::Apt => &["sudo", "apt", "install", "-y", "espeak-ng"],
            Self::Dnf => &["sudo", "dnf", "install", "-y", "espeak-ng"],
            Self::Pacman => &["sudo", "pacman", "-S", "--noconfirm", "espeak-ng"],
            Self::Zypper => &["sudo", "zypper", "install", "-y", "espeak-ng"],
            Self::Apk => &["sudo", "apk", "add", "espeak-ng"],
        }
    }
}

/// First supported package manager on PATH. `None` → manual install.
pub fn detect_package_manager() -> Option<PackageManager> {
    #[cfg(target_os = "macos")]
    {
        // Prefer Homebrew — more common. MacPorts fallback covers the
        // niche user who has `port` but not `brew`.
        if binary_on_path("brew") {
            return Some(PackageManager::Brew);
        }
        if binary_on_path("port") {
            return Some(PackageManager::MacPorts);
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        for (pm, bin) in [
            (PackageManager::Apt, "apt-get"),
            (PackageManager::Dnf, "dnf"),
            (PackageManager::Pacman, "pacman"),
            (PackageManager::Zypper, "zypper"),
            (PackageManager::Apk, "apk"),
        ] {
            if binary_on_path(bin) {
                return Some(pm);
            }
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

fn binary_on_path(name: &str) -> bool {
    crate::audio::espeak::which_in_path(name).is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureOutcome {
    Ready(PathBuf),
    UserDeclined,
    /// Install command failed or binary still isn't on PATH afterward.
    InstallFailed,
    NoPackageManager,
}

/// Probe → confirm → install → re-probe. Always asks first (even brew) — never silent.
pub fn ensure_or_install(theme: &ColorfulTheme) -> Result<EnsureOutcome> {
    if let Some(info) = probe_espeak_ng() {
        println!(
            "  {} espeak-ng found at {}",
            style("✓").green(),
            style(info.path.display()).dim(),
        );
        return Ok(EnsureOutcome::Ready(info.path));
    }

    let Some(pm) = detect_package_manager() else {
        println!();
        println!(
            "{} espeak-ng is required for local Kokoro TTS, but we couldn't",
            style("ℹ").cyan(),
        );
        println!("   detect a supported package manager on this system.");
        println!();
        #[cfg(target_os = "macos")]
        {
            println!(
                "   {} macOS has no built-in package manager. Install Homebrew first:",
                style("→").dim()
            );
            println!(
                "     {}",
                style(r#"/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)""#)
                    .dim(),
            );
            println!("   Then re-run {}.", style("tebis setup").bold());
        }
        #[cfg(not(target_os = "macos"))]
        {
            println!(
                "   Install espeak-ng manually, then re-run {}:",
                style("tebis setup").bold()
            );
            println!(
                "     {}",
                style("https://github.com/espeak-ng/espeak-ng#installation").dim(),
            );
        }
        return Ok(EnsureOutcome::NoPackageManager);
    };

    println!();
    println!(
        "{} espeak-ng is required for local Kokoro TTS (not yet installed).",
        style("ℹ").cyan(),
    );
    println!("   We'll run:");
    println!();
    println!("     {}", style(pm.install_command()).bold());
    println!();

    let confirm = Confirm::with_theme(theme)
        .with_prompt("Install now?")
        .default(true)
        .interact()
        .context("prompt: install espeak-ng")?;
    if !confirm {
        println!();
        println!(
            "   Skipping install. Re-run {} after installing manually.",
            style("tebis setup").bold(),
        );
        return Ok(EnsureOutcome::UserDeclined);
    }

    let argv = pm.install_argv();
    println!();
    println!("   → {}", style(pm.install_command()).dim());
    println!();
    let status = Command::new(argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("spawning {}", argv[0]))?;

    if !status.success() {
        println!();
        println!(
            "  {} {} exited with status {:?} — install did not complete.",
            style("✗").red(),
            pm.name(),
            status.code(),
        );
        println!("   You can run the command manually and re-run `tebis setup`.");
        return Ok(EnsureOutcome::InstallFailed);
    }

    // PM success doesn't guarantee PATH — re-probe and warn if missing.
    if let Some(info) = probe_espeak_ng() {
        println!(
            "  {} espeak-ng installed at {}",
            style("✓").green(),
            style(info.path.display()).dim(),
        );
        Ok(EnsureOutcome::Ready(info.path))
    } else {
        println!();
        println!(
            "  {} install command succeeded but `espeak-ng` still not on PATH.",
            style("⚠").yellow(),
        );
        println!("   You may need to open a new shell and re-run `tebis setup`.");
        Ok(EnsureOutcome::InstallFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_command_matches_argv_head() {
        for pm in [
            PackageManager::Brew,
            PackageManager::Apt,
            PackageManager::Dnf,
            PackageManager::Pacman,
            PackageManager::Zypper,
            PackageManager::Apk,
        ] {
            let cmd = pm.install_command();
            let argv = pm.install_argv();
            assert!(!argv.is_empty(), "argv must be non-empty for {pm:?}");
            let spaced = argv.join(" ");
            assert_eq!(cmd, spaced, "display string drift for {pm:?}");
        }
    }

    #[test]
    fn every_pm_mentions_espeak_ng() {
        for pm in [
            PackageManager::Brew,
            PackageManager::Apt,
            PackageManager::Dnf,
            PackageManager::Pacman,
            PackageManager::Zypper,
            PackageManager::Apk,
        ] {
            assert!(
                pm.install_command().contains("espeak-ng"),
                "{pm:?} install command missing espeak-ng"
            );
        }
    }

    #[test]
    fn linux_pms_use_sudo() {
        assert!(!PackageManager::Brew.install_command().starts_with("sudo"));
        for pm in [
            PackageManager::Apt,
            PackageManager::Dnf,
            PackageManager::Pacman,
            PackageManager::Zypper,
            PackageManager::Apk,
        ] {
            assert!(
                pm.install_command().starts_with("sudo "),
                "{pm:?} missing sudo prefix"
            );
        }
    }
}
