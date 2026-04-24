//! `tebis setup` — interactive first-run wizard. Writes `~/.config/tebis/env` (0600).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Select};

use crate::env_file;

mod discover;
mod installer;
pub mod onnxruntime;
/// `pub` so `examples/kokoro-smoke.rs` can call `probe_espeak_ng`.
pub mod phonemizer;
mod steps;
mod ui;

/// Keys the wizard rewrites on every run. Everything else is preserved verbatim.
const WIZARD_MANAGED_KEYS_ALWAYS: &[&str] = &[
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_ALLOWED_USER",
    "TELEGRAM_ALLOWED_SESSIONS",
    "TELEGRAM_AUTOSTART_SESSION",
    "TELEGRAM_AUTOSTART_DIR",
    "TELEGRAM_AUTOSTART_COMMAND",
    "INSPECT_PORT",
    "BRIDGE_ENV_FILE",
    "TELEGRAM_HOOKS_MODE",
    "TELEGRAM_STT",
    "TELEGRAM_STT_MODEL",
    // Legacy boolean `TELEGRAM_TTS` listed so re-runs retire it cleanly.
    "TELEGRAM_TTS",
    "TELEGRAM_TTS_BACKEND",
    "TELEGRAM_TTS_VOICE",
    "TELEGRAM_TTS_MODEL",
    "TELEGRAM_TTS_RESPOND_TO_ALL",
    "TELEGRAM_TTS_REMOTE_URL",
    "TELEGRAM_TTS_REMOTE_API_KEY",
    "TELEGRAM_TTS_REMOTE_MODEL",
    "TELEGRAM_TTS_REMOTE_TIMEOUT_SEC",
    "TELEGRAM_TTS_REMOTE_ALLOW_HTTP",
    // `ORT_DYLIB_PATH` is written by the wizard when Kokoro-local is
    // chosen (points at the brew-installed libonnxruntime). Listed here
    // so switching to a different backend on re-run doesn't leave a
    // stale path behind.
    "ORT_DYLIB_PATH",
];

fn wizard_managed_keys() -> impl Iterator<Item = &'static str> {
    WIZARD_MANAGED_KEYS_ALWAYS.iter().copied()
}

pub(super) struct Autostart {
    pub(super) session: String,
    pub(super) dir: String,
    pub(super) command: String,
}

#[derive(Clone, Debug)]
pub(super) struct VoiceChoice {
    pub(super) enabled: bool,
    /// Key from `audio::manifest.stt_models`.
    pub(super) model: String,
}

/// Wizard-side mirror of [`crate::audio::tts::BackendConfig`].
#[derive(Clone, Debug)]
pub(super) enum TtsChoice {
    /// Written as `TELEGRAM_TTS_BACKEND=none`.
    Off,
    Say {
        voice: String,
        respond_to_all: bool,
    },
    KokoroLocal {
        model: String,
        voice: String,
        respond_to_all: bool,
        /// Full path to `libonnxruntime.{dylib,so}` — written as
        /// `ORT_DYLIB_PATH=<path>` so the daemon's `libloading` call
        /// can find the shared library on Apple Silicon (where
        /// `/opt/homebrew/lib` isn't in the default dyld search path)
        /// and on Linux distros that don't symlink to `/usr/lib`.
        /// `None` means "trust the default search path" — suitable for
        /// env-file re-reads where the user or `tebis setup` has
        /// already set this separately.
        ort_dylib_path: Option<String>,
    },
    KokoroRemote {
        url: String,
        api_key: Option<String>,
        model: String,
        voice: String,
        timeout_sec: u32,
        allow_http: bool,
        respond_to_all: bool,
    },
}

impl TtsChoice {
    pub(super) const fn respond_to_all(&self) -> bool {
        match self {
            Self::Off => false,
            Self::Say { respond_to_all, .. }
            | Self::KokoroLocal { respond_to_all, .. }
            | Self::KokoroRemote { respond_to_all, .. } => *respond_to_all,
        }
    }

    /// User-visible voice name, or empty for `Off`.
    pub(super) fn voice_display(&self) -> &str {
        match self {
            Self::Off => "",
            Self::Say { voice, .. }
            | Self::KokoroLocal { voice, .. }
            | Self::KokoroRemote { voice, .. } => voice,
        }
    }
}

/// What the caller should do after `run()` returns.
pub enum Next {
    Exit,
    RunForeground,
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
    let voice = steps::step_voice(&theme, discovered.voice.as_ref())?;
    let tts = steps::step_tts(&theme, discovered.tts.as_ref())?;

