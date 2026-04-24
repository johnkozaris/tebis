//! `espeak-ng` detection, package-manager probe, interactive install.

use std::process::Command;

use anyhow::{Context, Result};
use console::style;
use dialoguer::Confirm;
use dialoguer::theme::ColorfulTheme;

pub use crate::audio::espeak::{EspeakInfo, probe as probe_espeak_ng};
pub use super::installer::{EnsureOutcome, PackageManager, detect_package_manager};
use super::installer::{install_argv, install_cmd_display};

const PKG: &str = "espeak-ng";

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
        print_no_pm_guidance();
        return Ok(EnsureOutcome::NoPackageManager);
    };

    let cmd_str = install_cmd_display(pm, PKG);
    println!();
    println!(
        "{} espeak-ng is required for local Kokoro TTS (not yet installed).",
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
        .context("prompt: install espeak-ng")?;
    if !confirm {
        println!();
        println!(
            "   Skipping install. Re-run {} after installing manually.",
            style("tebis setup").bold(),
        );
        return Ok(EnsureOutcome::UserDeclined);
    }

    println!();
    println!("   → {}", style(&cmd_str).dim());
    println!();
    let argv = install_argv(pm, PKG);
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

fn print_no_pm_guidance() {
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
}
