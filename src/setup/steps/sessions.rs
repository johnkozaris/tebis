//! Step 3 — multiplexer session allowlist (optional).

use anyhow::{Context, Result};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input};

use super::super::ui;

pub(in crate::setup) fn step_session_allowlist(
    theme: &ColorfulTheme,
    existing: Option<&[String]>,
) -> Result<Vec<String>> {
    ui::step_header(3, "Session allowlist (optional)");
    println!(
        "By default tebis accepts any valid {} session name. Optionally",
        crate::platform::multiplexer::BINARY
    );
    println!("restrict it to a fixed list for defense-in-depth on top of the");
    println!(
        "user-id filter. Names must match {} either way.",
        style("[A-Za-z0-9._-]{1,64}").bold(),
    );
    println!();

    let default_restrict = existing.is_some_and(|v| !v.is_empty());
    if !Confirm::with_theme(theme)
        .with_prompt("Restrict to specific session names?")
        .default(default_restrict)
        .interact()
        .context("prompt: restrict y/n")?
    {
        return Ok(Vec::new());
    }

    println!();
    println!("Enter names separated by {}, e.g.", style("commas").bold());
    println!();
    println!("    {}", style("claude-session, shell").dim().italic());
    println!();

    let default_list = existing
        .filter(|v| !v.is_empty())
        .map_or_else(|| "claude-session".to_string(), |v| v.join(","));
    let raw: String = Input::<String>::with_theme(theme)
        .with_prompt(format!(
            "Allowed {} sessions",
            crate::platform::multiplexer::BINARY
        ))
        .default(default_list)
        .validate_with(|s: &String| -> std::result::Result<(), &'static str> { validate(s) })
        .interact_text()
        .context("prompt: sessions")?;

    Ok(raw
        .split(',')
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect())
}

fn validate(s: &str) -> std::result::Result<(), &'static str> {
    let names: Vec<&str> = s
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return Err("at least one session name is required");
    }
    for name in &names {
        if name.len() > 64 {
            return Err("session names max 64 chars");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err("session names must match [A-Za-z0-9._-]");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn accepts_valid() {
        assert!(validate("claude-code").is_ok());
        assert!(validate("claude-code,shell,my.session_1").is_ok());
    }

    #[test]
    fn rejects_bad() {
        assert!(validate("").is_err());
        assert!(validate(",").is_err());
        assert!(validate("with space").is_err());
        assert!(validate("too;evil").is_err());
        assert!(validate(&"a".repeat(65)).is_err());
    }
}