    ui::print_summary(
        &token,
        user_id,
        &sessions,
        autostart.as_ref(),
        hooks_mode,
        inspect_port,
        voice.as_ref(),
        tts.as_ref(),
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

    let extras = extra_lines_to_preserve(&env_path);
    let mut content = build_env_file(
        &token,
        user_id,
        &sessions,
        autostart.as_ref(),
        inspect_port,
        hooks_mode,
        voice.as_ref(),
        tts.as_ref(),
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
        // Atomic backup via the shared helper — `fs::copy` could tear mid-write.
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
    // Service already running — new env won't apply until restart.
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

/// Canonical tebis env-file path — resolved per-OS via
/// [`crate::platform::paths::env_file_path`].
pub fn env_file_path() -> Result<PathBuf> {
    crate::platform::paths::env_file_path()
}

#[allow(
    clippy::too_many_arguments,
    reason = "grouping wizard outputs into a struct just for fn-arity hurts readability"
)]
fn build_env_file(
    token: &str,
    user_id: i64,
    sessions: &[String],
    autostart: Option<&Autostart>,
    inspect_port: Option<u16>,
    hooks_mode: HooksChoice,
    voice: Option<&VoiceChoice>,
    tts: Option<&TtsChoice>,
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

    if let Some(v) = voice {
        out.push_str("\n# Voice input (STT). Transcribes Telegram voice notes in-process\n");
        out.push_str("# via whisper-rs. Model downloads on first run to\n");
        out.push_str("# $XDG_DATA_HOME/tebis/models/ (about 148 MB for base.en).\n");
        if v.enabled {
            let _ = writeln!(out, "TELEGRAM_STT=on");
            let _ = writeln!(out, "TELEGRAM_STT_MODEL={}", v.model);
        } else {
            out.push_str("TELEGRAM_STT=off\n");
        }
    }

    if let Some(t) = tts {
        out.push_str(
            "\n# Voice replies (TTS). See PLAN-TTS-V2.md for the backend story.\n",
        );
        match t {
            TtsChoice::Off => {
                let _ = writeln!(out, "TELEGRAM_TTS_BACKEND=none");
            }
            TtsChoice::Say { voice, respond_to_all } => {
                out.push_str("# macOS `say` shell-out.\n");
                let _ = writeln!(out, "TELEGRAM_TTS_BACKEND=say");
                let _ = writeln!(out, "TELEGRAM_TTS_VOICE={voice}");
                if *respond_to_all {
                    out.push_str("TELEGRAM_TTS_RESPOND_TO_ALL=on\n");
                }
            }
            TtsChoice::KokoroLocal { model, voice, respond_to_all, ort_dylib_path } => {
                out.push_str(
                    "# Local Kokoro ONNX via espeak-ng phonemizer. Requires\n",
                );
                out.push_str("# the `kokoro` cargo feature at build time.\n");
                let _ = writeln!(out, "TELEGRAM_TTS_BACKEND=kokoro-local");
                let _ = writeln!(out, "TELEGRAM_TTS_MODEL={model}");
                let _ = writeln!(out, "TELEGRAM_TTS_VOICE={voice}");
                if let Some(p) = ort_dylib_path {
                    out.push_str(
                        "# Where the daemon's `libloading` finds the ONNX Runtime shared\n\
                         # library. On Apple Silicon brew installs to /opt/homebrew/lib,\n\
                         # which isn't in the default dyld search path.\n",
                    );
                    let _ = writeln!(out, "ORT_DYLIB_PATH={p}");
                }
                if *respond_to_all {
                    out.push_str("TELEGRAM_TTS_RESPOND_TO_ALL=on\n");
                }
            }
            TtsChoice::KokoroRemote {
                url,
                api_key,
                model,
                voice,
                timeout_sec,
                allow_http,
                respond_to_all,
            } => {
                out.push_str(
                    "# Remote OpenAI-compatible TTS endpoint (e.g. Kokoro-FastAPI).\n",
                );
                let _ = writeln!(out, "TELEGRAM_TTS_BACKEND=kokoro-remote");
                let _ = writeln!(out, "TELEGRAM_TTS_REMOTE_URL={url}");
                if let Some(k) = api_key
                    && !k.is_empty()
                {
                    let _ = writeln!(out, "TELEGRAM_TTS_REMOTE_API_KEY={k}");
                }
                let _ = writeln!(out, "TELEGRAM_TTS_REMOTE_MODEL={model}");
                let _ = writeln!(out, "TELEGRAM_TTS_VOICE={voice}");
                let _ = writeln!(out, "TELEGRAM_TTS_REMOTE_TIMEOUT_SEC={timeout_sec}");
                if *allow_http {
                    out.push_str("TELEGRAM_TTS_REMOTE_ALLOW_HTTP=on\n");
                }
                if *respond_to_all {
                    out.push_str("TELEGRAM_TTS_RESPOND_TO_ALL=on\n");
                }
            }
        }
    }

    out
}

/// Lines for keys NOT in the wizard-managed set — preserved so user-added
/// settings (`TELEGRAM_NOTIFY`, `TELEGRAM_AUTOREPLY`, …) survive re-runs.
/// Comments and blanks are dropped; the wizard emits its own headers.
fn extra_lines_to_preserve(env_path: &Path) -> Vec<String> {
    let Ok(content) = fs::read_to_string(env_path) else {
        return Vec::new();
    };
    let managed: HashSet<&str> = wizard_managed_keys().collect();
    content
        .lines()
        .filter_map(|line| match env_file::parse_kv_line(line) {
            Some((k, _)) if managed.contains(k) => None,
            Some(_) => Some(line.to_string()),
            None => None,
        })
        .collect()
}

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

/// Expand `~` / `~/…`, trim whitespace + trailing slashes.
fn normalize_dir(s: &str) -> String {
    let trimmed = s.trim().trim_end_matches('/');
    if trimmed == "~" {
        return crate::platform::paths::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| trimmed.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Ok(home) = crate::platform::paths::home_dir()
    {
        // `Path::join` → native separator: `/` on Unix, `\` on Windows.
        // Plain format!("{home}/{rest}") would mix separators when
        // `home` is `C:\Users\john` and `rest` is `projects\app`.
        return home.join(rest).to_string_lossy().into_owned();
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::env;

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

    /// Single test because `env::set_var` is process-wide. Unix-only: the
    /// assertions hard-code POSIX-style path formatting (`/Users/…`,
    /// forward slashes) that `Path::join` rewrites with `\` on Windows.
    #[cfg(unix)]
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
