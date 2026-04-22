//! Env → `Config`. Each subsystem owns its own config struct; this module
//! just maps env vars onto them.

use anyhow::{Context, Result, bail};
use secrecy::{ExposeSecret, SecretString};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent_hooks::HooksMode;
use crate::audio::AudioConfig;
use crate::audio::stt::SttConfig;
use crate::audio::tts::{BackendConfig as TtsBackendConfig, TtsConfig};
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
    pub notify: Option<NotifyConfig>,
    pub autostart: Option<AutostartConfig>,
    pub autoreply: Option<AutoreplyConfig>,
    pub hooks_mode: HooksMode,
    pub audio: AudioConfig,
}

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

        // Empty → permissive mode.
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
        if !(1..=900).contains(&poll_timeout) {
            bail!("TELEGRAM_POLL_TIMEOUT must be 1..=900 (0 busy-loops)");
        }

        let max_output_chars: usize = env::var("TELEGRAM_MAX_OUTPUT_CHARS")
            .unwrap_or_else(|_| "4000".to_string())
            .parse()
            .context("TELEGRAM_MAX_OUTPUT_CHARS must be a valid integer")?;
        if !(100..=20_000).contains(&max_output_chars) {
            bail!("TELEGRAM_MAX_OUTPUT_CHARS must be 100..=20000");
        }

        let notify = load_notify_config(allowed_user_id)?;
        let autostart = load_autostart_config(&allowed_sessions)?;
        let autoreply = load_autoreply_config()?;
        let hooks_mode =
            HooksMode::from_env_str(&env::var("TELEGRAM_HOOKS_MODE").unwrap_or_default())
                .context("TELEGRAM_HOOKS_MODE")?;
        let audio = load_audio_config()?;

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
            audio,
        })
    }
}

fn load_audio_config() -> Result<AudioConfig> {
    let stt = load_stt_config()?;
    let tts = load_tts_config()?;
    Ok(AudioConfig { stt, tts })
}

fn load_tts_config() -> Result<Option<TtsConfig>> {
    let backend_raw = env::var("TELEGRAM_TTS_BACKEND").ok();
    let legacy_on = parse_toggle_env("TELEGRAM_TTS", false)?;

    let backend_kind = match backend_raw.as_deref().map(str::trim) {
        Some("") | None => {
            if !legacy_on {
                return Ok(None);
            }
            #[cfg(target_os = "macos")]
            {
                "say".to_string()
            }
            #[cfg(not(target_os = "macos"))]
            {
                bail!(
                    "TELEGRAM_TTS=on with no TELEGRAM_TTS_BACKEND — \
                     set TELEGRAM_TTS_BACKEND to one of \
                     say (macOS only), kokoro-local, kokoro-remote, or none"
                );
            }
        }
        Some(s) => s.to_ascii_lowercase(),
    };

    if matches!(backend_kind.as_str(), "none" | "off" | "false" | "0") {
        return Ok(None);
    }

    let respond_to_all = parse_toggle_env("TELEGRAM_TTS_RESPOND_TO_ALL", false)?;

    let backend = match backend_kind.as_str() {
        "say" => {
            #[cfg(not(target_os = "macos"))]
            {
                bail!(
                    "TELEGRAM_TTS_BACKEND=say is macOS-only — \
                     use kokoro-local or kokoro-remote on this platform"
                );
            }
            #[cfg(target_os = "macos")]
            {
                let voice =
                    env::var("TELEGRAM_TTS_VOICE").unwrap_or_else(|_| "Samantha".to_string());
                TtsBackendConfig::Say { voice }
            }
        }
        "kokoro-local" | "kokoro_local" | "local" => {
            let voice =
                env::var("TELEGRAM_TTS_VOICE").unwrap_or_else(|_| "af_sarah".to_string());
            ensure_safe_voice_name(&voice)?;
            let model = env::var("TELEGRAM_TTS_MODEL").ok().unwrap_or_else(|| {
                crate::audio::manifest::get()
                    .default_tts_model()
                    .unwrap_or("kokoro-v1.0")
                    .to_string()
            });
            TtsBackendConfig::KokoroLocal { model, voice }
        }
        "kokoro-remote" | "kokoro_remote" | "remote" => {
            let url = env::var("TELEGRAM_TTS_REMOTE_URL").context(
                "TELEGRAM_TTS_BACKEND=remote requires TELEGRAM_TTS_REMOTE_URL",
            )?;
            if url.trim().is_empty() {
                bail!("TELEGRAM_TTS_REMOTE_URL must not be empty");
            }
            let allow_http = parse_toggle_env("TELEGRAM_TTS_REMOTE_ALLOW_HTTP", false)?;
            let lower = url.trim().to_ascii_lowercase();
            let is_https = lower.starts_with("https://");
            let is_allowed_http = allow_http && lower.starts_with("http://");
            if !is_https && !is_allowed_http {
                bail!(
                    "TELEGRAM_TTS_REMOTE_URL must start with https:// \
                     (set TELEGRAM_TTS_REMOTE_ALLOW_HTTP=true to allow http:// for LAN)"
                );
            }
            let api_key = env::var("TELEGRAM_TTS_REMOTE_API_KEY")
                .ok()
                .filter(|s| !s.is_empty())
                .map(SecretString::from);
            let model = env::var("TELEGRAM_TTS_REMOTE_MODEL")
                .unwrap_or_else(|_| "kokoro".to_string());
            let voice =
                env::var("TELEGRAM_TTS_VOICE").unwrap_or_else(|_| "af_sarah".to_string());
            let timeout_sec: u32 = env::var("TELEGRAM_TTS_REMOTE_TIMEOUT_SEC")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .context("TELEGRAM_TTS_REMOTE_TIMEOUT_SEC must be a positive integer")?;
            if !(1..=300).contains(&timeout_sec) {
                bail!("TELEGRAM_TTS_REMOTE_TIMEOUT_SEC must be 1..=300");
            }
            TtsBackendConfig::Remote {
                url: url.trim().to_string(),
                api_key,
                model,
                voice,
                timeout_sec,
            }
        }
        other => bail!(
            "TELEGRAM_TTS_BACKEND must be one of: \
             say, kokoro-local, kokoro-remote, none (got {other:?})"
        ),
    };

    Ok(Some(TtsConfig {
        backend,
        respond_to_all,
    }))
}

