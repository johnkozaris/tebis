//! Thin adapter over [`tebis_tts_kokoro`] — bridges the crate's
//! types to tebis's TTS surface (`Synthesis`, `TtsError`) and impls
//! the tebis [`super::Tts`] trait so Kokoro fits the `Backend` enum
//! dispatch without extra match arms.
//!
//! The crate stays tebis-agnostic; this file is the only place that
//! knows about both. If tebis's Tts surface ever changes, edits stop
//! here — the Kokoro crate is unaffected.

#![cfg(feature = "kokoro")]

use std::path::{Path, PathBuf};

use tebis_tts_kokoro::{KokoroError, KokoroTts as CrateTts};

use super::{Synthesis, Tts, TtsError};

pub struct KokoroTts {
    inner: CrateTts,
}

impl KokoroTts {
    pub fn load(model_path: &Path, voices_dir: PathBuf) -> Result<Self, TtsError> {
        let inner = CrateTts::load(model_path, voices_dir).map_err(err_from_crate)?;
        Ok(Self { inner })
    }
}

impl Tts for KokoroTts {
    async fn synthesize(&self, text: &str, voice: &str) -> Result<Synthesis, TtsError> {
        let out = self.inner.synthesize(text, voice).await.map_err(err_from_crate)?;
        Ok(Synthesis {
            pcm: out.pcm,
            duration_ms: out.duration_ms,
            sample_rate: out.sample_rate,
        })
    }
}

/// Translate the crate's typed errors to tebis's. `Init` and
/// `Synthesis` map one-to-one; `EmptyOutput` is the same variant name
/// but a distinct type, so we flatten into tebis's `EmptyOutput`.
fn err_from_crate(e: KokoroError) -> TtsError {
    match e {
        KokoroError::Init(msg) => TtsError::Init(msg),
        KokoroError::Synthesis(msg) => TtsError::Synthesis(msg),
        KokoroError::EmptyOutput => TtsError::EmptyOutput,
    }
}
