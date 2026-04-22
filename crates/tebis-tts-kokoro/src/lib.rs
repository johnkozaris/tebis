//! Kokoro v1.0 TTS engine.
//!
//! Pipeline: text → espeak-ng (shell-out) → E2M IPA fixups → vocab
//! filter → ort v2 `Session::run` → 24 kHz mono `f32` PCM.
//!
//! Runtime deps (not linked, so the crate stays MIT-clean):
//! - `espeak-ng` on PATH (`brew install espeak-ng` / `apt install espeak-ng`)
//! - `libonnxruntime` on the dylib search path
//!   (`brew install onnxruntime` / `apt install libonnxruntime-dev`)
//!
//! Caller provides file paths for the model + voices; this crate does
//! no network I/O, no SHA pinning, no caching.
//!
//! ```no_run
//! # async fn ex() -> Result<(), Box<dyn std::error::Error>> {
//! use tebis_tts_kokoro::KokoroTts;
//! let tts = KokoroTts::load(
//!     std::path::Path::new("model.onnx"),
//!     std::path::PathBuf::from("voices"),
//! )?;
//! let synth = tts.synthesize("Hello.", "af_sarah").await?;
//! // synth.pcm is 24 kHz f32 mono
//! # Ok(()) }
//! ```

mod e2m;
mod normalize;
mod phonemize;
mod session;
mod tokens;
mod voices;

pub use session::{KokoroTts, Synthesis};

/// Typed errors returned by [`KokoroTts`]. Matches the shape of
/// [`thiserror`] so callers can pattern-match on the variant to
/// decide between "init-time, fail-open to text-only" versus
/// "per-request, log-and-skip-voice".
#[derive(Debug, thiserror::Error)]
pub enum KokoroError {
    /// Model / voice / runtime setup failed — fatal for this backend
    /// instance. Callers should surface a clear install message and
    /// fall back to another TTS (or text-only).
    #[error("init: {0}")]
    Init(String),

    /// Synthesis failed on this call. Previous and future calls may
    /// still succeed. Callers should skip voice for this reply and
    /// continue.
    #[error("synthesis: {0}")]
    Synthesis(String),

    /// Session ran cleanly but the output tensor was empty — usually
    /// means a bug in phonemization (all phonemes dropped).
    #[error("empty output from Kokoro inference")]
    EmptyOutput,
}

/// Native output sample rate of Kokoro v1.0 (Hz). Callers encoding to
/// Opus should use this directly — Opus natively supports 24 kHz, and
/// resampling to 16 kHz without a low-pass filter aliases sibilant
/// energy (learned the hard way in an earlier version of this code).
pub const OUTPUT_SAMPLE_RATE: u32 = 24_000;
