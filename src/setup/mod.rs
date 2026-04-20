//! `tebis setup` — interactive first-run wizard.
//!
//! Walks the user through creating a bot, finding their numeric user id,
//! picking session names, and (optionally) autostart + the inspect dashboard.
//! Writes `~/.config/tebis/env` (mode 0600).

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Select};

use crate::env_file;

mod discover;
mod steps;
mod ui;

/// Keys the wizard actively manages — any of these present in an
/// existing env file gets replaced by the wizard's new values. Every
/// other key is preserved verbatim so users don't lose hand-added
/// settings (`TELEGRAM_HOOKS_MODE`, `TELEGRAM_NOTIFY`, etc.) when they
/// re-run `tebis setup`.
const WIZARD_MANAGED_KEYS: &[&str] = &[
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_ALLOWED_USER",
    "TELEGRAM_ALLOWED_SESSIONS",
    "TELEGRAM_AUTOSTART_SESSION",
    "TELEGRAM_AUTOSTART_DIR",
    "TELEGRAM_AUTOSTART_COMMAND",
    "INSPECT_PORT",
    "BRIDGE_ENV_FILE",
    "TELEGRAM_HOOKS_MODE",
];

/// Autostart triple. Shared between the step that collects it and the
/// discover pass that reloads it from an existing env file.
pub(super) struct Autostart {
    pub(super) session: String,
    pub(super) dir: String,
    pub(super) command: String,
}

/// What the caller should do after `run()` returns. Separates wizard
/// (interactive, blocking) from service-management (needs tokio for
/// `RunForeground`) so `fn main` can dispatch cleanly.
pub enum Next {
    /// Nothing more — the wizard printed instructions for manual start.
    Exit,
    /// Load the env file into this process and start the foreground daemon.
    RunForeground,
    /// Install as a background service via `crate::service::install`.
    Install,
}

pub fn run() -> Result<Next> {
    let theme = ColorfulTheme::default();
    ui::print_welcome();

    let env_path = env_file_path()?;
    let discovered = discover::discover(&env_path);
    if env_path.exists() {
        ui::note_info(&format!(
            "Loaded current config from {}.",
            style(env_path.display()).bold(),
        ));
        println!(
            "   Each prompt is pre-filled — press {} to keep.",
            style("Enter").bold(),
        );
        ui::note_warn(&format!(
            "Will be backed up to {} before saving.",
            style(backup_path(&env_path).display()).bold(),
        ));
        println!();
    }

    let token = steps::step_bot_token(&theme, discovered.bot_token.as_deref())?;
    let user_id = steps::step_user_id(&theme, discovered.allowed_user)?;
    let sessions = steps::step_session_allowlist(&theme, discovered.allowed_sessions.as_deref())?;
    let autostart = steps::step_autostart(&theme, &sessions, discovered.autostart.as_ref())?;
    let hooks_mode = steps::step_hooks_mode(&theme, autostart.as_ref(), discovered.hooks_mode)?;
    let inspect_port = steps::step_inspect_port(&theme, discovered.inspect_port)?;

    ui::print_summary(
        &token,
        user_id,
        &sessions,
        autostart.as_ref(),
        hooks_mode,
        inspect_port,
    );
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
        return Ok(Next::Exit);
    }

    // Preserve any user-managed keys (NOTIFY, AUTOREPLY, etc.) from an
    // existing env file so re-running the wizard never silently drops them.
    let extras = extra_lines_to_preserve(&env_path);
    let mut content = build_env_file(
        &token,
        user_id,
        &sessions,
        autostart.as_ref(),
        inspect_port,
        hooks_mode,
        &env_path,
    );
    if !extras.is_empty() {
        content.push_str("\n# Additional settings preserved from the previous env file.\n");
        for line in &extras {
            content.push_str(line);
            content.push('\n');
        }
    }

    if env_path.exists() {
        let bak = backup_path(&env_path);
        // `fs::copy` truncates then writes — a crash between truncate and
        // write leaves a torn .bak the user might later restore from.
        // Atomic-write via our shared helper so the backup inherits the
        // same 0600 + fsync + rename guarantees as the primary file.
        let existing = fs::read_to_string(&env_path)
            .with_context(|| format!("reading {} for backup", env_path.display()))?;
        env_file::atomic_write_0600(&bak, &existing)
            .with_context(|| format!("writing backup to {}", bak.display()))?;
    }
    env_file::atomic_write_0600(&env_path, &content)?;

    ui::print_wrote(&env_path);
    prompt_next_action(&theme, &env_path, inspect_port)
}

