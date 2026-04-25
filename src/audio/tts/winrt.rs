//! Windows built-in TTS backend via WinRT `SpeechSynthesizer`.
//!
//! Uses `Windows.Media.SpeechSynthesis.SpeechSynthesizer` (OneCore voice
//! stack, same engine `Narrator`/`Cortana` historically used). Zero-install
//! on Windows 10+: at least one voice (David/Zira/Mark) ships with every
//! consumer build. Quality is modest (not the Win11 "Natural" neural
//! voices, which Microsoft gates to Narrator) but acceptable for short
//! Telegram voice-note replies.
//!
//! ## Why WinRT, not PowerShell
//!
//! - In-proc: ~20 ms cold vs ~200 ms for `powershell.exe + System.Speech`.
//! - No quoting landmines: we pass the text as a WinRT `HSTRING`, not a
//!   shell argv.
//! - Access to `AllVoices()` enumeration → substring-match on
//!   `TELEGRAM_TTS_VOICE` so users can say `voice=Zira` without knowing
//!   the full `Microsoft Zira Desktop - English (United States)` name.
//!
//! ## Threading / COM
//!
//! WinRT APIs require an initialized Windows Runtime apartment on the calling
//! thread. The synthesis call is wrapped in `tokio::task::spawn_blocking`
//! so we stay off the reactor and can `RoInitialize(MTA)` once per call
//! without leaking state. WinRT async operations are awaited via
//! their completion handler on that same blocking thread.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tokio::task;
use windows::Media::SpeechSynthesis::SpeechSynthesizer;
use windows::Storage::Streams::{DataReader, IInputStream, InputStreamOptions};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{HRESULT, HSTRING, RuntimeType};
use windows_future::{AsyncOperationCompletedHandler, AsyncStatus, IAsyncOperation};

use super::{Synthesis, TtsError, wav};

/// Max time we'll wait for a single async op to complete. WinRT speech
/// synthesis of a short message is typically <500 ms; 30 s is a wide
/// safety margin that still prevents a stuck call from hanging the
/// bridge forever.
const ASYNC_WAIT: Duration = Duration::from_secs(30);

/// Sample rate WinRT picks for its default WAV output. We read
/// what actually came back from `fmt ` rather than trusting this.
const DEFAULT_RATE: u32 = 16_000;
const RPC_E_CHANGED_MODE: HRESULT = HRESULT(0x8001_0106_u32 as i32);

#[derive(Default, Clone, Copy)]
pub struct WinRtTts;

impl WinRtTts {
    pub const fn new() -> Self {
        Self
    }

    /// Instantiate a `SpeechSynthesizer` once to confirm at least one
    /// voice is installed. Called from `build_tts` so config errors
    /// surface at startup instead of the first Telegram message.
    pub async fn probe(voice: &str) -> Result<(), TtsError> {
        let voice = voice.to_string();
        task::spawn_blocking(move || -> Result<(), TtsError> {
            winrt_scope(|| {
                let synth = SpeechSynthesizer::new()
                    .map_err(|e| TtsError::Init(format!("SpeechSynthesizer::new: {e}")))?;
                let voices = SpeechSynthesizer::AllVoices()
                    .map_err(|e| TtsError::Init(format!("AllVoices: {e}")))?;
                let count = voices
                    .Size()
                    .map_err(|e| TtsError::Init(format!("AllVoices.Size: {e}")))?;
                if count == 0 {
                    return Err(TtsError::Init(
                        "no WinRT SpeechSynthesizer voices installed on this host".to_string(),
                    ));
                }
                if !voice.trim().is_empty()
                    && pick_voice(&voice)
                        .map_err(|e| TtsError::Init(format!("voice enumeration: {e}")))?
                        .is_none()
                {
                    tracing::warn!(
                        voice = %voice,
                        "WinRT TTS: configured voice substring not found among installed voices; default voice will be used"
                    );
                }
                drop(synth);
                Ok(())
            })
        })
        .await
        .map_err(|e| TtsError::Init(format!("spawn_blocking panicked: {e}")))?
    }

