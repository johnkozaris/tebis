//! Setup-time multiplexer prerequisite check.

use anyhow::Result;
#[cfg(windows)]
use anyhow::{Context as _, bail};
#[cfg(windows)]
use console::style;
use dialoguer::theme::ColorfulTheme;

pub(super) fn ensure_or_offer_install(theme: &ColorfulTheme) -> Result<()> {
    #[cfg(not(windows))]
    let _ = theme;

    if crate::platform::multiplexer::binary_on_path() {
        return Ok(());
    }

    let bin = crate::platform::multiplexer::BINARY;
    super::ui::note_warn(&format!(
        "`{bin}` is not on PATH yet. Setup can write config, but runtime needs it."
    ));

    #[cfg(windows)]
    offer_windows_psmux_install(theme)?;

    #[cfg(not(windows))]
    print_unix_guidance();

    println!();
    Ok(())
}

#[cfg(not(windows))]
fn print_unix_guidance() {
    println!("   Install tmux 3.x with your OS package manager, then re-run or start tebis.");
}

#[cfg(windows)]
fn offer_windows_psmux_install(theme: &ColorfulTheme) -> Result<()> {
    use dialoguer::{Confirm, Select};

    let methods = available_windows_installers();
    if methods.is_empty() {
        print_windows_manual_guidance();
        return abort_psmux_required();
    }

    println!("   psmux can be installed automatically with one of the tools on PATH.");
    println!("   psmux also ships `tmux.exe` and `pmux.exe` aliases.");
    println!();

    let install = Confirm::with_theme(theme)
        .with_prompt("Install psmux now?")
        .default(true)
        .interact()
        .context("prompt: install psmux")?;
    if !install {
        print_windows_manual_guidance();
        return abort_psmux_required();
    }

    let method = if methods.len() == 1 {
        methods[0]
    } else {
        let labels: Vec<String> = methods.iter().map(|m| m.label().to_string()).collect();
        let idx = Select::with_theme(theme)
            .with_prompt("Install method")
            .items(labels.as_slice())
            .default(0)
            .interact()
            .context("prompt: psmux install method")?;
        methods[idx]
    };

    println!();
    for step in method.install_steps() {
        println!("   → {}", style(step.display()).dim());
        let status = std::process::Command::new(step.program)
            .args(step.args)
            .status()
            .with_context(|| format!("spawning {}", step.program))?;
        if !status.success() {
            println!();
            println!(
                "  {} {} exited with status {:?} — psmux install did not complete.",
                style("✗").red(),
                step.program,
                status.code(),
            );
            print_windows_manual_guidance();
            return abort_psmux_required();
        }
    }

    if crate::platform::multiplexer::binary_on_path() {
        println!("  {} psmux is now on PATH.", style("✓").green());
    } else {
        println!();
        println!(
            "  {} Install command finished, but this terminal still cannot see `psmux`.",
            style("⚠").yellow(),
        );
        println!(
            "   Open a new terminal and re-run {}.",
            style("tebis setup").bold()
        );
        return abort_psmux_required();
    }

    Ok(())
}

#[cfg(windows)]
fn abort_psmux_required() -> Result<()> {
    println!();
    println!(
        "   {} Setup stopped before writing config. Install psmux, then re-run {}.",
        style("→").dim(),
        style("tebis setup").bold(),
    );
    bail!("psmux is required on Windows")
}

#[cfg(windows)]
fn print_windows_manual_guidance() {
    println!("   Install psmux, then open a new terminal:");
    println!("     scoop bucket add psmux https://github.com/psmux/scoop-psmux");
    println!("     scoop install psmux");
    println!(
        "   Other options: winget install marlocarlo.psmux, choco install psmux, cargo install psmux"
    );
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsPsmuxInstaller {
    Scoop,
    Winget,
    Chocolatey,
    Cargo,
}

#[cfg(windows)]
impl WindowsPsmuxInstaller {
    const fn label(self) -> &'static str {
        match self {
            Self::Scoop => "Scoop — per-user install, preferred when available",
            Self::Winget => "WinGet — Windows package manager",
            Self::Chocolatey => "Chocolatey — choco install psmux",
            Self::Cargo => "Cargo — cargo install psmux",
        }
    }

    fn install_steps(self) -> Vec<InstallStep> {
        match self {
            Self::Scoop => vec![
                InstallStep::new(
                    "scoop",
                    &[
                        "bucket",
                        "add",
                        "psmux",
                        "https://github.com/psmux/scoop-psmux",
                    ],
                ),
                InstallStep::new("scoop", &["install", "psmux"]),
            ],
            Self::Winget => vec![InstallStep::new(
                "winget",
                &[
                    "install",
                    "--id",
                    "marlocarlo.psmux",
                    "-e",
                    "--accept-package-agreements",
                    "--accept-source-agreements",
                ],
            )],
            Self::Chocolatey => vec![InstallStep::new("choco", &["install", "psmux", "-y"])],
            Self::Cargo => vec![InstallStep::new("cargo", &["install", "psmux"])],
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallStep {
    program: &'static str,
    args: Vec<&'static str>,
}

#[cfg(windows)]
impl InstallStep {
    fn new(program: &'static str, args: &[&'static str]) -> Self {
        Self {
            program,
            args: args.to_vec(),
        }
    }

    fn display(&self) -> String {
        std::iter::once(self.program)
            .chain(self.args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(windows)]
fn available_windows_installers() -> Vec<WindowsPsmuxInstaller> {
    [
        (WindowsPsmuxInstaller::Scoop, "scoop"),
        (WindowsPsmuxInstaller::Winget, "winget"),
        (WindowsPsmuxInstaller::Chocolatey, "choco"),
        (WindowsPsmuxInstaller::Cargo, "cargo"),
    ]
    .into_iter()
    .filter_map(|(method, binary)| command_on_path(binary).then_some(method))
    .collect()
}

#[cfg(windows)]
fn command_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        for candidate in executable_candidates(name) {
            if dir.join(candidate).is_file() {
                return true;
            }
        }
    }
    false
}

#[cfg(windows)]
fn executable_candidates(name: &str) -> Vec<String> {
    if std::path::Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let mut out = vec![name.to_string()];
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    out.extend(
        pathext
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(|ext| format!("{name}{ext}")),
    );
    out
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn winget_command_accepts_agreements() {
        let steps = WindowsPsmuxInstaller::Winget.install_steps();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].program, "winget");
        assert!(steps[0].args.contains(&"marlocarlo.psmux"));
        assert!(steps[0].args.contains(&"--accept-package-agreements"));
        assert!(steps[0].args.contains(&"--accept-source-agreements"));
    }

    #[test]
    fn scoop_adds_bucket_before_installing() {
        let steps = WindowsPsmuxInstaller::Scoop.install_steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(
            steps[0].display(),
            "scoop bucket add psmux https://github.com/psmux/scoop-psmux"
        );
        assert_eq!(steps[1].display(), "scoop install psmux");
    }

    #[test]
    fn pathext_candidates_include_exe() {
        let candidates = executable_candidates("winget");
        assert!(
            candidates
                .iter()
                .any(|c| c.eq_ignore_ascii_case("winget.exe"))
        );
    }
}
