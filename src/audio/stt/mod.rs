//! Speech-to-text: single local backend, gated by a trait that keeps
//! the test seam clean.
//!
//! Tebis ships **only local whisper.cpp** — no cloud providers, no LAN
//! escape hatches. The [`Stt`] trait exists (a) to mirror the shape of
//! `notify::Forwarder` for consistency across tebis subsystems, and
//! (b) so bridge-dispatch tests can inject a recording fake instead of
//! loading a real 148 MB model in CI.
//!
//! Callers hand in pre-decoded 16 kHz mono `f32` PCM samples in
//! `[-1.0, 1.0]`. OGG/Opus → PCM decoding is the caller's responsibility
//! and lives in `audio::codec`.

pub mod local;

/// Populated from `TELEGRAM_STT_*` env. Owned by `audio::AudioConfig`.
#[derive(Debug, Clone)]
pub struct SttConfig {
    /// Key in `manifest.stt_models` (e.g. `"base.en"`). Drives which
    /// model file gets downloaded on first run and loaded into the
    /// whisper-rs context.
    pub model: String,
    /// ISO-639-1 hint (`"en"`, `"de"`, …). Empty = let whisper.cpp autodetect.
    pub language: String,
    /// Reject voice notes whose `duration` exceeds this before downloading.
    pub max_duration_sec: u32,
    /// Reject voice `file_size` over this.
    pub max_bytes: u32,
    /// Whisper thread count (passed through to `whisper-rs`'s `n_threads`).
    pub threads: u32,
}

/// Transcription result, uniform across every backend.
#[derive(Debug, Clone)]
pub struct Transcription {
    /// Detected text, with whitespace trimmed. May be empty for silent input.
    pub text: String,
    /// Wall-clock time spent in `Stt::transcribe`.
    pub duration_ms: u32,
    /// Language actually used (echoes `SttConfig::language` for `Local`;
    /// remote providers may return a detected language).
    pub language: String,
}

/// Error taxonomy. Every backend collapses to this enum at its boundary
/// so the bridge handler doesn't pattern-match on backend-specifics.
#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("transcription failed: audio too long ({secs}s > {cap}s cap)")]
    TooLong { secs: u32, cap: u32 },

    /// Failed inside whisper-rs.
    #[error("transcription failed: local inference: {0}")]
    LocalInference(String),

    #[error("transcription failed: decoder: {0}")]
    Decoder(String),

    #[error("transcription failed: rate-limited")]
    RateLimited,
}

/// Test seam — mirrors `notify::Forwarder`.
///
/// AFIT (async fn in trait) — matches the `Forwarder` pattern instead
/// of `#[async_trait]` so the dep graph stays lean.
pub trait Stt: Send + Sync + 'static {
    /// Transcribe 16 kHz mono `f32` PCM samples in `[-1.0, 1.0]`.
    /// `lang` is ISO-639-1 or empty for auto-detect.
    fn transcribe(
        &self,
        pcm: &[f32],
        lang: &str,
    ) -> impl std::future::Future<Output = Result<Transcription, SttError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stt_error_too_long_renders_cap() {
        let err = SttError::TooLong {
            secs: 240,
            cap: 120,
        };
        let msg = err.to_string();
        assert!(msg.contains("240s"));
        assert!(msg.contains("120s"));
    }
}
