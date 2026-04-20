//! Interactive wizard steps + input validators.

use std::path::Path;

use anyhow::{Context, Result};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input};

use super::{Autostart, HooksChoice, normalize_dir, ui};
use crate::agent_hooks::AgentKind;
use crate::tmux::is_valid_session_name;

pub(super) fn step_bot_token(theme: &ColorfulTheme, existing: Option<&str>) -> Result<String> {
    ui::step_header(1, "Create a Telegram bot");

    // Rerun path: offer to keep an existing valid token via a masked Y/N
    // confirm instead of printing it as a prompt default.
    if let Some(token) = existing
        && validate_bot_token(token).is_ok()
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
        .validate_with(|s: &String| -> std::result::Result<(), &'static str> {
            validate_bot_token(s)
        })
        .interact_text()
        .context("prompt: bot token")
}

pub(super) fn step_user_id(theme: &ColorfulTheme, existing: Option<i64>) -> Result<i64> {
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

pub(super) fn step_session_allowlist(
    theme: &ColorfulTheme,
    existing: Option<&[String]>,
) -> Result<Vec<String>> {
    ui::step_header(3, "Session allowlist (optional)");
    println!("By default tebis accepts any valid tmux session name. Optionally");
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
        return Ok(Vec::new()); // permissive mode
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
        .with_prompt("Allowed tmux sessions")
        .default(default_list)
        .validate_with(|s: &String| -> std::result::Result<(), &'static str> {
            validate_session_list(s)
        })
        .interact_text()
        .context("prompt: sessions")?;

    Ok(raw
        .split(',')
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect())
}

pub(super) fn step_autostart(
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

    // Working directory — no smart default. Showing `$HOME/Repos` would
    // surface the operator's username to anyone watching the wizard
    // (screenshots, screenshares), and most users don't put projects under
    // `~/Repos` anyway. Re-runs still pre-fill from the existing env file.
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

pub(super) fn step_hooks_mode(
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
    // Default to Auto when the user hasn't explicitly opted out previously.
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

pub(super) fn step_inspect_port(
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

// ---------- validators ----------

/// Accept tokens shaped `<digits>:<30+ [A-Za-z0-9_-]>`. Telegram makes the
/// final call; `getMe` surfaces a bad token on first launch.
fn validate_bot_token(s: &str) -> std::result::Result<(), &'static str> {
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

fn validate_session_list(s: &str) -> std::result::Result<(), &'static str> {
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
    use super::*;

    #[test]
    fn bot_token_validator_accepts_real_shape() {
        assert!(validate_bot_token("123456789:ABCdefGHIjklMNOpqrSTUvwxYZ-1234567890_abcd").is_ok());
    }

    #[test]
    fn bot_token_validator_rejects_bad_shapes() {
        assert!(validate_bot_token("").is_err());
        assert!(validate_bot_token("no-colon").is_err());
        assert!(validate_bot_token("abc:short").is_err());
        assert!(validate_bot_token(":missinghead-chars-for-token").is_err());
        assert!(validate_bot_token("123:withspaces too short").is_err());
    }

    #[test]
    fn session_list_validator_accepts_valid() {
        assert!(validate_session_list("claude-code").is_ok());
        assert!(validate_session_list("claude-code,shell,my.session_1").is_ok());
    }

    #[test]
    fn session_list_validator_rejects_bad() {
        assert!(validate_session_list("").is_err());
        assert!(validate_session_list(",").is_err());
        assert!(validate_session_list("with space").is_err());
        assert!(validate_session_list("too;evil").is_err());
        assert!(validate_session_list(&"a".repeat(65)).is_err());
    }
}
