//! Step 8 — voice replies (TTS) with Simple/Advanced picker.
//!
//! The three backends tebis supports — `say` (macOS), `kokoro-local`
//! (offline neural), `kokoro-remote` (HTTP) — each have their own
//! configure function so this file stays a flat list of concerns.
//! The top-level `step_tts` just dispatches to Simple vs Advanced.

use anyhow::{Context, Result};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Select};

use super::super::{TtsChoice, ui};

pub(in crate::setup) fn step_tts(
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

/// Simple = lightest backend that works out of the box on this host.
/// macOS → `say`, Linux → Kokoro local after auto-installing
/// `espeak-ng`. Any install failure falls through to `Off`.
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
        let outcome = super::super::phonemizer::ensure_or_install(theme)
            .context("probing / installing espeak-ng")?;
        match outcome {
            super::super::phonemizer::EnsureOutcome::Ready(_) => {
                let model = default_tts_model();
                let voice = existing_voice_or(existing, "af_sarah");
                Ok(Some(TtsChoice::KokoroLocal {
                    model,
                    voice,
                    respond_to_all,
                }))
            }
            super::super::phonemizer::EnsureOutcome::UserDeclined
            | super::super::phonemizer::EnsureOutcome::InstallFailed
            | super::super::phonemizer::EnsureOutcome::NoPackageManager => {
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

/// Advanced = 4-way backend picker + per-backend config.
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

    // `Say` is cfg-gated — a flat `vec![]` would need two separate
    // macro calls with the same trailing `None` entry.
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
        v.push(("Say (macOS only)  — built-in, lower quality", Pick::Say));
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
    // The Advanced UI hides `Say` on non-macOS, so this branch should
    // never be hit from a user flow. Degrade gracefully rather than
    // panicking in case future refactors change the UI — a wizard
    // panic is exactly the kind of failure mode to avoid.
    tracing::warn!(
        "configure_say invoked on non-macOS; falling back to TTS off"
    );
    Ok(Some(TtsChoice::Off))
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
    let outcome = super::super::phonemizer::ensure_or_install(theme)
        .context("probing / installing espeak-ng")?;
    if !matches!(outcome, super::super::phonemizer::EnsureOutcome::Ready(_)) {
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
    // Trim surrounding whitespace before the empty check and before
    // persisting. A trailing space in the env file would otherwise be
    // carried into the Bearer header and silently break auth; surrounded
    // whitespace from paste is an easy mistake to miss.
    let api_key_trimmed = api_key_raw.trim();
    let api_key = if api_key_trimmed.is_empty() {
        None
    } else {
        Some(api_key_trimmed.to_string())
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

/// Current manifest's default TTS model key, with a literal fallback
/// so a wizard run in a repo where the manifest was temporarily
/// cleared still returns a sensible value — the daemon validates
/// properly at startup and surfaces a clear error if the key doesn't
/// resolve.
fn default_tts_model() -> String {
    crate::audio::manifest::get()
        .default_tts_model()
        .unwrap_or("kokoro-v1.0")
        .to_string()
}