fn prompt_next_action(
    theme: &ColorfulTheme,
    env_path: &Path,
    inspect_port: Option<u16>,
) -> Result<Next> {
    // If a background service is already running, the env changes we
    // just wrote won't take effect until restart. Offer a one-shot
    // restart rather than leave the user with mysteriously stale behavior.
    if crate::service::is_running() {
        ui::section_divider("Service restart");
        ui::note_warn(
            "tebis is already running as a background service. The new \
             env file only takes effect after restart.",
        );
        println!();
        if Confirm::with_theme(theme)
            .with_prompt("Restart the service now?")
            .default(true)
            .interact()
            .context("prompt: restart service")?
            && let Err(e) = crate::service::restart()
        {
            ui::note_warn(&format!(
                "restart failed: {e} — run `tebis restart` manually."
            ));
        }
        return Ok(Next::Exit);
    }

    ui::section_divider("What next");
    let choices = [
        "Start tebis now in this terminal (Ctrl-C to stop)",
        "Install as a background service (auto-starts at login)",
        "Exit — I'll start it manually later",
    ];
    let choice = Select::with_theme(theme)
        .with_prompt("How do you want to run tebis?")
        .items(choices.as_slice())
        .default(0)
        .interact()
        .context("prompt: next action")?;
    match choice {
        0 => Ok(Next::RunForeground),
        1 => Ok(Next::Install),
        _ => {
            ui::print_manual_start(env_path, inspect_port);
            Ok(Next::Exit)
        }
    }
}

/// `$HOME/.config/tebis/env`.
pub fn env_file_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(".config/tebis/env"))
}

fn build_env_file(
    token: &str,
    user_id: i64,
    sessions: &[String],
    autostart: Option<&Autostart>,
    inspect_port: Option<u16>,
    hooks_mode: HooksChoice,
    env_path: &Path,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("# Written by `tebis setup`. Safe to hand-edit.\n\n");
    let _ = writeln!(out, "TELEGRAM_BOT_TOKEN={token}");
    let _ = writeln!(out, "TELEGRAM_ALLOWED_USER={user_id}");
    if sessions.is_empty() {
        out.push_str(
            "# TELEGRAM_ALLOWED_SESSIONS unset → any tmux session name is accepted.\n\
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

    match hooks_mode {
        HooksChoice::Auto => {
            out.push_str(
                "\n# Auto-install agent hooks at autostart. Replies come via\n\
                 # the agent's native Stop event (precise) rather than pane-settle.\n",
            );
            out.push_str("TELEGRAM_HOOKS_MODE=auto\n");
        }
        HooksChoice::Off => {}
    }

    if let Some(port) = inspect_port {
        out.push_str("\n# Local HTML control dashboard (loopback only).\n");
        let _ = writeln!(out, "INSPECT_PORT={port}");
        out.push_str("# Enables the Settings-edit form on the dashboard.\n");
        let _ = writeln!(out, "BRIDGE_ENV_FILE={}", env_path.display());
    }

    out
}

/// Read the existing env file and return every line that sets a key
/// NOT in `WIZARD_MANAGED_KEYS`. These are user-added settings
/// (`TELEGRAM_NOTIFY`, `TELEGRAM_AUTOREPLY`, `NOTIFY_CHAT_ID`, etc.) we
/// must not silently drop when the wizard rewrites the file.
///
/// Comments and blank lines adjacent to preserved keys are kept in
/// order (a blank between two preserved keys survives, but a blank
/// inside a wizard-managed block is dropped — simplest sound policy).
fn extra_lines_to_preserve(env_path: &Path) -> Vec<String> {
    let Ok(content) = fs::read_to_string(env_path) else {
        return Vec::new();
    };
    let managed: HashSet<&str> = WIZARD_MANAGED_KEYS.iter().copied().collect();
    content
        .lines()
        .filter_map(|line| match env_file::parse_kv_line(line) {
            Some((k, _)) if managed.contains(k) => None,
            Some(_) => Some(line.to_string()),
            // Keep comments and blanks only if they're near a preserved
            // key — simpler: drop them. Users who want comments can add
            // them back. The wizard emits its own comment headers.
            None => None,
        })
        .collect()
}

/// Where the user landed on the hooks-mode question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HooksChoice {
    Auto,
    Off,
}

fn backup_path(env_path: &Path) -> PathBuf {
    let name = env_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("env");
    env_path.with_file_name(format!("{name}.bak"))
}

/// Expand `~` / `~/…`, trim whitespace + trailing slashes. Tilde without
/// `$HOME` is left as-is so the caller sees a clear error downstream.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Merged into one test — `env::set_var` is process-wide and cargo
    /// test is multi-threaded by default; two `#[test]`s would race on $HOME.
    #[test]
    fn normalize_dir_all_shapes() {
        let prior = env::var("HOME").ok();
        // SAFETY: single-threaded test body; restored below.
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
}
