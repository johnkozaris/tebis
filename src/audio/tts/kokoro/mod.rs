//! Kokoro v1.0 local TTS backend.
//!
//! Pipeline:
//! 1. `espeak-ng` shell-out → IPA phoneme string (`phonemize`)
//! 2. Filter-to-vocab + map to sparse i64 token ids (`tokens`)
//! 3. Load per-voice `.npy` style table, row-index by token count (`voices`)
//! 4. Build `input_ids` with boundary pads `[0, ..., 0]`
//! 5. `ort::Session::run` — single forward pass, ~200-400 ms on M4
//! 6. Return `Synthesis { pcm, duration_ms, sample_rate: 24_000 }`
//!    — `codec::encode_pcm_to_opus` encodes at native 24 kHz. No
//!    resample step (earlier versions aliased sibilant energy).
//!
//! Runtime dep: `libonnxruntime` via `ort`'s `load-dynamic` feature.
//! Install with `brew install onnxruntime` (macOS) or your distro's
//! package. Missing → [`TtsError::Init`]; `AudioSubsystem::new` catches
//! that and fails-open to text-only replies.
//!
//! Architecture decisions worth surfacing:
//!
//! - **One `Arc<Session>`, shared across synth calls.** ort sessions
//!   are thread-safe; no per-call construction. Session is expensive
//!   (~500 ms cold start for the 346 MB fp32 model), tokenizer+voice
//!   are cheap — separating lifetimes is the right tradeoff.
//!
//! - **Speed dtype dispatched at init.** The exported ONNX file could
//!   use either `int32` or `f32` for the `speed` input; which one
//!   depends on the export script. We check `Session::inputs()` once
//!   at load time and cache the result so synth calls don't reinspect.
//!
//! - **Inference on `spawn_blocking`.** Running the forward pass on
//!   a tokio worker would block the poll loop for hundreds of ms.
//!   Voice loading (~5 ms disk I/O) piggybacks on the same blocking
//!   task to avoid an extra await boundary.
//!
//! - **No voice caching.** Voices are 510 KB and load in ~5 ms; adding
//!   a HashMap + Mutex for one disk read per N-minute message is noise.
//!   If traffic patterns change, revisit.

#![cfg(feature = "kokoro")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ndarray::{Array1, Array2};
use ort::execution_providers::CPUExecutionProvider;
use ort::session::Session;
use ort::session::builder::{GraphOptimizationLevel, SessionBuilder};
use ort::tensor::TensorElementType;
use ort::value::{TensorRef, ValueType};

use super::super::TtsError;
use super::{Synthesis, Tts};

pub mod e2m;
pub mod normalize;
pub mod phonemize;
pub mod tokens;
pub mod voices;

use voices::Voice;

/// Boundary-pad token prepended + appended to every sequence. Kokoro's
/// tokenizer outputs `[0] + tokens + [0]` — the model was trained with
/// these markers and will misbehave if they're omitted.
const PAD_TOKEN: i64 = 0;

/// Dtype of the model's `speed` input tensor. Kokoro v1.0 exports have
/// varied between `int32` and `float32` depending on which script
/// generated the ONNX file; we detect once at load time rather than
/// hard-coding a guess.
#[derive(Debug, Clone, Copy)]
enum SpeedDtype {
    Int32,
    Float32,
}

pub struct KokoroTts {
    /// `ort::Session::run` takes `&mut self` in 2.x (the underlying
    /// onnxruntime API isn't reentrant per-session), so we serialize
    /// concurrent synth calls behind a `std::sync::Mutex`. Held only
    /// inside `spawn_blocking` — never across an `.await` — so no
    /// tokio-runtime deadlock risk.
    session: Arc<Mutex<Session>>,
    voices_dir: PathBuf,
    speed_dtype: SpeedDtype,
}

