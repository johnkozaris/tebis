//! Step 8 — voice replies (TTS). Simple vs Advanced picker; one configure_* fn per backend.

use anyhow::{Context, Result};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Select};

use super::super::{TtsChoice, ui};
use crate::platform::tts_support::{
    NativeTtsKind, kokoro_local_auto_install_supported, native_tts_kind,
};

pub(in crate::setup) fn step_tts(
    theme: &ColorfulTheme,
    existing: Option<&TtsChoice>,
) -> Result<Option<TtsChoice>> {
    ui::step_header(8, "Voice replies (optional)");
    println!("Synthesize text replies back as voice notes. Available backends:");
    match native_tts_kind() {
        Some(NativeTtsKind::Say) => println!(
            "  • {}: macOS built-in, zero install, modest quality",
            style("say").bold(),
        ),
        Some(NativeTtsKind::WinRt) => println!(
            "  • {}: Windows built-in WinRT SpeechSynthesizer, zero install, OneCore voices",
            style("winrt").bold(),
        ),
        None => {}
    }
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

/// macOS → `say`; Windows → WinRT; Linux → Kokoro local (auto-installs
/// espeak-ng). Any OS with no native backend + no auto-install path
/// (e.g. BSDs) falls through to a Kokoro-remote Y/N. Never dead-ends.
fn simple_tts(theme: &ColorfulTheme, existing: Option<&TtsChoice>) -> Result<Option<TtsChoice>> {
    let respond_to_all = existing.is_some_and(TtsChoice::respond_to_all);

    if let Some(kind) = native_tts_kind() {
        return simple_native(kind, existing, respond_to_all);
    }

    // Linux: try Kokoro-local auto-install (espeak-ng + onnxruntime).
    #[cfg(target_os = "linux")]
    {
        println!();
        println!(
            "  Using {} (offline, neural). Checking for {} + {}…",
            style("Kokoro local").bold(),
            style("espeak-ng").bold(),
            style("onnxruntime").bold(),
        );
        return kokoro_local_simple_flow(theme, existing, respond_to_all);
    }

    // Anything else (BSD, unknown) — no native, no auto-install path.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (theme, respond_to_all);
        debug_assert!(native_tts_kind().is_none());
        debug_assert!(!kokoro_local_auto_install_supported());
        simple_fallback_remote_or_off(theme, existing, respond_to_all)
    }
}