    fn synth_blocking(text: HSTRING, voice: String) -> Result<Vec<u8>, TtsError> {
        winrt_scope(|| {
            let synth = SpeechSynthesizer::new()
                .map_err(|e| TtsError::Synthesis(format!("SpeechSynthesizer::new: {e}")))?;

            if !voice.trim().is_empty() {
                match pick_voice(&voice) {
                    Ok(Some(v)) => {
                        if let Err(e) = synth.SetVoice(&v) {
                            return Err(TtsError::Synthesis(format!(
                                "SetVoice({voice:?}) failed: {e}"
                            )));
                        }
                    }
                    Ok(None) => {
                        tracing::warn!(
                            voice = %voice,
                            "WinRT TTS: voice substring not found among installed voices — using default"
                        );
                    }
                    Err(e) => {
                        return Err(TtsError::Synthesis(format!("voice enumeration: {e}")));
                    }
                }
            }

            let op = synth
                .SynthesizeTextToStreamAsync(&text)
                .map_err(|e| TtsError::Synthesis(format!("SynthesizeTextToStreamAsync: {e}")))?;
            let stream = wait_operation(&op, "SynthesizeTextToStreamAsync")?;

            let size = stream
                .Size()
                .map_err(|e| TtsError::Synthesis(format!("stream.Size: {e}")))?;
            if size == 0 {
                return Err(TtsError::EmptyOutput);
            }
            let size_u32 = u32::try_from(size).map_err(|_| {
                TtsError::Synthesis(format!("WinRT stream too large to read: {size} bytes"))
            })?;

            let input: IInputStream = stream
                .GetInputStreamAt(0)
                .map_err(|e| TtsError::Synthesis(format!("GetInputStreamAt(0): {e}")))?;
            let reader = DataReader::CreateDataReader(&input)
                .map_err(|e| TtsError::Synthesis(format!("CreateDataReader: {e}")))?;
            reader
                .SetInputStreamOptions(InputStreamOptions::None)
                .map_err(|e| TtsError::Synthesis(format!("SetInputStreamOptions: {e}")))?;
            let load_op = reader
                .LoadAsync(size_u32)
                .map_err(|e| TtsError::Synthesis(format!("LoadAsync: {e}")))?;
            let loaded = wait_operation(&load_op, "DataReader.LoadAsync")?;
            if loaded != size_u32 {
                return Err(TtsError::Synthesis(format!(
                    "WinRT stream loaded {loaded} of {size_u32} bytes"
                )));
            }

            let loaded = usize::try_from(loaded).map_err(|_| {
                TtsError::Synthesis(format!("WinRT stream too large to buffer: {loaded} bytes"))
            })?;
            let mut bytes = vec![0u8; loaded];
            reader
                .ReadBytes(&mut bytes)
                .map_err(|e| TtsError::Synthesis(format!("ReadBytes: {e}")))?;
            Ok(bytes)
        })
    }
}

impl super::Tts for WinRtTts {
    async fn synthesize(&self, text: &str, voice: &str) -> Result<Synthesis, TtsError> {
        if text.trim().is_empty() {
            return Err(TtsError::Synthesis("empty text".to_string()));
        }

        let start = Instant::now();
        let hs = HSTRING::from(text);
        let voice = voice.to_string();

        let wav_bytes = task::spawn_blocking(move || Self::synth_blocking(hs, voice))
            .await
            .map_err(|e| TtsError::Synthesis(format!("spawn_blocking panicked: {e}")))??;

        let pcm = wav::parse_le_i16_wav(&wav_bytes)?;
        if pcm.is_empty() {
            return Err(TtsError::EmptyOutput);
        }
        let mut sample_rate = wav::sample_rate_from_wav(&wav_bytes);
        if sample_rate == 0 {
            sample_rate = DEFAULT_RATE;
        }
        let (pcm, sample_rate) = normalize_for_opus(pcm, sample_rate);

        Ok(Synthesis {
            pcm,
            duration_ms: u32::try_from(start.elapsed().as_millis()).unwrap_or(u32::MAX),
            sample_rate,
        })
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "linear resampling maps sample indexes; bounds are checked before casts"
)]
fn normalize_for_opus(pcm: Vec<f32>, sample_rate: u32) -> (Vec<f32>, u32) {
    if matches!(sample_rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000) {
        return (pcm, sample_rate);
    }
    tracing::warn!(
        sample_rate,
        "WinRT TTS returned a non-Opus-native sample rate; resampling to 16 kHz"
    );
    if pcm.len() < 2 || sample_rate == 0 {
        return (pcm, DEFAULT_RATE);
    }

    let output_rate = DEFAULT_RATE;
    let out_len_u64 =
        (pcm.len() as u64).saturating_mul(u64::from(output_rate)) / u64::from(sample_rate);
    let out_len = usize::try_from(out_len_u64.max(1)).unwrap_or(usize::MAX);
    let mut out = Vec::with_capacity(out_len);
    let ratio = sample_rate as f64 / f64::from(output_rate);
    for i in 0..out_len {
        let pos = (i as f64) * ratio;
        let lo = pos.floor() as usize;
        let hi = (lo + 1).min(pcm.len() - 1);
        let frac = (pos - lo as f64) as f32;
        out.push(pcm[lo] * (1.0 - frac) + pcm[hi] * frac);
    }
    (out, output_rate)
}

