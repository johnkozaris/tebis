//! `libonnxruntime.{dylib,so}` detection + install for the Kokoro-local path.
//!
//! `ort` with `load-dynamic` resolves the shared library via
//! `libloading::Library::new`, which on macOS doesn't search
//! `/opt/homebrew/lib` by default. We find the lib on disk (standard
//! brew prefixes / `ORT_DYLIB_PATH`), or offer to install via the
//! detected package manager, then write `ORT_DYLIB_PATH=<full-path>`
//! to the env file so the daemon's libloading call finds it at boot.
//!
//! Reuses the `PackageManager` enum from [`super::phonemizer`] — same
//! detection logic, different package name.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use console::style;
use dialoguer::Confirm;
use dialoguer::theme::ColorfulTheme;

use super::phonemizer::{PackageManager, detect_package_manager};

#[cfg(target_os = "macos")]
const DYLIB_NAME: &str = "libonnxruntime.dylib";
#[cfg(target_os = "linux")]
const DYLIB_NAME: &str = "libonnxruntime.so";
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const DYLIB_NAME: &str = "libonnxruntime";

/// Standard install locations, checked in order. `ORT_DYLIB_PATH` wins
/// so a user with a custom install isn't second-guessed.
fn candidate_paths() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("ORT_DYLIB_PATH")
        && !p.is_empty()
    {
        out.push(PathBuf::from(p));
    }
    #[cfg(target_os = "macos")]
    {
        out.push(PathBuf::from("/opt/homebrew/lib").join(DYLIB_NAME)); // Apple Silicon brew
        out.push(PathBuf::from("/usr/local/lib").join(DYLIB_NAME));    // Intel brew
        out.push(PathBuf::from("/opt/local/lib").join(DYLIB_NAME));    // MacPorts
    }
    #[cfg(target_os = "linux")]
    {
        out.push(PathBuf::from("/usr/lib").join(DYLIB_NAME));
        out.push(PathBuf::from("/usr/lib64").join(DYLIB_NAME));
        out.push(PathBuf::from("/usr/lib/x86_64-linux-gnu").join(DYLIB_NAME));
        out.push(PathBuf::from("/usr/lib/aarch64-linux-gnu").join(DYLIB_NAME));
    }
    out
}

/// First existing path from [`candidate_paths`], or `None`.
#[must_use]
pub fn probe() -> Option<PathBuf> {
    candidate_paths().into_iter().find(|p| p.exists())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureOutcome {
    Ready(PathBuf),
    UserDeclined,
    InstallFailed,
    NoPackageManager,
}

/// Probe → confirm → install → re-probe. `Ready(path)` caller writes
/// `ORT_DYLIB_PATH=<path>` to the env file.
pub fn ensure_or_install(theme: &ColorfulTheme) -> Result<EnsureOutcome> {
    if let Some(p) = probe() {
        println!(
            "  {} onnxruntime found at {}",
            style("✓").green(),
            style(p.display()).dim(),
        );
        return Ok(EnsureOutcome::Ready(p));
    }

    let Some(pm) = detect_package_manager() else {
        println!();
        println!(
            "{} onnxruntime is required for Kokoro local TTS, but we couldn't",
            style("ℹ").cyan(),
        );
        println!("   detect a supported package manager.");
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
                "   Install `onnxruntime` manually, then re-run {}.",
                style("tebis setup").bold()
            );
        }
        return Ok(EnsureOutcome::NoPackageManager);
    };

    let pkg = package_name_for(pm);
    let cmd_str = install_cmd_display(pm, pkg);

    println!();
    println!(
        "{} onnxruntime is required for Kokoro local TTS (not yet installed).",
        style("ℹ").cyan(),
    );
    println!("   We'll run:");
    println!();
    println!("     {}", style(&cmd_str).bold());
    println!();

    let confirm = Confirm::with_theme(theme)
        .with_prompt("Install now?")
        .default(true)
        .interact()
        .context("prompt: install onnxruntime")?;
    if !confirm {
        println!();
        println!(
            "   Skipping. Re-run {} after installing manually.",
            style("tebis setup").bold(),
        );
        return Ok(EnsureOutcome::UserDeclined);
    }

    println!();
    println!("   → {}", style(&cmd_str).dim());
    println!();
    let argv = install_argv(pm, pkg);
    let status = Command::new(&argv[0])
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
        println!("   Run the command manually and re-run `tebis setup`.");
        return Ok(EnsureOutcome::InstallFailed);
    }

    if let Some(p) = probe() {
        println!(
            "  {} onnxruntime installed at {}",
            style("✓").green(),
            style(p.display()).dim(),
        );
        Ok(EnsureOutcome::Ready(p))
    } else {
        println!();
        println!(
            "  {} install succeeded but {DYLIB_NAME} still not found at any known location.",
            style("⚠").yellow(),
        );
        println!(
            "   Try `brew --prefix onnxruntime` (macOS) or your distro's file-list, then",
        );
        println!("   set `ORT_DYLIB_PATH=<full-path>` in ~/.config/tebis/env by hand.");
        Ok(EnsureOutcome::InstallFailed)
    }
}

