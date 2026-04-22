//! TTS backends selected by `TELEGRAM_TTS_BACKEND`.

#[cfg(target_os = "macos")]
pub mod say;

#[cfg(feature = "kokoro")]
pub mod kokoro;

pub mod remote;

use secrecy::SecretString;

#[derive(Debug, Clone)]
pub struct TtsConfig {
    pub backend: BackendConfig,
    /// When true every text reply gets a voice too; default false = voice-only.
    pub respond_to_all: bool,
}

#[derive(Debug, Clone)]
pub enum BackendConfig {
    Say { voice: String },
    KokoroLocal { model: String, voice: String },
    Remote {
        /// Base URL — client appends `/v1/audio/speech`.
        url: String,
        api_key: Option<SecretString>,
        model: String,
        voice: String,
        /// 1..=300.
        timeout_sec: u32,
    },
}

impl BackendConfig {
    pub fn voice(&self) -> &str {
        match self {
            Self::Say { voice }
            | Self::KokoroLocal { voice, .. }
            | Self::Remote { voice, .. } => voice,
        }
    }

    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::Say { .. } => "say",
            Self::KokoroLocal { .. } => "kokoro-local",
            Self::Remote { .. } => "kokoro-remote",
        }
    }
}

/// PCM at the backend's native rate (say=16k, Kokoro=24k). Encode avoids a resample pass.
#[derive(Debug)]
pub struct Synthesis {
    pub pcm: Vec<f32>,
    pub duration_ms: u32,
    /// Must be an Opus-native rate: 8/12/16/24/48 kHz.
    pub sample_rate: u32,
}

impl Synthesis {
    /// Whole-seconds floor from samples ÷ rate. Feeds Telegram's `sendVoice.duration`.
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
        let s = Synthesis {
            pcm: vec![0.0; 23_999],
            duration_ms: 0,
            sample_rate: 16_000,
        };
        assert_eq!(s.audio_duration_sec(), 1);
    }

    #[test]
    fn audio_duration_at_24k() {
        let s = Synthesis {
            pcm: vec![0.0; 24_000],
            duration_ms: 0,
            sample_rate: 24_000,
        };
        assert_eq!(s.audio_duration_sec(), 1);
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

/// AFIT — mirrors `stt::Stt`.
pub trait Tts: Send + Sync + 'static {
    fn synthesize(
        &self,
        text: &str,
        voice: &str,
    ) -> impl std::future::Future<Output = Result<Synthesis, TtsError>> + Send;
}

/// Enum dispatch over backends — keeps AFIT shape consistent with `stt::Stt`.
/// Variants are boxed so `Say`'s small size doesn't force enum layout bloat.
pub enum Backend {
    #[cfg(target_os = "macos")]
    Say(say::SayTts),
    Remote(Box<remote::RemoteTts>),
    #[cfg(feature = "kokoro")]
    Kokoro(Box<kokoro::KokoroTts>),
}
