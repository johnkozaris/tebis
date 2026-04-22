//! Text-to-speech: trait + backends.
//!
//! Three backends, selected via `TELEGRAM_TTS_BACKEND`:
//! - `say` — macOS `say` shell-out. Zero install, built-in voices.
//! - `kokoro-local` — local Kokoro v1.0 ONNX via `ort` + espeak-ng
//!   phonemizer. Feature-gated behind `kokoro`. Cross-platform.
//! - `kokoro-remote` — OpenAI-compatible HTTP TTS endpoint (user's
//!   Kokoro-FastAPI, etc.). Uses the existing hyper/rustls stack.
//!
//! Output contract for backends that return PCM (`say`, `kokoro-local`):
//! 16 kHz mono `f32` in `[-1.0, 1.0]`. The subsystem pipes through
//! [`crate::audio::codec::encode_pcm_to_opus`] for Telegram's sendVoice.
//! The remote backend skips the codec entirely — the server returns
//! OGG/Opus bytes that Telegram accepts verbatim.

#[cfg(target_os = "macos")]
pub mod say;

#[cfg(feature = "kokoro")]
pub mod kokoro;

pub mod remote;

use secrecy::SecretString;

/// Top-level TTS config. See `config::load_tts_config` for env parsing.
#[derive(Debug, Clone)]
pub struct TtsConfig {
    /// Which backend drives synthesis.
    pub backend: BackendConfig,
    /// When `true`, every text reply also gets a voice reply. When
    /// `false`, only replies to inbound voice messages get a voice
    /// reply. Default `false` — text-in → text-out stays text-only.
    pub respond_to_all: bool,
}

/// Per-backend configuration. Each variant carries only the fields that
/// backend actually needs — the env-var adapter in `config.rs` enforces
/// the "all required fields present" invariant at load time.
#[derive(Debug, Clone)]
pub enum BackendConfig {
    /// macOS `say` shell-out. `voice` is a `say -v` name, e.g.
    /// `"Samantha"`, `"Alex"`, `"Ava (Premium)"`.
    Say { voice: String },
    /// Local Kokoro v1.0 via `ort` + espeak-ng. `model` is a manifest
    /// key; `voice` is a key from that model's voices map.
    KokoroLocal { model: String, voice: String },
    /// Remote OpenAI-compatible TTS endpoint.
    Remote {
        /// Base URL — the client appends `/v1/audio/speech`.
        url: String,
        /// Bearer token. None = no `Authorization` header sent.
        api_key: Option<SecretString>,
        /// `model` param sent in the request body.
        model: String,
        /// `voice` param sent in the request body.
        voice: String,
        /// Request timeout in seconds (1..=300).
        timeout_sec: u32,
    },
}

impl BackendConfig {
    /// Voice name as the user configured it. Used for display (dashboard,
    /// banner) and for backward-compat callers that want the voice string
    /// regardless of backend.
    pub fn voice(&self) -> &str {
        match self {
            Self::Say { voice }
            | Self::KokoroLocal { voice, .. }
            | Self::Remote { voice, .. } => voice,
        }
    }

    /// Human-friendly backend label for the dashboard / logs.
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::Say { .. } => "say",
            Self::KokoroLocal { .. } => "kokoro-local",
            Self::Remote { .. } => "kokoro-remote",
        }
    }
}

/// Synthesis result — PCM + wall-clock synthesis time + native rate.
///
/// `sample_rate` is the backend's native output rate (16 kHz for
/// macOS `say`, 24 kHz for Kokoro). The caller encodes via
/// `codec::encode_pcm_to_opus(&pcm, sample_rate)` — no resample
/// in-between, which avoids the sibilant-aliasing problem we'd otherwise
/// get with ad-hoc decimation.
#[derive(Debug)]
pub struct Synthesis {
    pub pcm: Vec<f32>,
    /// Wall-clock milliseconds spent in the backend's synthesize call.
    pub duration_ms: u32,
    /// Native sample rate of `pcm`. Must be an Opus-supported rate
    /// (8/12/16/24/48 kHz) — backends that produce other rates must
    /// convert before returning.
    pub sample_rate: u32,
}

