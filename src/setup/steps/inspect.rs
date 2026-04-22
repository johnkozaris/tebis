//! Step 6 — local inspect dashboard port.

use anyhow::{Context, Result};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input};

use super::super::ui;

pub(in crate::setup) fn step_inspect_port(
    theme: &ColorfulTheme,
    existing: Option<u16>,
) -> Result<Option<u16>> {
    ui::step_header(6, "Control dashboard (optional)");
    println!("Local HTML page with live activity, kill / restart buttons, and");
    println!("in-place settings editing. Loopback only, no authentication.");
    println!();

    let default_enable = existing.is_some();
    if !Confirm::with_theme(theme)
        .with_prompt("Enable dashboard?")
        .default(default_enable)
        .interact()
        .context("prompt: dashboard yes/no")?
    {
        return Ok(None);
    }

    let port: u16 = Input::<u16>::with_theme(theme)
        .with_prompt("Port")
        .default(existing.unwrap_or(51_624_u16))
        .validate_with(|n: &u16| -> std::result::Result<(), &'static str> {
            if *n >= 1024 {
                Ok(())
            } else {
                Err("pick a port ≥ 1024 (non-privileged)")
            }
        })
        .interact_text()
        .context("prompt: dashboard port")?;

    Ok(Some(port))
}
