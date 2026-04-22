//! Step 7 — voice input (STT) enable + model picker.

use anyhow::{Context, Result};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Select};

use super::super::{VoiceChoice, ui};
use crate::audio;

pub(in crate::setup) fn step_voice(
    theme: &ColorfulTheme,
    existing: Option<&VoiceChoice>,
) -> Result<Option<VoiceChoice>> {
    ui::step_header(7, "Voice input (optional)");
    println!(
        "Transcribe Telegram {} messages in-process with {} —",
        style("voice").cyan().bold(),
        style("whisper.cpp").bold(),
    );
    println!("no cloud round-trip, no extra service. The model downloads on");
    println!(
        "first run to {} (about {} for {}).",
        style("$XDG_DATA_HOME/tebis/models/").dim(),
        style("148 MB").bold(),
        style("base.en").bold(),
    );
    println!();

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

    let manifest = audio::manifest::get();
    let mut choices: Vec<(String, String)> = manifest
        .stt_models
        .iter()
        .map(|(k, m)| (k.clone(), m.display_name.clone()))
        .collect();
    // Default model first so Enter picks it.
    choices.sort_by_key(|(k, _)| !manifest.stt_models.get(k).is_some_and(|m| m.default));

    let existing_key = existing.and_then(|v| {
        (!v.model.is_empty() && manifest.stt_models.contains_key(&v.model))
            .then_some(v.model.clone())
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