impl Synthesis {
    /// Actual audio duration in seconds, computed from sample count at
    /// the backend's native rate. Used by the bridge to send an
    /// accurate `duration` field with `sendVoice` so Telegram displays
    /// the right length on the waveform bubble.
    #[must_use]
    pub fn audio_duration_sec(&self) -> u32 {
        if self.sample_rate == 0 {
            return 0;
        }
        u32::try_from(self.pcm.len() as u64 / u64::from(self.sample_rate))
            .unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod synthesis_tests {
    use super::Synthesis;

    #[test]
    fn audio_duration_floors_to_whole_second_at_16k() {
        // 23_999 samples at 16 kHz = 1.499 s → 1 second floored.
        let s = Synthesis {
            pcm: vec![0.0; 23_999],
            duration_ms: 0,
            sample_rate: 16_000,
        };
        assert_eq!(s.audio_duration_sec(), 1);
    }

    #[test]
    fn audio_duration_at_24k() {
        // 24_000 samples at 24 kHz = exactly 1 second.
        let s = Synthesis {
            pcm: vec![0.0; 24_000],
            duration_ms: 0,
            sample_rate: 24_000,
        };
        assert_eq!(s.audio_duration_sec(), 1);
        // 2 s at 24 kHz
        let s2 = Synthesis {
            pcm: vec![0.0; 48_000],
            duration_ms: 0,
            sample_rate: 24_000,
        };
        assert_eq!(s2.audio_duration_sec(), 2);
    }

    #[test]
    fn audio_duration_empty_is_zero() {
        let s = Synthesis {
            pcm: Vec::new(),
            duration_ms: 0,
            sample_rate: 24_000,
        };
        assert_eq!(s.audio_duration_sec(), 0);
    }

    #[test]
    fn audio_duration_zero_rate_is_zero_not_division_by_zero() {
        let s = Synthesis {
            pcm: vec![0.0; 100],
            duration_ms: 0,
            sample_rate: 0,
        };
        assert_eq!(s.audio_duration_sec(), 0);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TtsError {
    #[error("TTS not available on this platform")]
    UnsupportedPlatform,

    #[error("TTS backend init failed: {0}")]
    Init(String),

    #[error("TTS synthesis failed: {0}")]
    Synthesis(String),

    #[error("TTS backend returned empty audio")]
    EmptyOutput,
}

/// Test seam — mirrors `stt::Stt`. AFIT to avoid the `async_trait` macro.
pub trait Tts: Send + Sync + 'static {
    /// Synthesize `text` to 16 kHz mono `f32` PCM. `voice` is the backend
    /// voice name (macOS `say -v` arg, or future Kokoro voice key).
    fn synthesize(
        &self,
        text: &str,
        voice: &str,
    ) -> impl std::future::Future<Output = Result<Synthesis, TtsError>> + Send;
}

/// Closed enum over TTS backends. Enum dispatch (vs `Box<dyn Tts>`)
/// keeps the trait's AFIT shape consistent with `stt::Stt`.
///
/// Variants:
/// - `Say` — macOS only, gated by `cfg(target_os = "macos")`.
/// - `Remote` — cross-platform, always compiled in.
/// - `Kokoro` — cross-platform, gated by the `kokoro` feature (added
///   in the feature-gated local-ONNX task). Absent here until that
///   lands to keep this file buildable.
///
/// On non-macOS platforms without the `kokoro` feature, the enum has
/// only the `Remote` variant — users who can't reach a remote server
/// will fall through to text-only at `AudioSubsystem::new`.
pub enum Backend {
    #[cfg(target_os = "macos")]
    Say(say::SayTts),
    // `RemoteTts` holds a hyper `Client` with a webpki root store —
    // a few hundred bytes. Box it so the enum layout doesn't bloat
    // every `AudioSubsystem` by the same amount.
    Remote(Box<remote::RemoteTts>),
    // Local Kokoro via ort — feature-gated because ONNX Runtime is a
    // heavy runtime dep. `KokoroTts` owns an `Arc<Session>` (also a
    // few hundred bytes), so boxed for the same layout reason.
    #[cfg(feature = "kokoro")]
    Kokoro(Box<kokoro::KokoroTts>),
}
