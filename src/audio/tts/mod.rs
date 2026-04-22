//! Text-to-speech: trait + backends.
//!
//! Phase 4 ships one backend: macOS `say`, shelled out as a subprocess.
//! Linux gets a clear "not yet implemented" error at `AudioSubsystem::new`
//! time rather than confusing errors at reply time. A future backend
//! for Linux (likely `espeak-ng` shell-out or a pure-Rust Kokoro via
//! `any-tts` once its `reqwest`-using download feature is optional)
//! drops in as a new module without touching the trait.
//!
//! Output: 16 kHz mono `f32` PCM in `[-1.0, 1.0]`. Caller pipes through
//! [`crate::audio::codec::encode_pcm_to_opus`] for Telegram's sendVoice.

#[cfg(target_os = "macos")]
pub mod say;

/// Config fed from `TELEGRAM_TTS_*` env vars. See `config.rs`.
#[derive(Debug, Clone)]
pub struct TtsConfig {
    /// Manifest voice key (for future Kokoro backend) OR the `say -v`
    /// voice name (for the macOS backend). On macOS examples:
    /// `"Samantha"`, `"Ava (Premium)"`, `"Alex"`.
    pub voice: String,
    /// When `true`, every text reply also gets a voice reply. When
    /// `false`, only replies to inbound voice messages get a voice
    /// reply. Default `false` — text-in → text-out stays text-only.
    pub respond_to_all: bool,
}

/// Synthesis result — PCM + wall-clock synthesis time.
/// `encode_pcm_to_opus` requires 16 kHz mono; backends that emit other
/// rates must resample before returning.
#[derive(Debug)]
pub struct Synthesis {
    pub pcm: Vec<f32>,
    /// Wall-clock milliseconds spent in the backend's synthesize call.
    pub duration_ms: u32,
}

impl Synthesis {
    /// Actual audio duration in seconds, computed from sample count at
    /// 16 kHz mono. Used by the bridge to send an accurate `duration`
    /// field with `sendVoice` so Telegram displays the right length on
    /// the waveform bubble (beats guessing from byte count).
    #[must_use]
    pub fn audio_duration_sec(&self) -> u32 {
        // PCM is always 16 kHz mono by contract (backend resamples).
        u32::try_from(self.pcm.len() / 16_000).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod synthesis_tests {
    use super::Synthesis;

    #[test]
    fn audio_duration_floors_to_whole_second() {
        // 23_999 samples at 16 kHz = 1.499 s → 1 second floored.
        let s = Synthesis {
            pcm: vec![0.0; 23_999],
            duration_ms: 0,
        };
        assert_eq!(s.audio_duration_sec(), 1);
    }

    #[test]
    fn audio_duration_exact_seconds() {
        let s = Synthesis {
            pcm: vec![0.0; 32_000],
            duration_ms: 0,
        };
        assert_eq!(s.audio_duration_sec(), 2);
    }

    #[test]
    fn audio_duration_empty_is_zero() {
        let s = Synthesis {
            pcm: Vec::new(),
            duration_ms: 0,
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
/// On non-macOS platforms this enum has **no variants** — which is
/// deliberate: it means `Option<Backend>` at the subsystem level is
/// statically always `None` on Linux / other targets, so the "TTS is
/// unavailable here" state is expressed through the type system
/// rather than runtime error paths.
pub enum Backend {
    #[cfg(target_os = "macos")]
    Say(say::SayTts),
}

impl Backend {
    pub async fn synthesize(&self, text: &str, voice: &str) -> Result<Synthesis, TtsError> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Say(b) => b.synthesize(text, voice).await,
        }
    }
}
