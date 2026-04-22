//! Interactive wizard steps + input validators.

use std::path::Path;

use anyhow::{Context, Result};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Select};

use super::{Autostart, HooksChoice, TtsChoice, VoiceChoice, normalize_dir, ui};
use crate::agent_hooks::AgentKind;
use crate::audio;
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

/// Voice / STT — Phase 1 exposes only the `local` provider. The cloud
/// providers live in config but aren't user-selectable from the wizard
/// yet (they want API keys and a cleaner interactive flow; deferred to
/// Phase 2 to avoid a half-finished story).
pub(super) fn step_voice(
    theme: &ColorfulTheme,
    existing: Option<&VoiceChoice>,
) -> Result<Option<VoiceChoice>> {
    ui::step_header(7, "Voice input (optional)");
    println!(
        "Transcribe Telegram {} messages in-process with {} —",
        style("voice").cyan().bold(),
        style("whisper.cpp").bold(),
    );
    println!(
        "no cloud round-trip, no extra service. The model downloads on",
    );
    println!(
        "first run to {} (about {} for {}).",
        style("$XDG_DATA_HOME/tebis/models/").dim(),
        style("148 MB").bold(),
        style("base.en").bold(),
    );
    println!();

    // Default to on for users who re-run the wizard and had it enabled;
    // default off for first-run (matches public-release posture).
    let default_on = existing.is_some_and(|v| v.enabled);
    let enabled = Confirm::with_theme(theme)
        .with_prompt("Enable voice-to-text for voice messages?")
        .default(default_on)
        .interact()
        .context("prompt: enable voice")?;

    if !enabled {
        return Ok(Some(VoiceChoice {
            enabled: false,
            model: String::new(),
        }));
    }

    // Pull labeled options from the manifest so upstream model additions
    // don't require a wizard change.
    let manifest = audio::manifest::get();
    let mut choices: Vec<(String, String)> = manifest
        .stt_models
        .iter()
        .map(|(k, m)| (k.clone(), m.display_name.clone()))
        .collect();
    // Show the default first so hitting Enter picks the 148 MB base.en.
    choices.sort_by_key(|(k, _)| {
        !manifest
            .stt_models
            .get(k)
            .is_some_and(|m| m.default)
    });

    let existing_key = existing.and_then(|v| {
        (!v.model.is_empty() && manifest.stt_models.contains_key(&v.model)).then_some(v.model.clone())
    });
    let default_idx = existing_key
        .as_deref()
        .and_then(|k| choices.iter().position(|(ck, _)| ck == k))
        .unwrap_or(0);

    let labels: Vec<&str> = choices.iter().map(|(_, d)| d.as_str()).collect();
    let picked = Select::with_theme(theme)
        .with_prompt("Whisper model")
        .items(labels.as_slice())
        .default(default_idx)
        .interact()
        .context("prompt: whisper model")?;

    Ok(Some(VoiceChoice {
        enabled: true,
        model: choices[picked].0.clone(),
    }))
}

/// Voice replies (TTS) — cross-platform Simple/Advanced picker.
///
/// Top-level choice:
/// - **Simple** defaults to `say` on macOS (zero install) and Kokoro
///   local on Linux (after ensuring `espeak-ng`).
/// - **Advanced** exposes the full 4-way picker (Kokoro local /
///   Kokoro remote / Say [macOS] / None) with per-backend config.
/// - **Skip** writes `TELEGRAM_TTS_BACKEND=none`.
///
/// `existing` is the previously-saved `TtsChoice` when the wizard is
/// re-run; the chosen flow pre-fills defaults from it.
pub(super) fn step_tts(
    theme: &ColorfulTheme,
    existing: Option<&TtsChoice>,
) -> Result<Option<TtsChoice>> {
    ui::step_header(8, "Voice replies (optional)");
    println!("Synthesize text replies back as voice notes. Three backends:");
    println!(
        "  • {}: macOS-native, zero install, lower quality",
        style("say").bold(),
    );
    println!(
        "  • {}: neural ONNX + {}, cross-platform (feature-gated)",
        style("kokoro-local").bold(),
        style("espeak-ng").bold(),
    );
    println!(
        "  • {}: HTTP endpoint (Kokoro-FastAPI, or any OpenAI-compatible server)",
        style("kokoro-remote").bold(),
    );
    println!();
    println!(
        "{}: by default only inbound voice messages trigger a voice reply.",
        style("Note").dim(),
    );
    println!();

    let items = [
        "Simple   — platform-appropriate defaults",
        "Advanced — pick backend + full config",
        "Skip     — text-only replies",
    ];
    let default_idx: usize = match existing {
        None => 0,
        Some(TtsChoice::Off) => 2,
        Some(_) => 1,
    };
    let choice = Select::with_theme(theme)
        .with_prompt("Setup mode")
        .items(items.as_slice())
        .default(default_idx)
        .interact()
        .context("prompt: tts setup mode")?;

    match choice {
        0 => simple_tts(theme, existing),
        1 => advanced_tts(theme, existing),
        _ => Ok(Some(TtsChoice::Off)),
    }
}

