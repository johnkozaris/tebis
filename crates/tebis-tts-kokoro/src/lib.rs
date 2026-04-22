//! # tebis-tts-kokoro
//!
//! Kokoro v1.0 TTS engine in pure Rust.
//!
//! - Text → phonemes via `espeak-ng` shell-out (no GPL/LGPL linking)
//! - IPA substitution table matching what Kokoro was trained on
//!   (diphthong merging, flap-T, rhotacization, tie-mark stripping)
//! - Text normalization for numbers, currency, titles, years
//! - ONNX Runtime inference via `ort` 2.x `load-dynamic`
//! - Output: 24 kHz mono `f32` PCM
//!
//! This crate is deliberately tebis-agnostic. Any project that needs
//! Kokoro synthesis can depend on it directly — there is no network
//! I/O, no SHA verification, no config system, no trait dependency on
//! a specific caller.
//!
//! ## Minimum viable call
//!
//! ```no_run
//! use std::path::PathBuf;
//! use tebis_tts_kokoro::KokoroTts;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Caller is responsible for placing these files on disk:
//! let model = PathBuf::from("/path/to/kokoro/model.onnx");
//! let voices_dir = PathBuf::from("/path/to/kokoro/voices");
//!
//! let tts = KokoroTts::load(&model, voices_dir)?;
//! let synthesis = tts.synthesize("Hello from Kokoro.", "af_sarah").await?;
//! // synthesis.pcm is 24 kHz f32 mono, ready for Opus / WAV encoding.
//! # Ok(())
//! # }
//! ```
//!
//! ## Runtime requirements (not linked, discovered at runtime)
//!
//! - `espeak-ng` on `PATH` — American-English phonemizer. Install:
//!   `brew install espeak-ng` (macOS) or your distro's package.
//! - `libonnxruntime` on the dynamic-library search path. Install:
//!   `brew install onnxruntime` (macOS) or `apt install
//!   libonnxruntime-dev` (Debian/Ubuntu).
//!
//! Both are shell-out / dynamically loaded, so this crate stays
//! MIT-clean regardless of those tools' licenses.
//!
//! ## What's *not* in scope
//!
//! - Downloading the 346 MB ONNX model or per-voice `.bin` files.
//! - Verifying their SHA against a manifest.
//! - Caching voice files to disk.
//! - Streaming / chunked synthesis (< 510-phoneme limit per call).
//! - Voice-style blending (e.g. `af_sky+af_nicole.3` syntax).
//!
//! All of those are caller concerns.

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
