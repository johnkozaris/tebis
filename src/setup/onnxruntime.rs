//! `libonnxruntime.{dylib,so}` detection + install for Kokoro-local TTS.
//! ort's `load-dynamic` uses libloading — which on macOS doesn't search
//! `/opt/homebrew/lib` — so we probe standard prefixes + `ORT_DYLIB_PATH`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use console::style;
use dialoguer::Confirm;
use dialoguer::theme::ColorfulTheme;

pub use super::installer::EnsureOutcome;
use super::installer::{
    PackageManager, detect_package_manager, install_argv, install_cmd_display,
};

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

    // Fedora/RHEL (dnf) and Alpine (apk) don't ship onnxruntime in
    // their default repos (April 2026). Trying the install exits non-
    // zero with nothing actionable. Steer the user at pip + manual
    // `ORT_DYLIB_PATH` instead of burning a confusing failure.
    if matches!(pm, PackageManager::Dnf | PackageManager::Apk) {
        println!();
        println!(
            "{} {} doesn't ship `onnxruntime` in its default repos.",
            style("ℹ").cyan(),
            pm.name(),
        );
        println!("   Install via Python pip and hand-set `ORT_DYLIB_PATH`:");
        println!();
        println!(
            "     {}",
            style("python3 -m pip install --user onnxruntime").bold()
        );
        println!(
            "     {}",
            style(r#"python3 -c 'import onnxruntime,os; print(os.path.join(os.path.dirname(onnxruntime.__file__),"capi","libonnxruntime.so.1"))'"#)
                .dim(),
        );
        println!(
            "   Add the printed path to {} as:",
            style("~/.config/tebis/env").bold()
        );
        println!(
            "     {}",
            style("ORT_DYLIB_PATH=/path/to/libonnxruntime.so.1").dim()
        );
        println!();
        println!("   Re-run {} once that's done.", style("tebis setup").bold());
        return Ok(EnsureOutcome::NoPackageManager);
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// `candidate_paths()` only has entries on macOS+Linux; on Windows
    /// the user supplies `ORT_DYLIB_PATH` manually (no package-manager
    /// auto-install is wired up yet).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn candidate_paths_non_empty() {
        let paths = candidate_paths();
        assert!(!paths.is_empty(), "at least one candidate for this platform");
    }

    #[test]
    fn package_name_for_every_pm() {
        for pm in [
            PackageManager::Brew,
            PackageManager::MacPorts,
            PackageManager::Apt,
            PackageManager::Dnf,
            PackageManager::Pacman,
            PackageManager::Zypper,
            PackageManager::Apk,
        ] {
            assert!(!package_name_for(pm).is_empty(), "{pm:?}");
        }
    }
}