/// Simple path: pick the lightest backend that works on this host.
///
/// - macOS → `say` with the existing or `Samantha` voice.
/// - Linux → Kokoro local with `af_sarah`, after probing / installing
///   `espeak-ng`. If espeak-ng isn't available (user declined, no
///   pkg manager), fall through to `Off` with a clear message.
fn simple_tts(
    theme: &ColorfulTheme,
    existing: Option<&TtsChoice>,
) -> Result<Option<TtsChoice>> {
    let respond_to_all = existing.is_some_and(TtsChoice::respond_to_all);

    #[cfg(target_os = "macos")]
    {
        let _ = theme;
        let voice = existing_voice_or(existing, "Samantha");
        println!();
        println!(
            "  {} macOS {} with voice {}.",
            style("✓").green(),
            style("say").bold(),
            style(&voice).bold(),
        );
        Ok(Some(TtsChoice::Say {
            voice,
            respond_to_all,
        }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        println!();
        println!(
            "  Using {} (offline, neural). Checking for {}…",
            style("Kokoro local").bold(),
            style("espeak-ng").bold(),
        );
        let outcome = super::phonemizer::ensure_or_install(theme)
            .context("probing / installing espeak-ng")?;
        match outcome {
            super::phonemizer::EnsureOutcome::Ready(_) => {
                let model = default_tts_model();
                let voice = existing_voice_or(existing, "af_sarah");
                Ok(Some(TtsChoice::KokoroLocal {
                    model,
                    voice,
                    respond_to_all,
                }))
            }
            super::phonemizer::EnsureOutcome::UserDeclined
            | super::phonemizer::EnsureOutcome::InstallFailed
            | super::phonemizer::EnsureOutcome::NoPackageManager => {
                println!();
                println!(
                    "   {} Continuing with text-only replies. Re-run {} later",
                    style("→").dim(),
                    style("tebis setup").bold(),
                );
                println!("     after installing espeak-ng to enable voice replies.");
                Ok(Some(TtsChoice::Off))
            }
        }
    }
}

/// Advanced path: full backend picker + per-backend config.
fn advanced_tts(
    theme: &ColorfulTheme,
    existing: Option<&TtsChoice>,
) -> Result<Option<TtsChoice>> {
    #[derive(Clone, Copy)]
    enum Pick {
        KokoroLocal,
        KokoroRemote,
        Say,
        None,
    }

    // Options list is built incrementally because the `Say` variant is
    // cfg-gated — a pure `vec![]` would need two separate macro calls
    // with the same trailing `None` entry, which is noisier than this.
    #[allow(
        clippy::vec_init_then_push,
        reason = "cfg-gated middle entry rules out a flat vec![] literal"
    )]
    let options: Vec<(&'static str, Pick)> = {
        let mut v = vec![
            (
                "Kokoro (local)    — neural, offline, needs espeak-ng",
                Pick::KokoroLocal,
            ),
            (
                "Kokoro (remote)   — HTTP endpoint (Kokoro-FastAPI, etc.)",
                Pick::KokoroRemote,
            ),
        ];
        #[cfg(target_os = "macos")]
        v.push((
            "Say (macOS only)  — built-in, lower quality",
            Pick::Say,
        ));
        v.push(("None              — text-only replies", Pick::None));
        v
    };

    let default_idx: usize = match existing {
        Some(TtsChoice::KokoroLocal { .. }) => 0,
        Some(TtsChoice::KokoroRemote { .. }) => 1,
        #[cfg(target_os = "macos")]
        Some(TtsChoice::Say { .. }) => 2,
        Some(TtsChoice::Off) | None => options.len().saturating_sub(1),
        #[cfg(not(target_os = "macos"))]
        Some(TtsChoice::Say { .. }) => 0,
    };

    let labels: Vec<&str> = options.iter().map(|(l, _)| *l).collect();
    let idx = Select::with_theme(theme)
        .with_prompt("Backend")
        .items(labels.as_slice())
        .default(default_idx)
        .interact()
        .context("prompt: tts backend")?;
    let pick = options[idx].1;

    let respond_to_all_default = existing.is_some_and(TtsChoice::respond_to_all);

    match pick {
        Pick::None => Ok(Some(TtsChoice::Off)),
        Pick::Say => configure_say(theme, existing, respond_to_all_default),
        Pick::KokoroLocal => configure_kokoro_local(theme, existing, respond_to_all_default),
        Pick::KokoroRemote => configure_kokoro_remote(theme, existing, respond_to_all_default),
    }
}

