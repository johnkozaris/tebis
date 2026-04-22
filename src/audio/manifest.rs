//! STT/TTS asset manifest — `include_str!` at build time (never fetched).
//! Placeholder SHAs (`TBD-PLACEHOLDER-…`) are rejected by `validate_*_usable`.

use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub manifest_version: u32,
    pub tebis_version: String,
    pub updated_at: String,
    pub stt_models: std::collections::BTreeMap<String, SttModel>,
    pub tts_models: std::collections::BTreeMap<String, TtsModel>,
}

#[derive(Debug, Deserialize)]
pub struct SttModel {
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub display_name: String,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Deserialize)]
pub struct TtsModel {
    pub onnx_url: String,
    pub onnx_sha256: String,
    pub onnx_size_bytes: u64,
    /// Per-voice `.bin` style embedding; lazy-fetched on selection.
    pub voices: std::collections::BTreeMap<String, VoiceAsset>,
    pub default_voice: String,
    pub display_name: String,
    // Kokoro's `tokenizer.json` is hardcoded in `audio::tts::kokoro::tokens` — not fetched.
}

#[derive(Debug, Deserialize)]
pub struct VoiceAsset {
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
}

const PLACEHOLDER_PREFIX: &str = "TBD-PLACEHOLDER-";

const EMBEDDED: &str = include_str!("manifest.json");

/// Parse once; panic on malformed embedded JSON — that's a build-time bug.
pub fn get() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        serde_json::from_str(EMBEDDED).expect("embedded manifest.json must parse — build-time bug")
    })
}

impl Manifest {
    pub fn stt_model(&self, name: &str) -> Result<&SttModel> {
        self.stt_models
            .get(name)
            .with_context(|| format!("unknown STT model `{name}`"))
    }

    pub fn tts_model(&self, name: &str) -> Result<&TtsModel> {
        self.tts_models
            .get(name)
            .with_context(|| format!("unknown TTS model `{name}`"))
    }

    pub fn default_stt_model(&self) -> Result<&str> {
        self.stt_models
            .iter()
            .find(|(_, m)| m.default)
            .map(|(k, _)| k.as_str())
            .context("manifest has no default STT model")
    }

    /// First declared TTS model. Revisit if multiple TTS models ship.
    pub fn default_tts_model(&self) -> Result<&str> {
        self.tts_models
            .keys()
            .next()
            .map(String::as_str)
            .context("manifest has no TTS models")
    }

    /// Refuses assets with placeholder SHAs.
    pub fn validate_stt_usable(&self, name: &str) -> Result<()> {
        let m = self.stt_model(name)?;
        if m.sha256.starts_with(PLACEHOLDER_PREFIX) {
            bail!(
                "STT model `{name}` has placeholder SHA — pin the real hash in \
                 src/audio/manifest.json (run `scripts/pin-model-shas.sh --apply`) \
                 before enabling local STT."
            );
        }
        Ok(())
    }

    /// Validates ONNX + default voice. Other voices validated on selection.
    pub fn validate_tts_usable(&self, name: &str) -> Result<()> {
        let m = self.tts_model(name)?;
        if m.onnx_sha256.starts_with(PLACEHOLDER_PREFIX) {
            bail!(
                "TTS model `{name}` has placeholder ONNX SHA — pin via \
                 scripts/pin-model-shas.sh --apply"
            );
        }
        let default_voice = m.voices.get(&m.default_voice).with_context(|| {
            format!(
                "TTS model `{name}` default_voice `{}` not declared in voices map",
                m.default_voice
            )
        })?;
        if default_voice.sha256.starts_with(PLACEHOLDER_PREFIX) {
            bail!(
                "TTS voice `{}` (the default) has placeholder SHA — pin via \
                 scripts/pin-model-shas.sh --apply",
                m.default_voice
            );
        }
        Ok(())
    }

    pub fn validate_voice(&self, model: &str, voice: &str) -> Result<&VoiceAsset> {
        let m = self.tts_model(model)?;
        let asset = m.voices.get(voice).with_context(|| {
            format!(
                "unknown TTS voice `{voice}` — manifest has: {:?}",
                m.voices.keys().collect::<Vec<_>>()
            )
        })?;
        if asset.sha256.starts_with(PLACEHOLDER_PREFIX) {
            bail!(
                "TTS voice `{voice}` has placeholder SHA — pin via \
                 scripts/pin-model-shas.sh --apply"
            );
        }
        Ok(asset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_parses() {
        let m = get();
        assert_eq!(m.manifest_version, 2);
        assert!(!m.stt_models.is_empty());
        assert!(!m.tts_models.is_empty());
    }

    #[test]
    fn every_stt_model_has_nonempty_url_and_sha() {
        for (name, m) in &get().stt_models {
            assert!(!m.url.is_empty(), "STT `{name}` has empty URL");
            assert!(!m.sha256.is_empty(), "STT `{name}` has empty SHA");
            assert!(m.size_bytes > 0, "STT `{name}` has zero size");
            assert!(
                m.url.starts_with("https://"),
                "STT `{name}` URL must be https"
            );
        }
    }

    #[test]
    fn every_tts_model_has_nonempty_urls_and_shas() {
        for (name, m) in &get().tts_models {
            assert!(!m.onnx_url.is_empty(), "TTS `{name}` has empty onnx URL");
            assert!(!m.onnx_sha256.is_empty());
            assert!(m.onnx_size_bytes > 0);
            assert!(
                !m.voices.is_empty(),
                "TTS `{name}` declares no voices"
            );
            assert!(
                m.voices.contains_key(&m.default_voice),
                "TTS `{name}` default_voice `{}` not in voices map",
                m.default_voice
            );
            for (v_name, v) in &m.voices {
                assert!(
                    !v.url.is_empty(),
                    "TTS `{name}` voice `{v_name}` has empty URL"
                );
                assert!(!v.sha256.is_empty());
                assert!(v.size_bytes > 0);
            }
        }
    }

    #[test]
    fn default_stt_model_is_set() {
        assert!(get().default_stt_model().is_ok());
    }

    #[test]
    fn default_tts_model_is_set() {
        assert!(get().default_tts_model().is_ok());
    }

    #[test]
    fn lookup_unknown_stt_errors() {
        assert!(get().stt_model("nonesuch").is_err());
    }

    #[test]
    fn validate_stt_usable_accepts_pinned_sha() {
        let default = get().default_stt_model().unwrap();
        assert!(get().validate_stt_usable(default).is_ok());
    }

    #[test]
    fn placeholder_prefix_is_rejected_by_convention() {
        assert!(
            PLACEHOLDER_PREFIX.starts_with("TBD-"),
            "placeholder convention drifted — validate_stt_usable relies on starts_with"
        );
        let placeholder = format!("{PLACEHOLDER_PREFIX}future-model");
        assert!(placeholder.starts_with(PLACEHOLDER_PREFIX));
    }
}
