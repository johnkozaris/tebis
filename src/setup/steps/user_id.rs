//! Step 2 — numeric Telegram user id prompt.

use anyhow::{Context, Result};
use console::style;
use dialoguer::Input;
use dialoguer::theme::ColorfulTheme;

use super::super::ui;

pub(in crate::setup) fn step_user_id(
    theme: &ColorfulTheme,
    existing: Option<i64>,
) -> Result<i64> {
    ui::step_header(2, "Lock the bot to your user id");
    println!(
        "Telegram bots are {} — anyone who discovers yours can DM it.",
        style("public by default").bold(),
    );
    println!("tebis only reacts to messages from your numeric user id; every");
    println!("other sender is silently dropped. This is the primary lockdown.");
    println!();
    println!(
        "DM {} (blue checkmark), tap {}. It replies with a line like:",
        style("@userinfobot").cyan().bold(),
        style("Start").bold(),
    );
    println!();
    println!("    {}", style("Id: 12345678").dim().italic());
    println!();

    let mut prompt = Input::<i64>::with_theme(theme)
        .with_prompt("Your numeric Telegram id")
        .validate_with(|n: &i64| -> std::result::Result<(), &'static str> {
            if *n > 0 {
                Ok(())
            } else {
                Err("must be a positive integer")
            }
        });
    if let Some(n) = existing.filter(|&n| n > 0) {
        prompt = prompt.default(n);
    }
    prompt.interact_text().context("prompt: user id")
}
