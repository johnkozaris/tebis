//! `tebis setup` — interactive first-run wizard.
//!
//! Walks the user through creating a bot on `@BotFather`, finding their
//! numeric Telegram id via `@userinfobot`, deciding whether to restrict
//! the session allowlist, and optionally enabling autostart + the inspect
//! dashboard. Writes a fresh env file at `~/.config/tebis/env` (mode 0600).
//!
//! Presentation idiom: **treat the terminal as a document, not a
//! dashboard**. Body prose at column 0, examples indented 4 spaces, step
//! dividers as horizontal rules. No frames, pills, or left-bar decoration
//! — they fight the eye for attention the prose should be getting.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use console::{Term, measure_text_width, style};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input};

use crate::tmux::is_valid_session_name;

// ---------- entry ----------

pub fn run() -> Result<()> {
    let theme = ColorfulTheme::default();
    print_welcome();

    let env_path = env_file_path()?;
    let discovered = discover(&env_path);
    if env_path.exists() {
        note_info(&format!(
            "Loaded current config from {}.",
            style(env_path.display()).bold(),
        ));
        println!(
            "   Each prompt is pre-filled — press {} to keep.",
            style("Enter").bold(),
        );
        note_warn(&format!(
            "Will be backed up to {} before saving.",
            style(backup_path(&env_path).display()).bold(),
        ));
        println!();
    }

    let token = step_bot_token(&theme, discovered.bot_token.as_deref())?;
    let user_id = step_user_id(&theme, discovered.allowed_user)?;
    let sessions = step_session_allowlist(&theme, discovered.allowed_sessions.as_deref())?;
    let autostart = step_autostart(&theme, &sessions, discovered.autostart.as_ref())?;
    let inspect_port = step_inspect_port(&theme, discovered.inspect_port)?;

    print_summary(&token, user_id, &sessions, autostart.as_ref(), inspect_port);
    if !Confirm::with_theme(&theme)
        .with_prompt("Save this config?")
        .default(true)
        .interact()
        .context("prompt: confirm save")?
    {
        println!();
        println!(
            "{} Nothing written. Re-run {} to try again.",
            style("Aborted.").red().bold(),
            style("tebis setup").bold(),
        );
        return Ok(());
    }

    let content = build_env_file(
        &token,
        user_id,
        &sessions,
        autostart.as_ref(),
        inspect_port,
        &env_path,
    );

    if env_path.exists() {
        let bak = backup_path(&env_path);
        fs::copy(&env_path, &bak)
            .with_context(|| format!("backing up existing env to {}", bak.display()))?;
    }
    write_env_file(&env_path, &content)?;

    print_done(&env_path, inspect_port);
    Ok(())
}

/// `$HOME/.config/tebis/env`.
pub fn env_file_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(".config/tebis/env"))
}

// ---------- steps ----------

