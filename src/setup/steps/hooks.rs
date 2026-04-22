//! Step 5 — agent hooks mode.

use anyhow::{Context, Result};
use console::style;
use dialoguer::Confirm;
use dialoguer::theme::ColorfulTheme;

use super::super::{Autostart, HooksChoice, ui};
use crate::agent_hooks::AgentKind;

pub(in crate::setup) fn step_hooks_mode(
    theme: &ColorfulTheme,
    autostart: Option<&Autostart>,
    existing: Option<HooksChoice>,
) -> Result<HooksChoice> {
    ui::step_header(5, "Agent hooks (optional)");
    let detected = autostart.and_then(|a| AgentKind::detect(&a.command));
    match detected {
        None => {
            println!("Your autostart command isn't a recognized agent, so there are no");
            println!("native hooks to install. Pane-settle auto-reply covers generic TUIs.");
            println!();
            return Ok(HooksChoice::Off);
        }
        Some(kind) => {
            println!(
                "Detected {}. Native hooks give precise, low-latency replies",
                style(kind.display()).bold(),
            );
            println!(
                "via the agent's {} event instead of polling the pane. Hooks",
                style("Stop").bold(),
            );
            println!(
                "are idempotent and reversible — remove any time with {}.",
                style("tebis hooks uninstall").bold(),
            );
            println!();
        }
    }
    let default = !matches!(existing, Some(HooksChoice::Off));
    let enable = Confirm::with_theme(theme)
        .with_prompt("Auto-install hooks into the autostart project?")
        .default(default)
        .interact()
        .context("prompt: hooks-mode")?;
    Ok(if enable {
        HooksChoice::Auto
    } else {
        HooksChoice::Off
    })
}
