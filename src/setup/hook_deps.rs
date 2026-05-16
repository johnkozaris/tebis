//! Hook script runtime dependencies — `jq` + `nc` on Unix.
//!
//! The embedded `.sh` hooks shell out to `jq` (parse Claude/Copilot
//! hook payloads) and `nc` (forward to the tebis notify UDS). Missing
//! either means the hook fires silently and no event reaches the
//! bridge. Setup probes for both up front and offers to install via
//! the user's package manager.
//!
//! Windows is a no-op: the `.ps1` hooks use native PowerShell
//! features (`ConvertFrom-Json`, `NamedPipeClientStream`) and need no
//! external runtime deps. Setup callers still invoke
//! `ensure_or_offer_install` unconditionally; this module decides
//! per-OS whether there's work to do.

#[cfg(unix)]
use anyhow::{Context, Result};
#[cfg(not(unix))]
use anyhow::Result;
#[cfg(unix)]
use console::style;
#[cfg(unix)]
use dialoguer::Confirm;
use dialoguer::theme::ColorfulTheme;

#[cfg(unix)]
use super::installer::{
    EnsureOutcome, PackageManager, detect_package_manager, install_argv, install_cmd_display,
};

/// One hook-script dependency: a binary on `$PATH` and the per-PM
/// package name(s) we'd install if it's missing. `jq` is uniform; `nc`
/// is the wart — every distro family ships its preferred netcat under
/// a different package name.
#[cfg(unix)]
struct HookDep {
    /// `$PATH` lookup name.
    bin: &'static str,
    /// Per-PM package name. Falls through to `bin` if not listed.
    pkg_for: fn(PackageManager) -> &'static str,
}

#[cfg(unix)]
const HOOK_DEPS: &[HookDep] = &[
    HookDep {
        bin: "jq",
        pkg_for: |_| "jq",
    },
    HookDep {
        bin: "nc",
        pkg_for: |pm| match pm {
            // BSD netcat on macOS / MacPorts.
            PackageManager::Brew | PackageManager::MacPorts => "netcat",
            // Modern Debian/Ubuntu ships the OpenBSD variant under this name.
            PackageManager::Apt => "netcat-openbsd",
            // Fedora/RHEL 8+ ships nmap-ncat as the default `nc`.
            PackageManager::Dnf => "nmap-ncat",
            // Arch ships OpenBSD netcat from `community`.
            PackageManager::Pacman => "openbsd-netcat",
            // openSUSE + Alpine both use the OpenBSD package name.
            PackageManager::Zypper | PackageManager::Apk => "netcat-openbsd",
        },
    },
];

/// Probe each known hook dep. If any are missing, offer to install
/// them via the detected package manager. Windows: no-op.
pub fn ensure_or_offer_install(theme: &ColorfulTheme) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = theme;
        return Ok(());
    }
    #[cfg(unix)]
    unix::ensure(theme)
}

#[cfg(unix)]
mod unix {
    use super::{
        Confirm, ColorfulTheme, Context, EnsureOutcome, HOOK_DEPS, Result, style,
    };
    use super::{detect_package_manager, install_argv, install_cmd_display};
    use std::process::Command;

    pub fn ensure(theme: &ColorfulTheme) -> Result<()> {
        let missing: Vec<&'static str> = HOOK_DEPS
            .iter()
            .filter(|d| crate::audio::espeak::which_in_path(d.bin).is_none())
            .map(|d| d.bin)
            .collect();

        if missing.is_empty() {
            return Ok(());
        }

        let Some(pm) = detect_package_manager() else {
            print_no_pm_guidance(&missing);
            return Ok(());
        };

        let pkgs: Vec<&'static str> = HOOK_DEPS
            .iter()
            .filter(|d| missing.contains(&d.bin))
            .map(|d| (d.pkg_for)(pm))
            .collect();
        let display_list = pkgs.join(" ");
        let cmd_str = install_cmd_display(pm, &display_list);

        println!();
        println!(
            "{} Hook scripts need {} (not installed).",
            style("ℹ").cyan(),
            style(human_list(&missing)).bold(),
        );
        println!("   We'll run:");
        println!();
        println!("     {}", style(&cmd_str).bold());
        println!();

        let confirm = Confirm::with_theme(theme)
            .with_prompt("Install now?")
            .default(true)
            .interact()
            .context("prompt: install hook deps")?;
        if !confirm {
            println!();
            println!(
                "   Skipping — hooks will silently no-op until you install \
                 {} manually.",
                style(human_list(&missing)).bold(),
            );
            // Don't bail — wizard continues; user may install later.
            let _ = EnsureOutcome::UserDeclined;
            return Ok(());
        }

        println!();
        println!("   → {}", style(&cmd_str).dim());
        println!();
        let argv = install_argv(pm, &display_list);
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
            println!("   Re-run `tebis doctor` after installing manually.");
            return Ok(());
        }

