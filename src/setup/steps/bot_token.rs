//! Step 1 — `BotFather` token prompt.

use anyhow::{Context, Result};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input};

use super::super::ui;

pub(in crate::setup) fn step_bot_token(
    theme: &ColorfulTheme,
    existing: Option<&str>,
) -> Result<String> {
    ui::step_header(1, "Create a Telegram bot");

    // Mask the current token; don't leak it as a prompt default.
    if let Some(token) = existing
        && validate(token).is_ok()
    {
        println!(
            "Current: {}   {}",
            style(ui::mask_token(token)).bold(),
            style("(masked)").dim(),
        );
        println!();
        if Confirm::with_theme(theme)
            .with_prompt("Keep the current bot token?")
            .default(true)
            .interact()
            .context("prompt: keep token")?
        {
            return Ok(token.to_string());
        }
        println!();
    }

    println!(
        "Open {} in Telegram — the one with the {} (official).",
        style("@BotFather").cyan().bold(),
        style("blue checkmark").blue().bold(),
    );
    println!(
        "Tap {}, send {}, and answer two prompts:",
        style("Start").bold(),
        style("/newbot").bold(),
    );
    println!();
    ui::kv_row("name", "any display name", "My Bridge");
    ui::kv_row("username", "ends in \"bot\"", "my_bridge_bot");
    println!();
    println!("BotFather replies with a single token. Paste the whole line below, e.g.");
    println!();
    println!(
        "    {}",
        style("123456789:ABCdefGHIjklMNOpqrSTUvwxYZ-1234567890_abcd")
            .dim()
            .italic(),
    );
    println!();

    Input::<String>::with_theme(theme)
        .with_prompt("Bot token")
        .validate_with(|s: &String| -> std::result::Result<(), &'static str> { validate(s) })
        .interact_text()
        .context("prompt: bot token")
}

/// Cheap pre-check for common paste errors — `getMe` has final say.
fn validate(s: &str) -> std::result::Result<(), &'static str> {
    let Some((head, tail)) = s.split_once(':') else {
        return Err("expected format: <digits>:<chars>");
    };
    if head.is_empty() || !head.chars().all(|c| c.is_ascii_digit()) {
        return Err("bot id (part before ':') must be digits only");
    }
    if tail.len() < 30 {
        return Err("token chars too short — did you paste it whole?");
    }
    if !tail
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("unexpected characters — paste exactly what BotFather sent");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn accepts_real_shape() {
        assert!(validate("123456789:ABCdefGHIjklMNOpqrSTUvwxYZ-1234567890_abcd").is_ok());
    }

    #[test]
    fn rejects_bad_shapes() {
        assert!(validate("").is_err());
        assert!(validate("no-colon").is_err());
        assert!(validate("abc:short").is_err());
        assert!(validate(":missinghead-chars-for-token").is_err());
        assert!(validate("123:withspaces too short").is_err());
    }
}
