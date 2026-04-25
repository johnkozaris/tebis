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
            println!("agent hooks to install. Tebis will still watch terminal output for replies.");
            println!();
            return Ok(HooksChoice::Off);
        }
        Some(kind) => {
            println!(
                "Detected {}. Agent hooks usually send replies faster",
                style(kind.display()).bold(),
            );
            println!("than waiting for terminal output to settle. Hooks");
            println!(
                "are idempotent and reversible. Remove any time with {}.",
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
