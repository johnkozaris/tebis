//! Step 4 — default agent autostart (session + dir + command).

use std::path::Path;

use anyhow::{Context, Result};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input};

use super::super::{Autostart, normalize_dir, ui};
use crate::platform::multiplexer::is_valid_session_name;

pub(in crate::setup) fn step_autostart(
    theme: &ColorfulTheme,
    allowlist: &[String],
    existing: Option<&Autostart>,
) -> Result<Option<Autostart>> {
    ui::step_header(4, "Default agent (optional)");
    println!("Pipe every plain-text message to one tmux session by default —");
    println!("ideal for driving an AI agent (Claude Code, etc.) from your");
    println!("phone. tebis spawns the session on first message, then routes");
    println!("anything that isn't a {} to it.", style("/command").bold());
    println!();

    if !Confirm::with_theme(theme)
        .with_prompt("Set a default agent?")
        .default(existing.is_some())
        .interact()
        .context("prompt: default-agent yes/no")?
    {
        return Ok(None);
    }

    let default_session = existing.map_or_else(
        || {
            allowlist
                .first()
                .cloned()
                .unwrap_or_else(|| "claude-session".to_string())
        },
        |e| e.session.clone(),
    );
    let allowed_set: Vec<String> = allowlist.to_vec();
    let permissive = allowed_set.is_empty();
    let session: String = Input::<String>::with_theme(theme)
        .with_prompt("Session name")
        .default(default_session)
        .validate_with(move |s: &String| -> std::result::Result<(), String> {
            if !is_valid_session_name(s) {
                return Err("must match [A-Za-z0-9._-], 1..=64 chars".into());
            }
            if !permissive && !allowed_set.iter().any(|a| a == s) {
                return Err(format!("'{s}' must be one of: {}", allowed_set.join(", ")));
            }
            Ok(())
        })
        .interact_text()
        .context("prompt: default-agent session")?;

    // No smart default for dir — `$HOME/Repos` would leak the operator's username.
    println!();
    println!(
        "    {} an absolute path like {} or {}",
        style("e.g.").dim(),
        style("/path/to/your/project").dim().italic(),
        style("~/your/project").dim().italic(),
    );
    println!();
    let mut dir_prompt = Input::<String>::with_theme(theme)
        .with_prompt("Working directory")
        .validate_with(|s: &String| -> std::result::Result<(), String> {
            let expanded = normalize_dir(s);
            if expanded.is_empty() {
                return Err("must not be empty".into());
            }
            if Path::new(&expanded).is_dir() {
                Ok(())
            } else {
                Err(format!("'{expanded}' is not an existing directory"))
            }
        });
    if let Some(e) = existing {
        dir_prompt = dir_prompt.default(e.dir.clone());
    }
    let raw_dir: String = dir_prompt
        .interact_text()
        .context("prompt: default-agent dir")?;
    let dir = normalize_dir(&raw_dir);

    let default_command = existing.map_or_else(|| "claude".to_string(), |e| e.command.clone());
    let command: String = Input::<String>::with_theme(theme)
        .with_prompt("Agent command")
        .default(default_command)
        .validate_with(|s: &String| -> std::result::Result<(), &'static str> {
            if s.chars().any(char::is_control) {
                Err("must not contain control characters")
            } else if s.trim().is_empty() {
                Err("must not be empty")
            } else {
                Ok(())
            }
        })
        .interact_text()
        .context("prompt: default-agent command")?;

    Ok(Some(Autostart {
        session,
        dir,
        command,
    }))
}
