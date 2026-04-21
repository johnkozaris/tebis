//! Speech-to-text: pluggable backends behind a thin [`Stt`] trait.
//!
//! Matching the shape of `notify::Forwarder` — one trait, one impl per
//! provider, tests inject a recording fake. Phase 1 ships only
//! [`local::LocalStt`] (whisper-rs in-process). Phase 2 adds remote
//! backends (`openai_compat`, `groq`, `openai`).
//!
//! Callers hand in pre-decoded 16 kHz mono `f32` PCM samples in
//! `[-1.0, 1.0]`. Decoding OGG/Opus → PCM is the caller's responsibility
//! and lives in `audio::codec` (stub until Phase 3 / bridge integration).

pub mod local;

use anyhow::{Result, bail};
use secrecy::SecretString;

/// Which backend handles transcription. Parsed from
/// `TELEGRAM_STT_PROVIDER`. Only `Local` is wired in Phase 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttProvider {
    /// whisper-rs linked in-process. Default.
    Local,
    /// `POST /v1/audio/transcriptions` against a user-supplied URL.
    OpenAiCompat,
    /// Groq (multipart, `whisper-large-v3-turbo`, 10s min billing).
    Groq,
    /// `OpenAI` (`whisper-1` / `gpt-4o-transcribe`).
    OpenAi,
}

impl SttProvider {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "openai_compat" | "openai-compat" => Ok(Self::OpenAiCompat),
            "groq" => Ok(Self::Groq),
            "openai" => Ok(Self::OpenAi),
            other => {
                bail!(
                    "unknown STT provider {other:?} — use local, openai_compat, groq, or openai"
                )
            }
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::OpenAiCompat => "openai_compat",
            Self::Groq => "groq",
            Self::OpenAi => "openai",
        }
    }
}

/// Populated from `TELEGRAM_STT_*` env. Owned by `audio::AudioConfig`.
#[derive(Debug, Clone)]
pub struct SttConfig {
    pub provider: SttProvider,
    /// For `Local`: key in `manifest.stt_models` (e.g. `"base.en"`).
    /// For remote providers: the provider's model name (e.g. `"whisper-1"`).
    pub model: String,
    /// Required for `OpenAiCompat`; ignored by others.
    pub base_url: Option<String>,
    /// Required for `Groq`/`OpenAi`; optional for `OpenAiCompat`; unused by `Local`.
    pub api_key: Option<SecretString>,
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
    /// Pre-redacted via `telegram::redact_network_error` / equivalents.
    #[error("transcription failed: network: {0}")]
    Network(String),

    #[error("transcription failed: provider returned HTTP {0}")]
    Provider(u16),

    #[error("transcription failed: audio too long ({secs}s > {cap}s cap)")]
    TooLong { secs: u32, cap: u32 },

    /// Failed inside whisper-rs or a backend-local crate.
    #[error("transcription failed: local inference: {0}")]
    LocalInference(String),

    #[error("transcription failed: decoder: {0}")]
    Decoder(String),

    #[error("transcription failed: rate-limited")]
    RateLimited,

    /// Provider selected in config but not compiled in this phase.
    #[error("transcription failed: provider `{0}` not available in this build")]
    ProviderUnavailable(&'static str),
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
    fn provider_parse_roundtrip() {
        for p in [
            SttProvider::Local,
            SttProvider::OpenAiCompat,
            SttProvider::Groq,
            SttProvider::OpenAi,
        ] {
            assert_eq!(SttProvider::parse(p.as_str()).unwrap(), p);
        }
    }

    #[test]
    fn provider_parse_accepts_dash_alias() {
        assert_eq!(
            SttProvider::parse("openai-compat").unwrap(),
            SttProvider::OpenAiCompat
        );
    }

    #[test]
    fn provider_parse_case_insensitive() {
        assert_eq!(SttProvider::parse("LOCAL").unwrap(), SttProvider::Local);
        assert_eq!(SttProvider::parse("  Groq  ").unwrap(), SttProvider::Groq);
    }

    #[test]
    fn provider_parse_rejects_unknown() {
        assert!(SttProvider::parse("lmstudio").is_err());
        assert!(SttProvider::parse("").is_err());
    }

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
