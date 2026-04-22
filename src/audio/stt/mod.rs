//! Speech-to-text — local whisper.cpp only. Trait exists as a test seam.
//! Callers hand in 16 kHz mono `f32` PCM in `[-1.0, 1.0]`; decoding lives in `audio::codec`.

pub mod local;

#[derive(Debug, Clone)]
pub struct SttConfig {
    /// Key in `manifest.stt_models`.
    pub model: String,
    /// ISO-639-1; empty = auto-detect.
    pub language: String,
    pub max_duration_sec: u32,
    pub max_bytes: u32,
    pub threads: u32,
}

#[derive(Debug, Clone)]
pub struct Transcription {
    pub text: String,
    pub duration_ms: u32,
    pub language: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("transcription failed: audio too long ({secs}s > {cap}s cap)")]
    TooLong { secs: u32, cap: u32 },

    #[error("transcription failed: local inference: {0}")]
    LocalInference(String),

    #[error("transcription failed: decoder: {0}")]
    Decoder(String),

    #[error("transcription failed: rate-limited")]
    RateLimited,
}

/// AFIT — consistent with `notify::Forwarder`.
pub trait Stt: Send + Sync + 'static {
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