fn load_stt_config() -> Result<Option<SttConfig>> {
    if !parse_toggle_env("TELEGRAM_STT", false)? {
        return Ok(None);
    }

    let default_model = crate::audio::manifest::get()
        .default_stt_model()
        .context("resolving default STT model from manifest")?
        .to_string();
    let model = env::var("TELEGRAM_STT_MODEL").unwrap_or(default_model);

    let language = env::var("TELEGRAM_STT_LANGUAGE").unwrap_or_else(|_| "en".to_string());

    let max_duration_sec: u32 = env::var("TELEGRAM_STT_MAX_DURATION_SEC")
        .unwrap_or_else(|_| "120".to_string())
        .parse()
        .context("TELEGRAM_STT_MAX_DURATION_SEC must be a positive integer")?;
    if !(1..=900).contains(&max_duration_sec) {
        bail!("TELEGRAM_STT_MAX_DURATION_SEC must be 1..=900");
    }

    let max_bytes: u32 = env::var("TELEGRAM_STT_MAX_BYTES")
        .unwrap_or_else(|_| "20971520".to_string()) // 20 MB
        .parse()
        .context("TELEGRAM_STT_MAX_BYTES must be a non-negative integer")?;
    if !(65_536..=52_428_800).contains(&max_bytes) {
        bail!("TELEGRAM_STT_MAX_BYTES must be 65536..=52428800 (64 KiB .. 50 MiB)");
    }

    let threads: u32 = match env::var("TELEGRAM_STT_THREADS")
        .unwrap_or_else(|_| "auto".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "auto" | "" => default_threads(),
        s => s
            .parse()
            .context("TELEGRAM_STT_THREADS must be 'auto' or a positive integer")?,
    };
    if !(1..=32).contains(&threads) {
        bail!("TELEGRAM_STT_THREADS must be 1..=32");
    }

    Ok(Some(SttConfig {
        model,
        language,
        max_duration_sec,
        max_bytes,
        threads,
    }))
}

/// Half of logical CPUs clamped to `[2, 8]`. Whisper scales to ~8 threads
/// on small models; past that it's contention.
fn default_threads() -> u32 {
    let total = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    let half = (total / 2).clamp(2, 8);
    u32::try_from(half).unwrap_or(4)
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

/// Defensive check on `TELEGRAM_TTS_VOICE`: `[A-Za-z0-9._-]{1,64}`, same
/// bar as tmux session names. The Kokoro crate builds the voice file
/// path as `voices_dir.join(format!("{voice_name}.bin"))`; without this
/// check a `/` or `..` would become a path-traversal surface if manifest
/// validation ordering ever changes.
fn ensure_safe_voice_name(voice: &str) -> Result<()> {
    if voice.is_empty() || voice.len() > 64 {
        bail!("TELEGRAM_TTS_VOICE must be 1..=64 chars (got {})", voice.len());
    }
    if !voice
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!(
            "TELEGRAM_TTS_VOICE {voice:?} contains disallowed characters — \
             use [A-Za-z0-9._-] only"
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