const fn package_name_for(pm: PackageManager) -> &'static str {
    match pm {
        PackageManager::Brew | PackageManager::MacPorts => "onnxruntime",
        PackageManager::Apt => "libonnxruntime-dev",
        PackageManager::Dnf | PackageManager::Pacman
        | PackageManager::Zypper | PackageManager::Apk => "onnxruntime",
    }
}

fn install_cmd_display(pm: PackageManager, pkg: &str) -> String {
    match pm {
        PackageManager::Brew => format!("brew install {pkg}"),
        PackageManager::MacPorts => format!("sudo port install {pkg}"),
        PackageManager::Apt => format!("sudo apt install -y {pkg}"),
        PackageManager::Dnf => format!("sudo dnf install -y {pkg}"),
        PackageManager::Pacman => format!("sudo pacman -S --noconfirm {pkg}"),
        PackageManager::Zypper => format!("sudo zypper install -y {pkg}"),
        PackageManager::Apk => format!("sudo apk add {pkg}"),
    }
}

fn install_argv(pm: PackageManager, pkg: &str) -> Vec<String> {
    let pkg = pkg.to_string();
    match pm {
        PackageManager::Brew => vec!["brew".into(), "install".into(), pkg],
        PackageManager::MacPorts => vec!["sudo".into(), "port".into(), "install".into(), pkg],
        PackageManager::Apt => vec!["sudo".into(), "apt".into(), "install".into(), "-y".into(), pkg],
        PackageManager::Dnf => vec!["sudo".into(), "dnf".into(), "install".into(), "-y".into(), pkg],
        PackageManager::Pacman => vec!["sudo".into(), "pacman".into(), "-S".into(), "--noconfirm".into(), pkg],
        PackageManager::Zypper => vec!["sudo".into(), "zypper".into(), "install".into(), "-y".into(), pkg],
        PackageManager::Apk => vec!["sudo".into(), "apk".into(), "add".into(), pkg],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_paths_non_empty() {
        let paths = candidate_paths();
        assert!(!paths.is_empty(), "at least one candidate for this platform");
    }

    #[test]
    fn install_argv_includes_pkg_name() {
        for pm in [
            PackageManager::Brew,
            PackageManager::MacPorts,
            PackageManager::Apt,
            PackageManager::Dnf,
            PackageManager::Pacman,
            PackageManager::Zypper,
            PackageManager::Apk,
        ] {
            let pkg = package_name_for(pm);
            let argv = install_argv(pm, pkg);
            assert!(
                argv.iter().any(|a| a == pkg),
                "argv for {pm:?} missing pkg {pkg}: {argv:?}"
            );
        }
    }

    #[test]
    fn install_cmd_display_matches_argv_head() {
        for pm in [
            PackageManager::Brew,
            PackageManager::MacPorts,
            PackageManager::Apt,
            PackageManager::Dnf,
            PackageManager::Pacman,
            PackageManager::Zypper,
            PackageManager::Apk,
        ] {
            let pkg = package_name_for(pm);
            let display = install_cmd_display(pm, pkg);
            let argv = install_argv(pm, pkg);
            assert_eq!(display, argv.join(" "), "display-vs-argv drift for {pm:?}");
        }
    }
}