#[cfg(target_os = "macos")]
fn configure_say(
    theme: &ColorfulTheme,
    existing: Option<&TtsChoice>,
    respond_to_all_default: bool,
) -> Result<Option<TtsChoice>> {
    let voice: String = Input::with_theme(theme)
        .with_prompt("`say` voice (e.g. Samantha, Alex, Ava (Premium))")
        .default(existing_voice_or(existing, "Samantha"))
        .interact_text()
        .context("prompt: say voice")?;
    let respond_to_all = Confirm::with_theme(theme)
        .with_prompt("Voice-reply to typed messages too?")
        .default(respond_to_all_default)
        .interact()
        .context("prompt: say respond_to_all")?;
    Ok(Some(TtsChoice::Say {
        voice,
        respond_to_all,
    }))
}

#[cfg(not(target_os = "macos"))]
fn configure_say(
    _theme: &ColorfulTheme,
    _existing: Option<&TtsChoice>,
    _respond_to_all_default: bool,
) -> Result<Option<TtsChoice>> {
    // `Say` is hidden on non-macOS; this branch is unreachable from the
    // UI, but the function must exist so the match in `advanced_tts`
    // type-checks on every platform.
    unreachable!("Say backend is macOS-only — UI should not offer it here")
}

fn configure_kokoro_local(
    theme: &ColorfulTheme,
    existing: Option<&TtsChoice>,
    respond_to_all_default: bool,
) -> Result<Option<TtsChoice>> {
    println!();
    println!(
        "Kokoro local requires {} on PATH.",
        style("espeak-ng").bold(),
    );
    let outcome = super::phonemizer::ensure_or_install(theme)
        .context("probing / installing espeak-ng")?;
    if !matches!(outcome, super::phonemizer::EnsureOutcome::Ready(_)) {
        println!();
        println!(
            "   {} espeak-ng unavailable — falling back to no TTS.",
            style("→").dim(),
        );
        return Ok(Some(TtsChoice::Off));
    }
    let model = default_tts_model();
    let voice: String = Input::with_theme(theme)
        .with_prompt("Voice (af_sarah or am_adam)")
        .default(existing_voice_or(existing, "af_sarah"))
        .interact_text()
        .context("prompt: kokoro voice")?;
    let respond_to_all = Confirm::with_theme(theme)
        .with_prompt("Voice-reply to typed messages too?")
        .default(respond_to_all_default)
        .interact()
        .context("prompt: kokoro respond_to_all")?;
    Ok(Some(TtsChoice::KokoroLocal {
        model,
        voice,
        respond_to_all,
    }))
}