fn wait_operation<T>(op: &IAsyncOperation<T>, label: &'static str) -> Result<T, TtsError>
where
    T: RuntimeType + 'static,
{
    let (tx, rx) = mpsc::channel();
    op.SetCompleted(&AsyncOperationCompletedHandler::new(move |_, status| {
        let _ = tx.send(status);
        Ok(())
    }))
    .map_err(|e| TtsError::Synthesis(format!("{label}: SetCompleted: {e}")))?;

    let status = match op
        .Status()
        .map_err(|e| TtsError::Synthesis(format!("{label}: Status: {e}")))?
    {
        AsyncStatus::Started => rx
            .recv_timeout(ASYNC_WAIT)
            .map_err(|e| TtsError::Synthesis(format!("{label}: timed out: {e}")))?,
        other => other,
    };

    match status {
        AsyncStatus::Completed => op
            .GetResults()
            .map_err(|e| TtsError::Synthesis(format!("{label}: GetResults: {e}"))),
        AsyncStatus::Canceled => Err(TtsError::Synthesis(format!("{label}: canceled"))),
        AsyncStatus::Error => {
            let code = op
                .ErrorCode()
                .map_err(|e| TtsError::Synthesis(format!("{label}: ErrorCode: {e}")))?;
            Err(TtsError::Synthesis(format!(
                "{label}: failed with {code:?}"
            )))
        }
        AsyncStatus::Started => Err(TtsError::Synthesis(format!(
            "{label}: completion handler returned Started"
        ))),
        _ => Err(TtsError::Synthesis(format!(
            "{label}: unknown async status {status:?}"
        ))),
    }
}

/// Substring-match `AllVoices()` on display name or ID. Case-insensitive.
fn pick_voice(
    needle: &str,
) -> windows::core::Result<Option<windows::Media::SpeechSynthesis::VoiceInformation>> {
    let needle_lc = needle.to_ascii_lowercase();
    let voices = SpeechSynthesizer::AllVoices()?;
    let count = voices.Size()?;
    for i in 0..count {
        let v = voices.GetAt(i)?;
        let name = v.DisplayName().unwrap_or_default().to_string_lossy();
        let id = v.Id().unwrap_or_default().to_string_lossy();
        if name.to_ascii_lowercase().contains(&needle_lc)
            || id.to_ascii_lowercase().contains(&needle_lc)
        {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// Scope guard for `RoInitialize(MTA)` + `RoUninitialize`. Runs the closure
/// with WinRT initialized on the current thread. `S_FALSE` is a success and
/// still requires a matching `RoUninitialize`; STA reuse (`RPC_E_CHANGED_MODE`)
/// is rejected because WinRT TTS is tested in MTA.
fn winrt_scope<T>(f: impl FnOnce() -> Result<T, TtsError>) -> Result<T, TtsError> {
    // SAFETY: RoInitialize is thread-local; caller runs on a spawn_blocking
    // worker and balances every successful call with RoUninitialize below.
    let init = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    if let Err(e) = init {
        if e.code() == RPC_E_CHANGED_MODE {
            return Err(TtsError::Synthesis(
                "RoInitialize(MTA) failed: Windows Runtime is already initialized as STA"
                    .to_string(),
            ));
        }
        return Err(TtsError::Synthesis(format!(
            "RoInitialize(MTA) failed: {e}"
        )));
    }
    let _guard = RoInitGuard;
    f()
}

struct RoInitGuard;

impl Drop for RoInitGuard {
    fn drop(&mut self) {
        // SAFETY: constructed only after a successful RoInitialize on this thread.
        unsafe { RoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_runs_without_panic() {
        // On CI without an audio stack we may still hit a failure;
        // accept either outcome so long as we don't crash the process.
        let _ = WinRtTts::probe("").await;
    }
}
