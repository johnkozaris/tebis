//! Env-var parsing for the bridge's `Config`.
//!
//! This module owns parsing + validation only — the config types themselves
//! live with the subsystems that consume them (`AutostartConfig` in
//! [`crate::session`], `NotifyConfig` in [`crate::notify`]). That way each
//! subsystem owns the shape of its own configuration, and `config.rs` is a
//! thin "populate from env" adapter you could swap for a file loader,
//! argv, etc., without touching downstream code.

use anyhow::{Context, Result, bail};
use secrecy::{ExposeSecret, SecretString};
use std::env;
use std::path::PathBuf;

use crate::notify::NotifyConfig;
use crate::session::AutostartConfig;
use crate::tmux::is_valid_session_name;

pub struct Config {
    pub bot_token: SecretString,
    pub allowed_user_id: i64,
    pub allowed_sessions: Vec<String>,
    pub poll_timeout: u32,
    pub max_output_chars: usize,
    /// Optional outbound-notification listener. `Some` only when BOTH
    /// `NOTIFY_SOCKET_PATH` and `NOTIFY_CHAT_ID` are set — it's opt-in so
    /// existing deployments behave unchanged.
    pub notify: Option<NotifyConfig>,
    /// Optional auto-start of a default tmux session on the first plain-text
    /// message. `Some` only when ALL three `TELEGRAM_AUTOSTART_*` env vars
    /// are set. Session name must be in `allowed_sessions`.
    pub autostart: Option<AutostartConfig>,
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

        // Empty / unset `TELEGRAM_ALLOWED_SESSIONS` means *permissive*:
        // any name passing the regex resolves. See `tmux::Tmux::is_permissive`.
        // This is now the default for fresh installs — the `tebis setup`
        // wizard only writes this env var when the user opts into restricting
        // the bridge. Existing deployments that already have the line keep
        // strict behavior unchanged.
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
        // 0 would busy-loop getUpdates. The upper bound tracks the
        // inspect dashboard's slider range (1..=900) so a value set via
        // the web UI never fails startup.
        if !(1..=900).contains(&poll_timeout) {
            bail!("TELEGRAM_POLL_TIMEOUT must be 1..=900 (0 busy-loops)");
        }

        let max_output_chars: usize = env::var("TELEGRAM_MAX_OUTPUT_CHARS")
            .unwrap_or_else(|_| "4000".to_string())
            .parse()
            .context("TELEGRAM_MAX_OUTPUT_CHARS must be a valid integer")?;
        // Mirror the dashboard Settings panel's bounds so a value reached
        // from either path fails the same way at startup. 100 is the floor
        // below which /read output would barely show one line; 20_000 is
        // the ceiling above which Telegram's 4096-char message cap would
        // truncate us anyway.
        if !(100..=20_000).contains(&max_output_chars) {
            bail!("TELEGRAM_MAX_OUTPUT_CHARS must be 100..=20000");
        }

        let notify = load_notify_config()?;
        let autostart = load_autostart_config(&allowed_sessions)?;

        Ok(Self {
            bot_token,
            allowed_user_id,
            allowed_sessions,
            poll_timeout,
            max_output_chars,
            notify,
            autostart,
        })
    }
}

/// Load optional autostart config. All three env vars must be set; any
/// partial combination is a configuration error, not a silent fallback.
///
/// When `allowed_sessions` is non-empty (strict mode), the autostart
/// session must be in it — otherwise the first plain-text message would
/// fail at `has_session` with a confusing error. When it's empty
/// (permissive mode), only the name regex is checked.
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
            // Fail-fast if the dir is missing — without this, the first
            // plain-text message hits an opaque "can't cd" error deep
            // inside tmux's spawn. Symlinks are resolved by `is_dir`, which
            // returns true only for a dir or a symlink pointing to one.
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

/// Reject control chars (newline, CR, tab, C0/C1) in values passed to tmux
/// as subprocess argv. tmux would reject them too, but catching it at
/// config load gives a clearer message than a cryptic tmux error on the
/// first message.
fn reject_control_chars(value: &str, name: &str) -> Result<()> {
    if let Some(bad) = value.chars().find(|c| c.is_control()) {
        bail!(
            "{name} contains a control character (U+{:04X}); tmux argv values must be printable",
            bad as u32
        );
    }
    Ok(())
}

/// Load optional notify config. Enabled only if BOTH env vars are set.
///
/// Socket path resolution when `NOTIFY_SOCKET_PATH` is unset but `NOTIFY_CHAT_ID`
/// is: prefer `$XDG_RUNTIME_DIR/tebis.sock` (systemd-managed), fall
/// back to `/tmp/tebis-<uid>.sock` (0600 makes it safe to share /tmp).
fn load_notify_config() -> Result<Option<NotifyConfig>> {
    let Ok(chat_id) = env::var("NOTIFY_CHAT_ID") else {
        return Ok(None);
    };
    let chat_id: i64 = chat_id
        .parse()
        .context("NOTIFY_CHAT_ID must be a valid integer")?;
    if chat_id == 0 {
        bail!("NOTIFY_CHAT_ID must be non-zero");
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

fn default_socket_path() -> Option<PathBuf> {
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("tebis.sock"));
    }
    // Fallback for macOS / non-systemd environments. User-scoped so two
    // users on the same host don't clobber each other's sockets (0600 perms
    // enforce isolation, but distinct paths are easier to reason about).
    if let Ok(user) = env::var("USER")
        && !user.is_empty()
    {
        return Some(PathBuf::from(format!("/tmp/tebis-{user}.sock")));
    }
    None
}