fn configure_kokoro_remote(
    theme: &ColorfulTheme,
    existing: Option<&TtsChoice>,
    respond_to_all_default: bool,
) -> Result<Option<TtsChoice>> {
    let existing_remote = existing.and_then(remote_fields);
    let default_url = existing_remote
        .as_ref()
        .map_or_else(String::new, |r| r.url.clone());
    let url: String = Input::with_theme(theme)
        .with_prompt("Base URL (e.g. https://kokoro.example.com)")
        .default(default_url)
        .validate_with(|s: &String| -> std::result::Result<(), &'static str> {
            let lower = s.trim().to_ascii_lowercase();
            if lower.starts_with("https://") || lower.starts_with("http://") {
                Ok(())
            } else {
                Err("must start with https:// (or http:// for LAN)")
            }
        })
        .interact_text()
        .context("prompt: remote url")?;
    let allow_http = url.trim().to_ascii_lowercase().starts_with("http://");
    if allow_http {
        println!(
            "   {} {}  (encrypted transit recommended for anything but LAN)",
            style("⚠").yellow(),
            style("HTTP URL — TELEGRAM_TTS_REMOTE_ALLOW_HTTP will be set").yellow(),
        );
    }

    let default_key = existing_remote
        .as_ref()
        .and_then(|r| r.api_key.clone())
        .unwrap_or_default();
    let api_key_raw: String = Input::with_theme(theme)
        .with_prompt("Bearer API key (optional — press Enter for none)")
        .default(default_key)
        .allow_empty(true)
        .interact_text()
        .context("prompt: remote api key")?;
    let api_key = if api_key_raw.is_empty() {
        None
    } else {
        Some(api_key_raw)
    };

    let default_model = existing_remote
        .as_ref()
        .map_or_else(|| "kokoro".to_string(), |r| r.model.clone());
    let model: String = Input::with_theme(theme)
        .with_prompt("Model parameter")
        .default(default_model)
        .interact_text()
        .context("prompt: remote model")?;

    let default_voice = existing_remote
        .as_ref()
        .map_or_else(|| "af_sarah".to_string(), |r| r.voice.clone());
    let voice: String = Input::with_theme(theme)
        .with_prompt("Voice parameter (e.g. af_sarah)")
        .default(default_voice)
        .interact_text()
        .context("prompt: remote voice")?;

    let default_timeout = existing_remote.as_ref().map_or(10, |r| r.timeout_sec);
    let timeout_sec: u32 = Input::with_theme(theme)
        .with_prompt("Request timeout in seconds (1..=300)")
        .default(default_timeout)
        .validate_with(|n: &u32| -> std::result::Result<(), &'static str> {
            if (1..=300).contains(n) {
                Ok(())
            } else {
                Err("must be 1..=300")
            }
        })
        .interact_text()
        .context("prompt: remote timeout")?;

    let respond_to_all = Confirm::with_theme(theme)
        .with_prompt("Voice-reply to typed messages too?")
        .default(respond_to_all_default)
        .interact()
        .context("prompt: remote respond_to_all")?;

    Ok(Some(TtsChoice::KokoroRemote {
        url: url.trim().to_string(),
        api_key,
        model,
        voice,
        timeout_sec,
        allow_http,
        respond_to_all,
    }))
}

#[derive(Clone)]
struct RemoteFields {
    url: String,
    api_key: Option<String>,
    model: String,
    voice: String,
    timeout_sec: u32,
}

fn remote_fields(existing: &TtsChoice) -> Option<RemoteFields> {
    match existing {
        TtsChoice::KokoroRemote {
            url,
            api_key,
            model,
            voice,
            timeout_sec,
            ..
        } => Some(RemoteFields {
            url: url.clone(),
            api_key: api_key.clone(),
            model: model.clone(),
            voice: voice.clone(),
            timeout_sec: *timeout_sec,
        }),
        _ => None,
    }
}

fn existing_voice_or(existing: Option<&TtsChoice>, default: &str) -> String {
    match existing {
        Some(t) if !t.voice_display().is_empty() => t.voice_display().to_string(),
        _ => default.to_string(),
    }
}

/// Current manifest's default TTS model key. Falls back to a literal
/// so a wizard run in a repo where the manifest was temporarily cleared
/// still returns a reasonable value; the daemon validates properly at
/// startup and surfaces a clear error if the key doesn't resolve.
fn default_tts_model() -> String {
    crate::audio::manifest::get()
        .default_tts_model()
        .unwrap_or("kokoro-v1.0")
        .to_string()
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