/// macOS `say` / Windows WinRT — both are zero-install so the Simple
/// flow just picks a default voice and is done.
fn simple_native(
    kind: NativeTtsKind,
    existing: Option<&TtsChoice>,
    respond_to_all: bool,
) -> Result<Option<TtsChoice>> {
    match kind {
        NativeTtsKind::Say => {
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
        NativeTtsKind::WinRt => {
            // WinRT picks an installed voice if we leave this empty —
            // matches "use system default" behavior most Windows users
            // expect from a zero-install flow.
            let voice = existing_voice_or(existing, "");
            println!();
            if voice.is_empty() {
                println!(
                    "  {} Windows {} with the system default voice.",
                    style("✓").green(),
                    style("WinRT SpeechSynthesizer").bold(),
                );
                println!(
                    "     Pick a specific voice (e.g. {}, {}) in {}.",
                    style("Zira").bold(),
                    style("David").bold(),
                    style("Advanced").bold(),
                );
            } else {
                println!(
                    "  {} Windows {} with voice substring {}.",
                    style("✓").green(),
                    style("WinRT SpeechSynthesizer").bold(),
                    style(&voice).bold(),
                );
            }
            Ok(Some(TtsChoice::WinRt {
                voice,
                respond_to_all,
            }))
        }
    }
}

/// Simple flow escape hatch for OSes without a native TTS or auto-install
/// path — offer Kokoro (remote) or Off; never panic.
#[cfg(not(target_os = "linux"))]
fn simple_fallback_remote_or_off(
    theme: &ColorfulTheme,
    existing: Option<&TtsChoice>,
    respond_to_all: bool,
) -> Result<Option<TtsChoice>> {
    println!();
    println!(
        "  {} No native or auto-installable TTS backend on this OS.",
        style("ℹ").cyan(),
    );
    println!(
        "  Choose {} for a network-hosted voice, or {} to stay text-only.",
        style("Advanced → Kokoro (remote)").bold(),
        style("Skip").bold(),
    );
    let want_remote = Confirm::with_theme(theme)
        .with_prompt("Configure Kokoro (remote) now?")
        .default(false)
        .interact()
        .context("prompt: simple fallback remote")?;
    if want_remote {
        return configure_kokoro_remote(theme, existing, respond_to_all);
    }
    Ok(Some(TtsChoice::Off))
}

/// Probe-and-install Kokoro-local dependencies, preserving existing config on
/// transient failure. Extracted so the Linux arm of [`simple_tts`] stays
/// readable after the per-OS cfg split.
#[cfg(target_os = "linux")]
fn kokoro_local_simple_flow(
    theme: &ColorfulTheme,
    existing: Option<&TtsChoice>,
    respond_to_all: bool,
) -> Result<Option<TtsChoice>> {
    let espeak = super::super::phonemizer::ensure_or_install(theme)
        .context("probing / installing espeak-ng")?;
    let ort = super::super::onnxruntime::ensure_or_install(theme)
        .context("probing / installing onnxruntime")?;
    match (espeak, ort) {
        (
            super::super::phonemizer::EnsureOutcome::Ready(_),
            super::super::onnxruntime::EnsureOutcome::Ready(ort_path),
        ) => {
            let model = default_tts_model();
            let voice = existing_voice_or(existing, "af_sarah");
            Ok(Some(TtsChoice::KokoroLocal {
                model,
                voice,
                respond_to_all,
                ort_dylib_path: Some(ort_path.to_string_lossy().into_owned()),
            }))
        }
        _ => {
            println!();
            println!(
                "   {} Continuing with text-only replies. Re-run {} later",
                style("→").dim(),
                style("tebis setup").bold(),
            );
            println!("     after installing the missing dependency to enable voice replies.");
            Ok(Some(TtsChoice::Off))
        }
    }
}

fn advanced_tts(theme: &ColorfulTheme, existing: Option<&TtsChoice>) -> Result<Option<TtsChoice>> {
    #[derive(Clone, Copy)]
    enum Pick {
        Native(NativeTtsKind),
        KokoroLocal,
        KokoroRemote,
        None,
    }

    // Build the catalog based on capabilities, not hardcoded cfg —
    // this is the OCP seam: adding a backend means adding an entry,
    // not editing 7 files.
    let mut options: Vec<(&'static str, Pick)> = Vec::new();
    if let Some(kind) = native_tts_kind() {
        options.push((
            match kind {
                NativeTtsKind::Say => "Say (macOS)       — built-in, modest quality",
                NativeTtsKind::WinRt => "WinRT (Windows)   — built-in, OneCore voices",
            },
            Pick::Native(kind),
        ));
    }
    options.push((
        "Kokoro (local)    — neural, offline, needs espeak-ng",
        Pick::KokoroLocal,
    ));
    options.push((
        "Kokoro (remote)   — HTTP endpoint (Kokoro-FastAPI, etc.)",
        Pick::KokoroRemote,
    ));
    options.push(("None              — text-only replies", Pick::None));

    let default_idx: usize = match existing {
        Some(TtsChoice::Say { .. }) if matches!(native_tts_kind(), Some(NativeTtsKind::Say)) => 0,
        Some(TtsChoice::WinRt { .. })
            if matches!(native_tts_kind(), Some(NativeTtsKind::WinRt)) =>
        {
            0
        }
        Some(TtsChoice::KokoroLocal { .. }) => {
            if native_tts_kind().is_some() {
                1
            } else {
                0
            }
        }
        Some(TtsChoice::KokoroRemote { .. }) => {
            if native_tts_kind().is_some() {
                2
            } else {
                1
            }
        }
        Some(TtsChoice::Off) | None => options.len().saturating_sub(1),
        // Unreachable on a given host (e.g. `Say` on Windows), fall to "None".
        Some(_) => options.len().saturating_sub(1),
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
        Pick::Native(kind) => configure_native(theme, kind, existing, respond_to_all_default),
        Pick::KokoroLocal => configure_kokoro_local(theme, existing, respond_to_all_default),
        Pick::KokoroRemote => configure_kokoro_remote(theme, existing, respond_to_all_default),
    }
}

fn configure_native(
    theme: &ColorfulTheme,
    kind: NativeTtsKind,
    existing: Option<&TtsChoice>,
    respond_to_all_default: bool,
) -> Result<Option<TtsChoice>> {
    match kind {
        NativeTtsKind::Say => {
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
        NativeTtsKind::WinRt => {
            println!();
            println!(
                "  {}: substring match on installed voice. Common names: \
                 {}, {}, {} (US English); {} (UK English).",
                style("Voice").dim(),
                style("Zira").bold(),
                style("David").bold(),
                style("Mark").bold(),
                style("Hazel").bold(),
            );
            println!(
                "  Leave empty for the system default. Win11 \"Natural\" voices \
                 (Aria/Jenny/Guy) are Narrator-only and {}.",
                style("not exposed to third-party apps").dim(),
            );
            let voice: String = Input::with_theme(theme)
                .with_prompt("Voice substring (empty = default)")
                .default(existing_voice_or(existing, ""))
                .allow_empty(true)
                .interact_text()
                .context("prompt: winrt voice")?;
            let respond_to_all = Confirm::with_theme(theme)
                .with_prompt("Voice-reply to typed messages too?")
                .default(respond_to_all_default)
                .interact()
                .context("prompt: winrt respond_to_all")?;
            Ok(Some(TtsChoice::WinRt {
                voice: voice.trim().to_string(),
                respond_to_all,
            }))
        }
    }
}

fn configure_kokoro_local(
    theme: &ColorfulTheme,
    existing: Option<&TtsChoice>,
    respond_to_all_default: bool,
) -> Result<Option<TtsChoice>> {
    println!();
    println!(
        "Kokoro local requires {} on PATH and {} shared library.",
        style("espeak-ng").bold(),
        style("onnxruntime").bold(),
    );
    if !kokoro_local_auto_install_supported() {
        println!(
            "   {} Auto-install not wired on this OS. {}:",
            style("⚠").yellow(),
            style("Install manually").bold(),
        );
        println!("     • espeak-ng: add to PATH");
        println!(
            "     • onnxruntime: set {} to the DLL/dylib/so path",
            style("ORT_DYLIB_PATH").bold()
        );
        println!("   The daemon will start if these env vars point at valid binaries.");
    }
    let espeak = super::super::phonemizer::ensure_or_install(theme)
        .context("probing / installing espeak-ng")?;
    if !matches!(espeak, super::super::phonemizer::EnsureOutcome::Ready(_)) {
        println!();
        println!(
            "   {} espeak-ng unavailable — disabling Kokoro-local TTS.",
            style("→").dim(),
        );
        return Ok(Some(TtsChoice::Off));
    }

    let ort_outcome = super::super::onnxruntime::ensure_or_install(theme)
        .context("probing / installing onnxruntime")?;
    let ort_path = match ort_outcome {
        super::super::onnxruntime::EnsureOutcome::Ready(p) => p,
        _ => {
            println!();
            println!(
                "   {} onnxruntime unavailable — disabling Kokoro-local TTS.",
                style("→").dim(),
            );
            return Ok(Some(TtsChoice::Off));
        }
    };

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
        ort_dylib_path: Some(ort_path.to_string_lossy().into_owned()),
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

/// Manifest default with a literal fallback; daemon re-validates at startup.
fn default_tts_model() -> String {
    crate::audio::manifest::get()
        .default_tts_model()
        .unwrap_or("kokoro-v1.0")
        .to_string()
}
