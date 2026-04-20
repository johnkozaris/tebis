//! Env-var parsing + validation.
//!
//! Config types live with the subsystems that consume them:
//! `AutostartConfig` in `bridge::session`, `NotifyConfig` in `notify`,
//! `AutoreplyConfig` in `bridge::autoreply`, `HooksMode` in
//! `agent_hooks`. This module is just a "populate from env" adapter
//! that knows which env vars map to which consumer type.

use anyhow::{Context, Result, bail};
use secrecy::{ExposeSecret, SecretString};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent_hooks::HooksMode;
use crate::bridge::autoreply::AutoreplyConfig;
use crate::bridge::session::AutostartConfig;
use crate::env_file;
use crate::notify::NotifyConfig;
use crate::tmux::is_valid_session_name;

pub struct Config {
    pub bot_token: SecretString,
    pub allowed_user_id: i64,
    pub allowed_sessions: Vec<String>,
    pub poll_timeout: u32,
    pub max_output_chars: usize,
    /// Outbound-notify listener. Enabled by default (`chat_id` defaults
    /// to `allowed_user_id`). Opt out with `TELEGRAM_NOTIFY=off`.
    pub notify: Option<NotifyConfig>,
    /// `Some` only when all three `TELEGRAM_AUTOSTART_*` env vars are set.
    pub autostart: Option<AutostartConfig>,
    /// TUI-agnostic auto-reply. Default on; opt out via
    /// `TELEGRAM_AUTOREPLY=off`.
    pub autoreply: Option<AutoreplyConfig>,
    /// How tebis handles agent hooks at autostart time.
    pub hooks_mode: HooksMode,
}

/// Parse an env-var toggle with a default when unset or empty.
/// Wraps [`env_file::parse_toggle`] so operators see which `TELEGRAM_*`
/// var failed when they pass a typo'd value.
fn parse_toggle_env(key: &str, default: bool) -> Result<bool> {
    let raw = env::var(key).unwrap_or_default();
    env_file::parse_toggle(&raw)
        .map(|v| v.unwrap_or(default))
        .with_context(|| format!("parsing {key}"))
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let bot_token: SecretString = env::var("TELEGRAM_BOT_TOKEN")
            .context("TELEGRAM_BOT_TOKEN env var not set")?
            .into();

        if bot_token.expose_secret().is_empty() {
            bail!("TELEGRAM_BOT_TOKEN is empty");
        }

        let allowed_user_id: i64 = env::var("TELEGRAM_ALLOWED_USER")
            .context("TELEGRAM_ALLOWED_USER env var not set")?
            .parse()
            .context("TELEGRAM_ALLOWED_USER must be a valid integer")?;

        if allowed_user_id <= 0 {
            bail!("TELEGRAM_ALLOWED_USER must be positive");
        }

        // Unset or empty → permissive mode (any regex-valid name resolves).
        let allowed_sessions: Vec<String> = env::var("TELEGRAM_ALLOWED_SESSIONS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        for name in &allowed_sessions {
            if !is_valid_session_name(name) {
                bail!(
                    "TELEGRAM_ALLOWED_SESSIONS contains invalid name {name:?} — \
                     only [A-Za-z0-9._-], 1..=64 chars allowed"
                );
            }
        }

        let poll_timeout: u32 = env::var("TELEGRAM_POLL_TIMEOUT")
            .unwrap_or_else(|_| "30".to_string())
            .parse()
            .context("TELEGRAM_POLL_TIMEOUT must be a valid integer")?;
        // 0 busy-loops getUpdates; 900 matches the dashboard slider range.
        if !(1..=900).contains(&poll_timeout) {
            bail!("TELEGRAM_POLL_TIMEOUT must be 1..=900 (0 busy-loops)");
        }

        let max_output_chars: usize = env::var("TELEGRAM_MAX_OUTPUT_CHARS")
            .unwrap_or_else(|_| "4000".to_string())
            .parse()
            .context("TELEGRAM_MAX_OUTPUT_CHARS must be a valid integer")?;
        // Mirrors the dashboard Settings-panel bounds.
        if !(100..=20_000).contains(&max_output_chars) {
            bail!("TELEGRAM_MAX_OUTPUT_CHARS must be 100..=20000");
        }

        let notify = load_notify_config(allowed_user_id)?;
        let autostart = load_autostart_config(&allowed_sessions)?;
        let autoreply = load_autoreply_config()?;
        let hooks_mode =
            HooksMode::from_env_str(&env::var("TELEGRAM_HOOKS_MODE").unwrap_or_default())
                .context("TELEGRAM_HOOKS_MODE")?;

        Ok(Self {
            bot_token,
            allowed_user_id,
            allowed_sessions,
            poll_timeout,
            max_output_chars,
            notify,
            autostart,
            autoreply,
            hooks_mode,
        })
    }
}

/// Autoreply (pane-settle) is on by default as the universal fallback
/// for any TUI. `TELEGRAM_AUTOREPLY=off` disables it — the common pair
/// is `TELEGRAM_HOOKS_MODE=auto` + `TELEGRAM_AUTOREPLY=off` when you
/// only want precise hook-driven replies from Claude / Copilot. Note
/// that pane-settle is already suppressed per-session when hooks are
/// installed for that session, so you rarely need `off`.
fn load_autoreply_config() -> Result<Option<AutoreplyConfig>> {
    let enabled = parse_toggle_env("TELEGRAM_AUTOREPLY", true)?;
    Ok(if enabled {
        Some(AutoreplyConfig::default())
    } else {
        None
    })
}