        // Re-probe; PM success doesn't guarantee PATH (fresh shell may
        // be needed for some PMs, e.g. brew bottling weirdness).
        let still_missing: Vec<&'static str> = HOOK_DEPS
            .iter()
            .filter(|d| missing.contains(&d.bin))
            .filter(|d| crate::audio::espeak::which_in_path(d.bin).is_none())
            .map(|d| d.bin)
            .collect();
        if still_missing.is_empty() {
            println!(
                "  {} {} installed.",
                style("✓").green(),
                style(human_list(&missing)).bold(),
            );
        } else {
            println!();
            println!(
                "  {} install completed but {} still not on PATH.",
                style("⚠").yellow(),
                style(human_list(&still_missing)).bold(),
            );
            println!("   Open a new shell and run `tebis doctor`.");
        }
        Ok(())
    }

    fn human_list(items: &[&str]) -> String {
        match items {
            [] => String::new(),
            [a] => (*a).to_string(),
            [a, b] => format!("{a} + {b}"),
            _ => items.join(", "),
        }
    }

    fn print_no_pm_guidance(missing: &[&str]) {
        println!();
        println!(
            "{} Hook scripts need {} but we couldn't detect a supported",
            style("ℹ").cyan(),
            style(human_list(missing)).bold(),
        );
        println!("   package manager on this system.");
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
            println!();
            println!(
                "   Then: {} (and {} on older macOS)",
                style("brew install jq").bold(),
                style("brew install netcat").bold(),
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            println!("   Install manually then re-run `tebis doctor`:");
            for &m in missing {
                println!("     {}", style(format!("• {m}")).dim());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity test: Windows path compiles and is a no-op.
    #[test]
    fn ensure_compiles_on_all_platforms() {
        let theme = ColorfulTheme::default();
        // Don't actually call ensure_or_offer_install — on Unix it
        // probes the live PATH and may prompt. The cfg-gating + this
        // signature smoke-test is enough for cross-platform coverage.
        let _: fn(&ColorfulTheme) -> Result<()> = ensure_or_offer_install;
        let _ = theme;
    }

    #[cfg(unix)]
    #[test]
    fn nc_package_name_per_pm_uniform_jq() {
        for pm in [
            PackageManager::Brew,
            PackageManager::MacPorts,
            PackageManager::Apt,
            PackageManager::Dnf,
            PackageManager::Pacman,
            PackageManager::Zypper,
            PackageManager::Apk,
        ] {
            assert_eq!((HOOK_DEPS[0].pkg_for)(pm), "jq", "jq drift for {pm:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn nc_package_name_varies_by_pm() {
        // Spot-check that we do NOT just return "nc" for every PM —
        // each distro family has a different canonical name.
        let names: Vec<&'static str> = [
            PackageManager::Brew,
            PackageManager::Apt,
            PackageManager::Dnf,
            PackageManager::Pacman,
        ]
        .into_iter()
        .map(HOOK_DEPS[1].pkg_for)
        .collect();
        // At least one is the BSD variant, at least one is the GNU/nmap variant.
        assert!(names.contains(&"netcat"));
        assert!(names.contains(&"netcat-openbsd"));
        assert!(names.contains(&"nmap-ncat"));
        assert!(names.contains(&"openbsd-netcat"));
        // None should be the bare bin name.
        assert!(!names.contains(&"nc"));
    }
}