fn step_bot_token(theme: &ColorfulTheme, existing: Option<&str>) -> Result<String> {
    step_header(1, "Create a Telegram bot");

    // Rerun path: if a token's already on file, offer to keep it. The
    // token is the one secret in the file, so we don't pre-fill it via
    // `Input.default` (that would print it). A Y/N confirm with a masked
    // display is the safest UX.
    if let Some(token) = existing
        && validate_bot_token(token).is_ok()
    {
        println!(
            "Current: {}   {}",
            style(mask_token(token)).bold(),
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
    kv_row("name", "any display name", "My Bridge");
    kv_row("username", "ends in \"bot\"", "my_bridge_bot");
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

fn step_user_id(theme: &ColorfulTheme, existing: Option<i64>) -> Result<i64> {
    step_header(2, "Lock the bot to your user id");
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

fn step_session_allowlist(
    theme: &ColorfulTheme,
    existing: Option<&[String]>,
) -> Result<Vec<String>> {
    step_header(3, "Session allowlist (optional)");
    println!("By default tebis accepts any valid tmux session name. Optionally");
    println!("restrict it to a fixed list for defense-in-depth on top of the");
    println!(
        "user-id filter. Names must match {} either way.",
        style("[A-Za-z0-9._-]{1,64}").bold(),
    );
    println!();

    // Default y/n reflects the user's current state: if they already had
    // a list, the obvious Enter-to-keep path is "yes, restrict".
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
    println!("    {}", style("claude-code, shell").dim().italic());
    println!();

    let default_list = existing
        .filter(|v| !v.is_empty())
        .map_or_else(|| "claude-code,shell".to_string(), |v| v.join(","));
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

struct Autostart {
    session: String,
    dir: String,
    command: String,
}

fn step_autostart(
    theme: &ColorfulTheme,
    allowlist: &[String],
    existing: Option<&Autostart>,
) -> Result<Option<Autostart>> {
    step_header(4, "Default agent (optional)");
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
                .unwrap_or_else(|| "claude-code".to_string())
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

    let default_dir = existing.map_or_else(
        || env::var("HOME").map_or_else(|_| "/tmp".to_string(), |h| format!("{h}/Repos")),
        |e| e.dir.clone(),
    );
    // Validator expands `~/…` before checking existence so `~/Repos/Foo`
    // isn't rejected as "not a directory" when it obviously exists. The
    // post-prompt `normalize_dir` call produces the expanded path that
    // gets persisted.
    let raw_dir: String = Input::<String>::with_theme(theme)
        .with_prompt("Working directory")
        .default(default_dir)
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
        })
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

fn step_inspect_port(theme: &ColorfulTheme, existing: Option<u16>) -> Result<Option<u16>> {
    step_header(5, "Control dashboard (optional)");
    println!("Local HTML page with live activity, kill / restart buttons, and");
    println!("in-place settings editing. Loopback only, no authentication.");
    println!();

    // Default y/n reflects current state: if a port was already set,
    // Enter-to-keep reads as "yes, enable".
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

// ---------- summary + done ----------

/// Show the collected values so the user can sanity-check before
/// committing to disk. Bot token is masked — everything else is
/// non-sensitive and worth seeing in full.
fn print_summary(
    token: &str,
    user_id: i64,
    sessions: &[String],
    autostart: Option<&Autostart>,
    inspect_port: Option<u16>,
) {
    divider_rule("Review");
    let masked_token = mask_token(token);
    let sessions_row = if sessions.is_empty() {
        style("(any — permissive)").dim().to_string()
    } else {
        sessions.join(", ")
    };
    let autostart_row = autostart.map_or_else(
        || style("(disabled)").dim().to_string(),
        |a| {
            format!(
                "{} · {} · {}",
                a.session,
                style(&a.dir).dim(),
                style(&a.command).italic(),
            )
        },
    );
    let dashboard_row = inspect_port.map_or_else(
        || style("(disabled)").dim().to_string(),
        |p| {
            format!(
                "http://127.0.0.1:{p}   {}",
                style("edit from the dashboard later").dim(),
            )
        },
    );
    row("Bot token", &masked_token);
    row("User id", &user_id.to_string());
    row("Sessions", &sessions_row);
    row("Agent", &autostart_row);
    row("Dashboard", &dashboard_row);
    println!();
}

/// `123456789:ABC…XYZ` — enough to recognize, not enough to reuse.
fn mask_token(token: &str) -> String {
    let Some((head, tail)) = token.split_once(':') else {
        return style("(invalid)").red().to_string();
    };
    let tail_chars: Vec<char> = tail.chars().collect();
    if tail_chars.len() <= 8 {
        return format!("{head}:{}", style("…").dim());
    }
    let prefix: String = tail_chars.iter().take(3).collect();
    let mut suffix_chars: Vec<char> = tail_chars.iter().rev().take(3).copied().collect();
    suffix_chars.reverse();
    let suffix: String = suffix_chars.into_iter().collect();
    format!("{head}:{prefix}{}{suffix}", style("…").dim())
}

fn print_done(env_path: &Path, inspect_port: Option<u16>) {
    divider_rule("Done");
    println!(
        "{}  Wrote {}.",
        style("✓").green().bold(),
        style(env_path.display()).bold(),
    );
    println!();
    println!("Start tebis:");
    println!("    {}", style("tebis").bold());
    if let Some(port) = inspect_port {
        println!();
        println!("Dashboard:");
        println!(
            "    {}",
            style(format!("http://127.0.0.1:{port}"))
                .cyan()
                .underlined(),
        );
    }
    println!();
    println!("Install as a launchd agent (macOS):");
    println!("    {}", style("./contrib/macos/install.sh").bold());
    println!();
    print_security_tips();
}

/// Non-blocking "after-setup" recommendation — close the remaining
/// Telegram-side gap (group-add) that the bridge can't gate on its own.
fn print_security_tips() {
    divider_rule("Hardening (recommended)");
    println!("tebis already drops messages from any user id other than yours.");
    println!();
    println!(
        "Next, disable {} in BotFather so nobody can add",
        style("Allow Groups").bold()
    );
    println!("your bot to a group where it would see messages:");
    println!();
    println!(
        "    {} → your bot → {} → {} → Turn off",
        style("/mybots").bold(),
        style("Bot Settings").bold(),
        style("Allow Groups?").bold(),
    );
    println!();
}

// ---------- validators ----------

/// Telegram bot tokens are shaped `<digits>:<base64ish>` where the tail is
/// 30+ characters of `[A-Za-z0-9_-]`. We accept anything close to that
/// shape — Telegram ultimately decides, and a real bad token fails
/// cleanly at `getMe` on first launch.
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

/// Mirror of `tmux::is_valid_session_name` applied to each item in a
/// comma-separated list.
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

// ---------- discovery ----------

/// Previously-saved values parsed out of the env file so the wizard can
/// pre-fill each prompt. Every field is `Option` so a missing/partial
/// file just falls through to fresh-install defaults.
#[derive(Default)]
struct Discovered {
    bot_token: Option<String>,
    allowed_user: Option<i64>,
    /// `None` = key unset (permissive); `Some(empty)` can't happen because
    /// we filter blanks. `Some(non-empty)` = previously restricted.
    allowed_sessions: Option<Vec<String>>,
    /// Only `Some` when all three autostart env vars are present — a
    /// partial triple would fail config load anyway.
    autostart: Option<Autostart>,
    inspect_port: Option<u16>,
}

/// Parse KEY=VALUE lines out of the env file. Comments (`#`) and blank
/// lines are skipped; unknown keys are ignored. Invalid integer values
/// (e.g. a corrupted `TELEGRAM_ALLOWED_USER`) silently fall back to `None`
/// so the wizard re-prompts — we don't want to refuse to launch over a
/// typo the user is here to fix.
fn discover(env_path: &Path) -> Discovered {
    let Ok(content) = fs::read_to_string(env_path) else {
        return Discovered::default();
    };
    let mut d = Discovered::default();
    let mut auto_session: Option<String> = None;
    let mut auto_dir: Option<String> = None;
    let mut auto_command: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "TELEGRAM_BOT_TOKEN" if !value.is_empty() => {
                d.bot_token = Some(value.to_string());
            }
            "TELEGRAM_ALLOWED_USER" => d.allowed_user = value.parse().ok().filter(|&n: &i64| n > 0),
            "TELEGRAM_ALLOWED_SESSIONS" => {
                let names: Vec<String> = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !names.is_empty() {
                    d.allowed_sessions = Some(names);
                }
            }
            "TELEGRAM_AUTOSTART_SESSION" if !value.is_empty() => {
                auto_session = Some(value.to_string());
            }
            "TELEGRAM_AUTOSTART_DIR" if !value.is_empty() => {
                auto_dir = Some(value.to_string());
            }
            "TELEGRAM_AUTOSTART_COMMAND" if !value.is_empty() => {
                auto_command = Some(value.to_string());
            }
            "INSPECT_PORT" => d.inspect_port = value.parse().ok().filter(|&n: &u16| n >= 1024),
            _ => {}
        }
    }
    if let (Some(session), Some(dir), Some(command)) = (auto_session, auto_dir, auto_command) {
        d.autostart = Some(Autostart {
            session,
            dir,
            command,
        });
    }
    d
}

// ---------- env file I/O ----------

fn build_env_file(
    token: &str,
    user_id: i64,
    sessions: &[String],
    autostart: Option<&Autostart>,
    inspect_port: Option<u16>,
    env_path: &Path,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("# Written by `tebis setup`. Safe to hand-edit.\n\n");
    let _ = writeln!(out, "TELEGRAM_BOT_TOKEN={token}");
    let _ = writeln!(out, "TELEGRAM_ALLOWED_USER={user_id}");
    // Empty list = permissive mode. Emit a commented-out example instead
    // of writing an empty value, so a future hand-edit has something to
    // uncomment without having to read the docs.
    if sessions.is_empty() {
        out.push_str(
            "# TELEGRAM_ALLOWED_SESSIONS is unset → any tmux session name is accepted.\n\
             # Uncomment and set a comma-separated list to restrict, e.g.:\n\
             # TELEGRAM_ALLOWED_SESSIONS=claude-code,shell\n",
        );
    } else {
        let _ = writeln!(out, "TELEGRAM_ALLOWED_SESSIONS={}", sessions.join(","));
    }

    if let Some(a) = autostart {
        out.push_str("\n# Autostart: first plain-text message spawns this.\n");
        let _ = writeln!(out, "TELEGRAM_AUTOSTART_SESSION={}", a.session);
        let _ = writeln!(out, "TELEGRAM_AUTOSTART_DIR={}", a.dir);
        let _ = writeln!(out, "TELEGRAM_AUTOSTART_COMMAND={}", a.command);
    }

    if let Some(port) = inspect_port {
        out.push_str("\n# Local HTML control dashboard (loopback only).\n");
        let _ = writeln!(out, "INSPECT_PORT={port}");
        out.push_str("# Enables the Settings-edit form on the dashboard.\n");
        let _ = writeln!(out, "BRIDGE_ENV_FILE={}", env_path.display());
    }

    out
}

fn write_env_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    // `path.with_extension` REPLACES the full extension; if the file has
    // no extension (our `env` case) it adds one. Use a sibling `.tmp`
    // instead to be predictable regardless of path shape.
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("env")
    ));
    fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Sibling backup path for the env file. Using `with_file_name` (not
/// `with_extension`) because `env` has no extension — `with_extension`
/// would produce `env.env.bak`.
fn backup_path(env_path: &Path) -> PathBuf {
    let name = env_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("env");
    env_path.with_file_name(format!("{name}.bak"))
}

// ---------- path utils ----------

/// Expand `~` / `~/…`, trim whitespace + trailing slashes.
///
/// We don't do full POSIX shell expansion (no `$VAR`, no globs, no `~user`)
/// — just the common `~/path` shorthand. If `$HOME` isn't set, tilde is
/// left as-is so the caller sees a predictable error rather than a
/// silently-wrong path.
fn normalize_dir(s: &str) -> String {
    let trimmed = s.trim().trim_end_matches('/');
    if trimmed == "~" {
        return env::var("HOME").unwrap_or_else(|_| trimmed.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Ok(home) = env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    trimmed.to_string()
}

// ---------- UI primitives ----------

/// Terminal width clamped to a comfortable line length. 72 is the upper
/// bound so long rules don't overwhelm; 48 is the lower bound so narrow
/// terminals still get a rule at all.
fn text_width() -> usize {
    Term::stdout().size().1.clamp(48, 72) as usize
}

/// One-line welcome — a bold colored title followed by a dim tagline.
/// No frame. rustup-init, cargo and fly.io all use this "unpretentious
/// authority" pattern; box-drawing around the welcome reads as decorative.
fn print_welcome() {
    println!();
    println!(
        "{}  {}",
        style("tebis").bold().cyan(),
        style(format!("v{}", env!("CARGO_PKG_VERSION"))).dim(),
    );
    println!(
        "{}",
        style("Telegram → tmux bridge · first-run setup").dim(),
    );
    println!();
}

/// Step divider: `───── Step 1 of 5 · Create a Telegram bot ─────────────`.
/// Horizontal rule at column 0, "Step N of 5" in cyan, title in bold,
/// remaining width filled with dim dashes so the eye tracks left-to-right.
fn step_header(n: u8, title: &str) {
    let width = text_width();
    let prefix = format!("───── Step {n} of 5 · ");
    let suffix = " ";
    let prefix_w = measure_text_width(&prefix);
    let title_w = measure_text_width(title);
    let trail = width
        .saturating_sub(prefix_w + title_w + suffix.len())
        .max(3);

    println!();
    println!();
    println!(
        "{}{}{}{}",
        style(&prefix).cyan(),
        style(title).bold(),
        suffix,
        style("─".repeat(trail)).dim(),
    );
    println!();
}

/// Major-section rule: `───── Review ─────────────────────────────────────`.
/// Used for Review, Done, Hardening. Same structure as the step header
/// without the numbered prefix.
fn divider_rule(label: &str) {
    let width = text_width();
    let prefix = "───── ";
    let suffix = " ";
    let label_w = measure_text_width(label);
    let trail = width
        .saturating_sub(prefix.len() + label_w + suffix.len())
        .max(3);

    println!();
    println!();
    println!(
        "{}{}{}{}",
        style(prefix).cyan(),
        style(label).bold(),
        suffix,
        style("─".repeat(trail)).dim(),
    );
    println!();
}

/// Inline info — blue `ℹ` glyph, plain prose. Used for neutral status
/// (pre-fill notices etc.) where the user doesn't need to act.
fn note_info(text: &str) {
    println!("{}  {text}", style("ℹ").blue().bold());
}

/// Inline warning — yellow `⚠` glyph, plain prose. No box, no pill. A
/// pill would claim more attention than the message warrants (the backup
/// is informational, not a failure).
fn note_warn(text: &str) {
    println!("{}  {text}", style("⚠").yellow().bold());
}

/// Key-value hint line used in step 1 for the `BotFather` name/username
/// prompts. `name` and `username` are bold, the description is plain,
/// the example is dim italic at a fixed right-hand column so both rows
/// align regardless of label length.
fn kv_row(label: &str, desc: &str, example: &str) {
    // Label column: pad to the widest label in use here so both lines
    // align. `username` is 8, `name` is 4 → 9 chars with trailing space.
    const LABEL_COL: usize = 10;
    const DESC_COL: usize = 22;
    let label_pad = LABEL_COL.saturating_sub(measure_text_width(label));
    let desc_pad = DESC_COL.saturating_sub(measure_text_width(desc));
    println!(
        "    {}{}{}{}e.g.  {}",
        style(label).bold(),
        " ".repeat(label_pad),
        desc,
        " ".repeat(desc_pad),
        style(example).dim().italic(),
    );
}

/// Review-screen row: indented dim label + value.
fn row(label: &str, value: &str) {
    println!("    {}  {value}", style(format!("{label:<10}")).dim());
}

// ---------- tests ----------

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

    /// Regression: `Path::with_extension` REPLACES the extension.
    /// For our file (`env`, no extension) `with_extension("env.bak")`
    /// would produce `env.env.bak` — not what we want. The write path
    /// uses `with_file_name` to append.
    #[test]
    fn backup_path_is_sibling() {
        assert_eq!(
            backup_path(Path::new("/tmp/tebis/env")),
            Path::new("/tmp/tebis/env.bak")
        );
    }

    #[test]
    fn tmp_path_is_sibling() {
        let env_path = Path::new("/tmp/tebis/env");
        let tmp = env_path.with_file_name(format!(
            "{}.tmp",
            env_path.file_name().and_then(|n| n.to_str()).unwrap()
        ));
        assert_eq!(tmp, Path::new("/tmp/tebis/env.tmp"));
    }

    /// Merged into one test because `env::set_var` mutates process-wide
    /// state and the default cargo-test runner is multi-threaded — two
    /// separate #[test]s running in parallel would race on `$HOME` and
    /// flake. This test owns HOME for its duration, then restores it.
    #[test]
    fn normalize_dir_all_shapes() {
        let prior = env::var("HOME").ok();
        // SAFETY: single-threaded test body; `prior` captured before the
        // mutation and restored after so other tests see a stable HOME.
        unsafe { env::set_var("HOME", "/Users/test") };

        assert_eq!(normalize_dir("~/projects/app"), "/Users/test/projects/app");
        assert_eq!(normalize_dir("~"), "/Users/test");
        assert_eq!(normalize_dir("  /tmp/foo/  "), "/tmp/foo");
        assert_eq!(normalize_dir("/tmp/foo/"), "/tmp/foo");
        assert_eq!(normalize_dir("/absolute/path"), "/absolute/path");

        match prior {
            Some(v) => unsafe { env::set_var("HOME", v) },
            None => unsafe { env::remove_var("HOME") },
        }
    }

    #[test]
    fn discover_parses_full_env_file() {
        let tmp = std::env::temp_dir().join(format!("tebis-discover-{}.env", std::process::id()));
        fs::write(
            &tmp,
            "\
# Written by `tebis setup`.

TELEGRAM_BOT_TOKEN=123:ABCdefGHIjklMNOpqrSTUvwxYZ-1234567890_abcd
TELEGRAM_ALLOWED_USER=1234567890
TELEGRAM_ALLOWED_SESSIONS=claude-code,shell

TELEGRAM_AUTOSTART_SESSION=demo
TELEGRAM_AUTOSTART_DIR=/tmp
TELEGRAM_AUTOSTART_COMMAND=claude

INSPECT_PORT=51624
",
        )
        .unwrap();

        let d = discover(&tmp);
        assert_eq!(
            d.bot_token.as_deref(),
            Some("123:ABCdefGHIjklMNOpqrSTUvwxYZ-1234567890_abcd")
        );
        assert_eq!(d.allowed_user, Some(1_234_567_890));
        assert_eq!(
            d.allowed_sessions.as_deref(),
            Some(&["claude-code".to_string(), "shell".to_string()][..]),
        );
        let a = d.autostart.expect("autostart triple present");
        assert_eq!(a.session, "demo");
        assert_eq!(a.dir, "/tmp");
        assert_eq!(a.command, "claude");
        assert_eq!(d.inspect_port, Some(51_624));

        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn discover_ignores_partial_autostart_triple() {
        let tmp =
            std::env::temp_dir().join(format!("tebis-discover-partial-{}.env", std::process::id()));
        fs::write(
            &tmp,
            "TELEGRAM_AUTOSTART_SESSION=foo\nTELEGRAM_AUTOSTART_DIR=/tmp\n",
        )
        .unwrap();
        let d = discover(&tmp);
        // Missing AUTOSTART_COMMAND → the triple is rejected.
        assert!(d.autostart.is_none());
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn discover_returns_default_when_file_missing() {
        let d = discover(Path::new("/tmp/tebis-does-not-exist-xyz"));
        assert!(d.bot_token.is_none());
        assert!(d.allowed_user.is_none());
        assert!(d.allowed_sessions.is_none());
        assert!(d.autostart.is_none());
        assert!(d.inspect_port.is_none());
    }

    #[test]
    fn discover_handles_permissive_allowlist() {
        // A commented-out allowlist (permissive) yields None, not Some(empty).
        let tmp = std::env::temp_dir().join(format!(
            "tebis-discover-permissive-{}.env",
            std::process::id()
        ));
        fs::write(
            &tmp,
            "TELEGRAM_BOT_TOKEN=123:ABCdefGHIjklMNOpqrSTUvwxYZ-1234567890_abcd\n\
             # TELEGRAM_ALLOWED_SESSIONS=commented,out\n",
        )
        .unwrap();
        let d = discover(&tmp);
        assert!(d.allowed_sessions.is_none());
        let _ = fs::remove_file(&tmp);
    }
}
