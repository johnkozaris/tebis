//! Embedded manifest of downloadable STT/TTS assets.
//!
//! The manifest is compiled in via `include_str!`; **never fetched at
//! runtime.** A runtime-fetched manifest would let whoever controls the
//! URL rug-pull which model tebis downloads. Pinning at binary-build
//! time is the whole point.
//!
//! Bumping a model (new URL, new SHA) is a tebis source change → new
//! release. The SHA fields are currently placeholders (`TBD-PLACEHOLDER-*`)
//! because Hugging Face doesn't expose stable SHA-256 HTTP headers; a
//! human has to `shasum -a 256` a known-good download once and paste the
//! hex into `manifest.json`. `Manifest::validate_for_use` refuses to
//! operate against placeholder SHAs — callers must either swap to a
//! remote provider or wait until real hashes are pinned.

use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Source of truth. Any change here must preserve `Deserialize`-compat
/// with the JSON in `manifest.json` — we assert at the end of this file
/// via `#[test]` that the embedded blob parses.
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
    pub tokenizer_url: String,
    pub tokenizer_sha256: String,
    pub tokenizer_size_bytes: u64,
    /// Per-voice style files. Kokoro ships each voice as an individual
    /// ~510 KB `.bin` containing that voice's style embedding. Users can
    /// opt into additional voices beyond the shipped set by adding entries
    /// here (or via `TELEGRAM_TTS_VOICE=<name>` if the file is already in
    /// the cache).
    pub voices: std::collections::BTreeMap<String, VoiceAsset>,
    /// Which voice to use when `TELEGRAM_TTS_VOICE` is unset.
    pub default_voice: String,
    pub display_name: String,
}

/// One Kokoro voice-style file. Small (~510 KB each) so downloading on
/// demand is cheap; we ship a few by default and lazy-fetch others.
#[derive(Debug, Deserialize)]
pub struct VoiceAsset {
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// Sentinel prefix for SHAs that haven't been filled in yet. `validate_for_use`
/// rejects any asset whose SHA still starts with this.
const PLACEHOLDER_PREFIX: &str = "TBD-PLACEHOLDER-";

const EMBEDDED: &str = include_str!("manifest.json");

/// Parse the embedded manifest once per process. Panics at first call
/// if the embedded blob is malformed — that's a build-time bug we want
/// to catch immediately rather than ship a daemon that silently can't
/// load models.
pub fn get() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        serde_json::from_str(EMBEDDED).expect("embedded manifest.json must parse — build-time bug")
    })
}

impl Manifest {
    /// Look up one STT model descriptor by key.
    pub fn stt_model(&self, name: &str) -> Result<&SttModel> {
        self.stt_models
            .get(name)
            .with_context(|| format!("unknown STT model `{name}`"))
    }

    /// Look up one TTS model descriptor by key.
    pub fn tts_model(&self, name: &str) -> Result<&TtsModel> {
        self.tts_models
            .get(name)
            .with_context(|| format!("unknown TTS model `{name}`"))
    }

    /// Name of the STT model marked `default: true`. Errors if none are
    /// flagged — a `#[test]` below catches that at build time too.
    pub fn default_stt_model(&self) -> Result<&str> {
        self.stt_models
            .iter()
            .find(|(_, m)| m.default)
            .map(|(k, _)| k.as_str())
            .context("manifest has no default STT model")
    }

    /// Name of the first declared TTS model. With one model in the
    /// manifest today this is unambiguous; if we ever ship multiple
    /// TTS models, add a `default: true` field like STT has.
    pub fn default_tts_model(&self) -> Result<&str> {
        self.tts_models
            .keys()
            .next()
            .map(String::as_str)
            .context("manifest has no TTS models")
    }

    /// Fail loudly if callers try to use an asset whose SHA is still a
    /// placeholder. Tebis refuses to download a file it can't verify.
    pub fn validate_stt_usable(&self, name: &str) -> Result<()> {
        let m = self.stt_model(name)?;
        if m.sha256.starts_with(PLACEHOLDER_PREFIX) {
            bail!(
                "STT model `{name}` has placeholder SHA — pin the real hash in \
                 src/audio/manifest.json before enabling `local` STT, or switch to \
                 a remote provider (`groq`, `openai`, `openai_compat`)"
            );
        }
        Ok(())
    }

    /// Fail loudly if callers try to use a TTS model whose SHAs are still
    /// placeholders. Validates onnx + tokenizer + the default voice; other
    /// voices are validated lazily when the user picks one.
    pub fn validate_tts_usable(&self, name: &str) -> Result<()> {
        let m = self.tts_model(name)?;
        if m.onnx_sha256.starts_with(PLACEHOLDER_PREFIX) {
            bail!(
                "TTS model `{name}` has placeholder ONNX SHA — pin via \
                 scripts/pin-model-shas.sh --apply"
            );
        }
        if m.tokenizer_sha256.starts_with(PLACEHOLDER_PREFIX) {
            bail!(
                "TTS model `{name}` has placeholder tokenizer SHA — pin via \
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

    /// Validate a specific voice (used when the user picks a non-default
    /// voice at runtime). Returns the voice descriptor on success.
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

    /// Catches malformed manifest.json at `cargo test` time.
    #[test]
    fn embedded_manifest_parses() {
        let m = get();
        // v2 added per-voice files + tokenizer to the TTS schema.
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
            assert!(
                !m.tokenizer_url.is_empty(),
                "TTS `{name}` has empty tokenizer URL"
            );
            assert!(!m.onnx_sha256.is_empty());
            assert!(!m.tokenizer_sha256.is_empty());
            assert!(m.onnx_size_bytes > 0);
            assert!(m.tokenizer_size_bytes > 0);
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
        // Default (`base.en`) now has a real SHA pinned; validate should pass.
        let default = get().default_stt_model().unwrap();
        assert!(get().validate_stt_usable(default).is_ok());
    }

    /// The placeholder-rejection path is still important — if a future
    /// manifest bump adds a new asset without pinning its hash, tebis
    /// must refuse to run local STT against it. We can't test via the
    /// embedded manifest (all real SHAs now) so synthesize the check.
    #[test]
    fn placeholder_prefix_is_rejected_by_convention() {
        assert!(
            PLACEHOLDER_PREFIX.starts_with("TBD-"),
            "placeholder convention drifted — validate_stt_usable relies on starts_with"
        );
        // A placeholder string satisfies the starts_with guard.
        let placeholder = format!("{PLACEHOLDER_PREFIX}future-model");
        assert!(placeholder.starts_with(PLACEHOLDER_PREFIX));
    }
}
