//! `espeak-ng` detection, package-manager probe, interactive install.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use console::style;
use dialoguer::Confirm;
use dialoguer::theme::ColorfulTheme;

pub use crate::audio::espeak::{EspeakInfo, probe as probe_espeak_ng};

/// Platform package managers we drive directly. Order of enum variants
/// has no semantic meaning; priority order lives in
/// [`detect_package_manager`].
///
/// On macOS builds only `Brew` is ever constructed at runtime, so the
/// compiler flags the Linux variants as dead code. They're still
/// essential to the shared logic — `install_command`, tests, and the
/// future cross-compile story all enumerate every variant — so we
/// silence the lint at the type level rather than per-variant.
#[allow(dead_code, reason = "Linux-only variants are compiled on macOS for testing")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    /// macOS Homebrew. No sudo needed.
    Brew,
    /// Debian, Ubuntu, Mint, Pop!_OS.
    Apt,
    /// Fedora, RHEL 8+, CentOS Stream.
    Dnf,
    /// Arch, Manjaro, EndeavourOS.
    Pacman,
    /// openSUSE Leap / Tumbleweed.
    Zypper,
    /// Alpine, postmarketOS.
    Apk,
}

impl PackageManager {
    /// Human-readable command string — this is what we show to the
    /// user before asking "install now?". Must exactly match what
    /// [`Self::install_argv`] actually spawns so there's no surprise.
    pub const fn install_command(self) -> &'static str {
        match self {
            Self::Brew => "brew install espeak-ng",
            Self::Apt => "sudo apt install -y espeak-ng",
            Self::Dnf => "sudo dnf install -y espeak-ng",
            Self::Pacman => "sudo pacman -S --noconfirm espeak-ng",
            Self::Zypper => "sudo zypper install -y espeak-ng",
            Self::Apk => "sudo apk add espeak-ng",
        }
    }

    /// Short name for display ("apt", "dnf", …).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Brew => "brew",
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Pacman => "pacman",
            Self::Zypper => "zypper",
            Self::Apk => "apk",
        }
    }

    /// Argv for spawning the install. First element is the binary
    /// (we drop argv[0] and `Command::new` the rest). Every Linux
    /// manager prepends `sudo` because they need root; brew does not.
    const fn install_argv(self) -> &'static [&'static str] {
        match self {
            Self::Brew => &["brew", "install", "espeak-ng"],
            Self::Apt => &["sudo", "apt", "install", "-y", "espeak-ng"],
            Self::Dnf => &["sudo", "dnf", "install", "-y", "espeak-ng"],
            Self::Pacman => &["sudo", "pacman", "-S", "--noconfirm", "espeak-ng"],
            Self::Zypper => &["sudo", "zypper", "install", "-y", "espeak-ng"],
            Self::Apk => &["sudo", "apk", "add", "espeak-ng"],
        }
    }
}

/// Detect the first supported package manager on this host, in
/// priority order. Returns `None` on systems we don't know how to
/// drive (Windows, FreeBSD, exotic distros) — the caller prints
/// manual-install instructions.
///
/// Priority order corresponds to distro popularity among likely
/// tebis users; first binary found wins. `detect_package_manager`
/// itself is cheap (~1 ms for 5 PATH lookups) — fine to call multiple
/// times without caching.
pub fn detect_package_manager() -> Option<PackageManager> {
    #[cfg(target_os = "macos")]
    {
        if binary_on_path("brew") {
            return Some(PackageManager::Brew);
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

/// Outcome of the interactive install flow — so the caller can
/// distinguish "we're ready to use espeak-ng" from "user declined,
/// fall back" from "install failed, fall back."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// espeak-ng is available at this path and ready to use.
    Ready(PathBuf),
    /// User declined to install. Proceed without it (text-only / say).
    UserDeclined,
    /// Install attempted and either failed, or succeeded but binary
    /// still isn't on PATH. Same effect as `UserDeclined` for the
    /// caller, but logged differently so the wizard can suggest a
    /// shell restart.
    InstallFailed,
    /// No supported package manager detected. Manual install required.
    NoPackageManager,
}

/// Interactive: probe → offer install → re-probe. Prints status
/// lines directly (bypassing `tracing`) because this is a wizard
/// flow, not a library primitive.
///
/// The user is always asked to confirm before we run anything — even
/// brew, which doesn't need sudo. That's a deliberate "good citizen"
/// stance: the wizard never modifies the host silently, because tebis
/// might be run by someone trying out the tool who didn't expect it
/// to `apt install` things.
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
        println!("   Install espeak-ng manually, then re-run {}:", style("tebis setup").bold());
        println!(
            "     {}",
            style("https://github.com/espeak-ng/espeak-ng#installation").dim(),
        );
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

    // "Success" from the package manager doesn't guarantee the binary
    // is on PATH of the *current* shell — some distros put it in
    // /usr/sbin, homebrew's shim dir may not be on PATH for root.
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
        // `install_command` is for display; we must never print a
        // command that differs from what we actually spawn.
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
        // Brew explicitly must not use sudo; every Linux manager must.
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

    // `which_in_path` tests live in `crate::audio::espeak` — the helper
    // moved there to keep runtime audio deps separate from wizard UI.
}
