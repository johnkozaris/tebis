//! Terminal rendering primitives for the wizard: headers, dividers, rows,
//! notices, token masking, end-of-flow summary.
//!
//! Idiom: treat the terminal as a document. Body prose at column 0,
//! examples indented 4 spaces, step dividers as horizontal rules. No frames
//! or pills — they compete with the prose the user is here to read.

use std::path::Path;

use console::{Term, measure_text_width, style};

use super::{Autostart, HooksChoice};

fn text_width() -> usize {
    Term::stdout().size().1.clamp(48, 72) as usize
}

pub(super) fn print_welcome() {
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

pub(super) fn step_header(n: u8, title: &str) {
    let width = text_width();
    let prefix = format!("───── Step {n} of 6 · ");
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

pub(super) fn section_divider(label: &str) {
    divider_rule(label);
}

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

pub(super) fn note_info(text: &str) {
    println!("{}  {text}", style("ℹ").blue().bold());
}

pub(super) fn note_warn(text: &str) {
    println!("{}  {text}", style("⚠").yellow().bold());
}

pub(super) fn kv_row(label: &str, desc: &str, example: &str) {
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

fn row(label: &str, value: &str) {
    println!("    {}  {value}", style(format!("{label:<10}")).dim());
}

/// `123456789:ABC…XYZ` — enough to recognize, not enough to reuse.
pub(super) fn mask_token(token: &str) -> String {
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

#[allow(
    clippy::too_many_arguments,
    reason = "wizard-only helper; flat args map to flat output"
)]
pub(super) fn print_summary(
    token: &str,
    user_id: i64,
    sessions: &[String],
    autostart: Option<&Autostart>,
    hooks_mode: HooksChoice,
    inspect_port: Option<u16>,
    voice: Option<&super::VoiceChoice>,
    tts: Option<&super::TtsChoice>,
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
    let hooks_row = match hooks_mode {
        HooksChoice::Auto => style("auto-install on first message").to_string(),
        HooksChoice::Off => style("(pane-settle only)").dim().to_string(),
    };
    let dashboard_row = inspect_port.map_or_else(
        || style("(disabled)").dim().to_string(),
        |p| {
            format!(
                "http://127.0.0.1:{p}   {}",
                style("edit from the dashboard later").dim(),
            )
        },
    );
    let voice_row = voice.map_or_else(
        || style("(not configured)").dim().to_string(),
        |v| {
            if v.enabled {
                format!("local whisper.cpp · model: {}", style(&v.model).bold())
            } else {
                style("(disabled)").dim().to_string()
            }
        },
    );
    let tts_row = tts.map_or_else(
        || style("(not configured)").dim().to_string(),
        |t| {
            if t.enabled {
                let scope = if t.respond_to_all {
                    "all replies"
                } else {
                    "voice replies only"
                };
                format!("macOS `say` · voice: {} · {}", style(&t.voice).bold(), scope)
            } else {
                style("(disabled)").dim().to_string()
            }
        },
    );
    row("Bot token", &masked_token);
    row("User id", &user_id.to_string());
    row("Sessions", &sessions_row);
    row("Agent", &autostart_row);
    row("Hooks", &hooks_row);
    row("Dashboard", &dashboard_row);
    row("Voice in", &voice_row);
    row("Voice out", &tts_row);
    println!();
}

/// Confirmation that the env file was written. Follow-up prompt (run
/// now / install / exit) is driven by `setup::prompt_next_action`.
pub(super) fn print_wrote(env_path: &Path) {
    divider_rule("Saved");
    println!(
        "{}  Wrote {}.",
        style("✓").green().bold(),
        style(env_path.display()).bold(),
    );
    println!();
    print_security_tips();
}

/// Shown when the user picks "exit" at the end of setup — so they know
/// exactly how to start tebis later.
pub(super) fn print_manual_start(env_path: &Path, inspect_port: Option<u16>) {
    println!();
    println!("{} Start tebis later with:", style("›").dim());
    println!("    {}", style("tebis").bold());
    println!(
        "  {} auto-loads {} on launch.",
        style("(").dim(),
        style(env_path.display()).dim(),
    );
    println!();
    println!(
        "{} Install as a background service (auto-starts at login):",
        style("›").dim(),
    );
    println!("    {}", style("tebis install").bold());
    println!();
    if let Some(port) = inspect_port {
        println!(
            "{} Dashboard (once running): {}",
            style("›").dim(),
            style(format!("http://127.0.0.1:{port}"))
                .cyan()
                .underlined(),
        );
        println!();
    }
}

fn print_security_tips() {
    println!(
        "{} disable {} in BotFather so your bot can't be added to groups:",
        style("Hardening:").bold().yellow(),
        style("Allow Groups").bold(),
    );
    println!(
        "    {} → your bot → {} → {} → {}",
        style("/mybots").bold(),
        style("Bot Settings").bold(),
        style("Allow Groups?").bold(),
        style("Turn off").bold(),
    );
    println!();
}