impl KokoroTts {
    /// Load the ONNX model + inspect its signature. `voices_dir` is
    /// the directory holding `<voice>.bin` files — the synth call
    /// reads the right one based on the requested voice name.
    ///
    /// Blocks for ~500 ms on first load of the 346 MB fp32 model.
    /// Caller should invoke from a context that tolerates that
    /// (startup path, not a request handler).
    pub fn load(model_path: &Path, voices_dir: PathBuf) -> Result<Self, TtsError> {
        let session = SessionBuilder::new()
            .map_err(|e| TtsError::Init(format!("ort SessionBuilder: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| TtsError::Init(format!("ort optimization level: {e}")))?
            .with_execution_providers([CPUExecutionProvider::default().build()])
            .map_err(|e| TtsError::Init(format!("ort CPU provider: {e}")))?
            .commit_from_file(model_path)
            .map_err(|e| {
                TtsError::Init(format!(
                    "ort load `{}`: {e} (is libonnxruntime installed? macOS: `brew install onnxruntime`)",
                    model_path.display(),
                ))
            })?;

        let speed_dtype = detect_speed_dtype(&session)?;

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            voices_dir,
            speed_dtype,
        })
    }
}

/// Inspect session inputs to figure out the `speed` tensor's dtype.
/// Falls back to `Float32` only as a last resort so mis-exported
/// models surface via a wrong-dtype runtime error rather than silent
/// misbehavior.
fn detect_speed_dtype(session: &Session) -> Result<SpeedDtype, TtsError> {
    let speed_input = session
        .inputs
        .iter()
        .find(|i| i.name == "speed")
        .ok_or_else(|| {
            TtsError::Init(
                "ONNX model has no `speed` input — not a Kokoro v1.0 export?".to_string(),
            )
        })?;
    match &speed_input.input_type {
        ValueType::Tensor {
            ty: TensorElementType::Int32,
            ..
        } => Ok(SpeedDtype::Int32),
        ValueType::Tensor {
            ty: TensorElementType::Float32,
            ..
        } => Ok(SpeedDtype::Float32),
        other => Err(TtsError::Init(format!(
            "ONNX `speed` input has unexpected type {other:?} — expected int32 or float32"
        ))),
    }
}

impl Tts for KokoroTts {
    async fn synthesize(&self, text: &str, voice_name: &str) -> Result<Synthesis, TtsError> {
        if text.trim().is_empty() {
            return Err(TtsError::Synthesis("empty text".to_string()));
        }

        // Phase 1 (async) — shell out to espeak-ng.
        let ipa = phonemize::phonemize(text).await?;

        // Phase 2 (blocking) — CPU/memory-bound everything else.
        // spawn_blocking releases the tokio worker so the poll loop
        // and other handlers keep running during the ~300 ms forward
        // pass.
        let session = Arc::clone(&self.session);
        let voices_dir = self.voices_dir.clone();
        let speed_dtype = self.speed_dtype;
        let voice_name = voice_name.to_string();

        let start = std::time::Instant::now();
        let pcm_24k = tokio::task::spawn_blocking(move || -> Result<Vec<f32>, TtsError> {
            let mut guard = session
                .lock()
                .map_err(|_| TtsError::Synthesis("session mutex poisoned".to_string()))?;
            synthesize_blocking(&mut guard, &voices_dir, speed_dtype, &ipa, &voice_name)
        })
        .await
        .map_err(|e| TtsError::Synthesis(format!("blocking join: {e}")))??;

        let duration_ms = u32::try_from(start.elapsed().as_millis()).unwrap_or(u32::MAX);
        if pcm_24k.is_empty() {
            return Err(TtsError::EmptyOutput);
        }

        // Kokoro emits 24 kHz f32 natively. We return it at that rate
        // and let `codec::encode_pcm_to_opus` encode at 24 kHz — Opus
        // supports it natively. Previously we downsampled to 16 kHz
        // with a linear filter, which aliased sibilant energy above
        // the 8 kHz output Nyquist and produced static on "s" / "sh".
        Ok(Synthesis {
            pcm: pcm_24k,
            duration_ms,
            sample_rate: 24_000,
        })
    }
}

/// Blocking body of `synthesize`. Keeps the async fn short and makes
/// the unit-test path (when we add one with a fixture model) free of
/// tokio plumbing.
fn synthesize_blocking(
    session: &mut Session,
    voices_dir: &Path,
    speed_dtype: SpeedDtype,
    ipa: &str,
    voice_name: &str,
) -> Result<Vec<f32>, TtsError> {
    // Tokenize + boundary pads.
    let token_ids = tokens::ipa_to_token_ids(ipa);
    if token_ids.is_empty() {
        return Err(TtsError::Synthesis(
            "no vocab-valid phonemes after espeak-ng filter".to_string(),
        ));
    }
    if token_ids.len() > tokens::MAX_PHONEMES {
        return Err(TtsError::Synthesis(format!(
            "phonemized text exceeds {} tokens ({})",
            tokens::MAX_PHONEMES,
            token_ids.len(),
        )));
    }

    let mut input_ids = Vec::with_capacity(token_ids.len() + 2);
    input_ids.push(PAD_TOKEN);
    input_ids.extend_from_slice(&token_ids);
    input_ids.push(PAD_TOKEN);
    // Shape (1, seq_len).
    let input_ids_arr = Array2::from_shape_vec((1, input_ids.len()), input_ids).map_err(|e| {
        TtsError::Synthesis(format!("input_ids shape: {e}"))
    })?;

    // Voice: pick row = len(tokens) BEFORE the boundary pads — this
    // matches kokoro-onnx's `voice[len(tokens)]` and the style table
    // is keyed on the unpadded count.
    let voice_path = voices_dir.join(format!("{voice_name}.bin"));
    let voice = Voice::load(&voice_path)?;
    let style = voice.style_for_token_count(token_ids.len());

    // Run — dispatch on speed dtype resolved at init.
    let session_outputs = match speed_dtype {
        SpeedDtype::Int32 => {
            let speed = Array1::<i32>::from_vec(vec![1]);
            session
                .run(ort::inputs![
                    "input_ids" => TensorRef::from_array_view(&input_ids_arr)
                        .map_err(|e| TtsError::Synthesis(format!("input_ids tensor: {e}")))?,
                    "style" => TensorRef::from_array_view(&style)
                        .map_err(|e| TtsError::Synthesis(format!("style tensor: {e}")))?,
                    "speed" => TensorRef::from_array_view(&speed)
                        .map_err(|e| TtsError::Synthesis(format!("speed tensor: {e}")))?,
                ])
                .map_err(|e| TtsError::Synthesis(format!("session run: {e}")))?
        }
        SpeedDtype::Float32 => {
            let speed = Array1::<f32>::from_vec(vec![1.0]);
            session
                .run(ort::inputs![
                    "input_ids" => TensorRef::from_array_view(&input_ids_arr)
                        .map_err(|e| TtsError::Synthesis(format!("input_ids tensor: {e}")))?,
                    "style" => TensorRef::from_array_view(&style)
                        .map_err(|e| TtsError::Synthesis(format!("style tensor: {e}")))?,
                    "speed" => TensorRef::from_array_view(&speed)
                        .map_err(|e| TtsError::Synthesis(format!("speed tensor: {e}")))?,
                ])
                .map_err(|e| TtsError::Synthesis(format!("session run: {e}")))?
        }
    };

    // Output name varies between exports — try `waveform` first
    // (the canonical name from kokoro-onnx's export.py) and fall
    // back to the first output so we tolerate custom re-exports.
    let output_key = if session_outputs.contains_key("waveform") {
        "waveform"
    } else {
        session_outputs
            .keys()
            .next()
            .ok_or_else(|| TtsError::Synthesis("session produced no outputs".to_string()))?
    };

    let waveform_value = &session_outputs[output_key];
    let (shape, data) = waveform_value
        .try_extract_tensor::<f32>()
        .map_err(|e| TtsError::Synthesis(format!("extract waveform: {e}")))?;
    if shape.is_empty() {
        return Err(TtsError::EmptyOutput);
    }
    let pcm_24k: Vec<f32> = data.to_vec();
    if pcm_24k.is_empty() {
        return Err(TtsError::EmptyOutput);
    }

    // Return 24 kHz PCM verbatim. Opus encoder accepts it natively —
    // no resample step, so no aliasing of sibilant energy above 8 kHz
    // (which was the audible artifact in the 16 kHz-downsampled path).
    Ok(pcm_24k)
}

#[cfg(test)]
mod tests {
    // Unit tests for sub-modules live in their own files (tokens.rs,
    // voices.rs, resample.rs). A real end-to-end test needs a 346 MB
    // ONNX download + espeak-ng + libonnxruntime — run the
    // `examples/audio-smoke.rs --features kokoro` path manually.
}