/// All three env vars must be set together — a partial triple is an error,
/// not a silent fallback.
fn load_autostart_config(allowed_sessions: &[String]) -> Result<Option<AutostartConfig>> {
    let session = env::var("TELEGRAM_AUTOSTART_SESSION").ok();
    let dir = env::var("TELEGRAM_AUTOSTART_DIR").ok();
    let command = env::var("TELEGRAM_AUTOSTART_COMMAND").ok();

    match (session, dir, command) {
        (None, None, None) => Ok(None),
        (Some(session), Some(dir), Some(command))
            if !session.is_empty() && !dir.is_empty() && !command.is_empty() =>
        {
            if !is_valid_session_name(&session) {
                bail!(
                    "TELEGRAM_AUTOSTART_SESSION {session:?} is invalid — \
                     only [A-Za-z0-9._-], 1..=64 chars allowed"
                );
            }
            if !allowed_sessions.is_empty() && !allowed_sessions.iter().any(|s| s == &session) {
                bail!(
                    "TELEGRAM_AUTOSTART_SESSION {session:?} must be in TELEGRAM_ALLOWED_SESSIONS"
                );
            }
            reject_control_chars(&dir, "TELEGRAM_AUTOSTART_DIR")?;
            reject_control_chars(&command, "TELEGRAM_AUTOSTART_COMMAND")?;
            // Fail-fast so the first plain-text message doesn't hit an
            // opaque "can't cd" error deep inside tmux's spawn.
            if !std::path::Path::new(&dir).is_dir() {
                bail!("TELEGRAM_AUTOSTART_DIR {dir:?} does not exist or is not a directory");
            }
            Ok(Some(AutostartConfig {
                session,
                dir,
                command,
            }))
        }
        _ => bail!(
            "TELEGRAM_AUTOSTART_{{SESSION,DIR,COMMAND}} must all be set together, or all unset"
        ),
    }
}

/// Reject control chars in tmux argv — tmux would reject them too, but
/// catching it at config load gives a clearer error.
fn reject_control_chars(value: &str, name: &str) -> Result<()> {
    if let Some(bad) = value.chars().find(|c| c.is_control()) {
        bail!(
            "{name} contains a control character (U+{:04X}); tmux argv values must be printable",
            bad as u32
        );
    }
    Ok(())
}

/// The outbound-notify listener is on by default; `chat_id` defaults to
/// the authorized user id so hooks can forward to the same person who
/// sent the original message. Opt out with `TELEGRAM_NOTIFY=off`.
/// `NOTIFY_CHAT_ID=<id>` overrides the default target.
fn load_notify_config(allowed_user_id: i64) -> Result<Option<NotifyConfig>> {
    if !parse_toggle_env("TELEGRAM_NOTIFY", true)? {
        return Ok(None);
    }

    let chat_id: i64 = match env::var("NOTIFY_CHAT_ID").ok() {
        Some(s) if !s.is_empty() => s
            .parse()
            .context("NOTIFY_CHAT_ID must be a valid integer")?,
        _ => allowed_user_id,
    };
    if chat_id == 0 {
        bail!("NOTIFY_CHAT_ID is 0 — set TELEGRAM_ALLOWED_USER first or override NOTIFY_CHAT_ID");
    }

    let socket_path = env::var("NOTIFY_SOCKET_PATH")
        .map(PathBuf::from)
        .ok()
        .or_else(default_socket_path)
        .context(
            "Unable to determine notify socket path: set NOTIFY_SOCKET_PATH \
             or ensure XDG_RUNTIME_DIR is set",
        )?;

    Ok(Some(NotifyConfig {
        socket_path,
        chat_id,
    }))
}

/// Load a `KEY=VALUE` env file into the current process. Skips blank
/// lines and `#` comments. Does **not** override vars already set in the
/// environment (so `systemd`'s `EnvironmentFile=` and launchd's
/// `set -a ; source` keep precedence).
///
/// # Safety
///
/// Must be called **before** any tokio runtime starts or any thread is
/// spawned. `std::env::set_var` is sound under edition 2024 only when no
/// other thread could observe the write (it mutates a process-global
/// table). `main()` calling this pre-runtime is fine; calling from
/// anywhere async is a data race.
pub unsafe fn load_env_file(path: &Path) -> Result<()> {
    let content =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    for line in content.lines() {
        let Some((key, value)) = env_file::parse_kv_line(line) else {
            continue;
        };
        if env::var_os(key).is_some() {
            continue;
        }
        // SAFETY: invariant forwarded from our `unsafe` signature — caller
        // guarantees no threads are running.
        unsafe {
            env::set_var(key, value);
        }
    }
    Ok(())
}

/// `$XDG_RUNTIME_DIR/tebis.sock` (Linux / systemd), else
/// `/tmp/tebis-$USER.sock` (macOS / fallback).
fn default_socket_path() -> Option<PathBuf> {
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("tebis.sock"));
    }
    if let Ok(user) = env::var("USER")
        && !user.is_empty()
    {
        return Some(PathBuf::from(format!("/tmp/tebis-{user}.sock")));
    }
    None
}
