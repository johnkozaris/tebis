//! Adapter between `tebis_tts_kokoro` and tebis's Tts surface.

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

fn err_from_crate(e: KokoroError) -> TtsError {
    match e {
        KokoroError::Init(msg) => TtsError::Init(msg),
        KokoroError::Synthesis(msg) => TtsError::Synthesis(msg),
        KokoroError::EmptyOutput => TtsError::EmptyOutput,
    }
}
