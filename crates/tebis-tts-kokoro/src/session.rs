//! `ort::Session` lifecycle + synthesis pipeline.
//!
//! ort 2.x's `Session::run` takes `&mut self` (onnxruntime C API isn't
//! reentrant per-session), so we serialize synth calls behind a
//! `std::sync::Mutex` held only inside `spawn_blocking` — never across
//! `.await`, so no tokio deadlock risk.
//!
//! `speed` dtype is detected once at load time — Kokoro v1.0 exports
//! ship either `int32` or `f32` depending on the export script; a
//! hard-coded guess silently misbehaves on the other variant.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ndarray::{Array1, Array2};
use ort::execution_providers::CPUExecutionProvider;
use ort::session::Session;
use ort::session::builder::{GraphOptimizationLevel, SessionBuilder};
use ort::tensor::TensorElementType;
use ort::value::{TensorRef, ValueType};

use crate::KokoroError;
use crate::phonemize;
use crate::tokens;
use crate::voices::Voice;

/// Kokoro's tokenizer pads sequences with zero on both sides. Model
/// was trained with the markers; removing them breaks prosody.
const PAD_TOKEN: i64 = 0;

#[derive(Debug, Clone, Copy)]
enum SpeedDtype {
    Int32,
    Float32,
}

/// Loaded Kokoro model + voice directory + cached input dtype.
pub struct KokoroTts {
    session: Arc<Mutex<Session>>,
    voices_dir: PathBuf,
    speed_dtype: SpeedDtype,
}

/// 24 kHz mono `f32` PCM + wall-clock synthesis time.
///
/// `sample_rate` is always [`crate::OUTPUT_SAMPLE_RATE`]; exposed as
/// a field for convenience when passing to an encoder that accepts
/// rate as a parameter.
#[derive(Debug)]
pub struct Synthesis {
    pub pcm: Vec<f32>,
    pub duration_ms: u32,
    pub sample_rate: u32,
}

impl KokoroTts {
    /// Load the ONNX model and inspect its input signature.
    ///
    /// Blocks for ~500 ms on first load of the 346 MB fp32 model.
    /// Don't invoke from a request handler — this is startup work.
    pub fn load(model_path: &Path, voices_dir: PathBuf) -> Result<Self, KokoroError> {
        let session = SessionBuilder::new()
            .map_err(|e| init_err("ort SessionBuilder", &e))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| init_err("ort optimization level", &e))?
            .with_execution_providers([CPUExecutionProvider::default().build()])
            .map_err(|e| init_err("ort CPU provider", &e))?
            .commit_from_file(model_path)
            .map_err(|e| {
                KokoroError::Init(format!(
                    "ort load `{}`: {e} (is libonnxruntime installed? \
                     macOS: `brew install onnxruntime`)",
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

    /// Synthesize `text` with the named `voice`.
    ///
    /// `voice_name` must match a `<name>.bin` file in the `voices_dir`
    /// passed to [`Self::load`]. Missing voice → [`KokoroError::Init`]
    /// bubbled from the voice loader.
    ///
    /// Holds the session mutex for the duration of the ~300 ms forward
    /// pass, so process-shutdown cancellation waits up to one synth
    /// worth of latency — acceptable for normal bounded text, not for
    /// a runaway 510-phoneme max call.
    pub async fn synthesize(
        &self,
        text: &str,
        voice_name: &str,
    ) -> Result<Synthesis, KokoroError> {
        if text.trim().is_empty() {
            return Err(KokoroError::Synthesis("empty text".to_string()));
        }

        let ipa = phonemize::phonemize(text).await?;

        let session = Arc::clone(&self.session);
        let voices_dir = self.voices_dir.clone();
        let speed_dtype = self.speed_dtype;
        let voice_name = voice_name.to_string();

        let start = std::time::Instant::now();
        let pcm = tokio::task::spawn_blocking(move || -> Result<Vec<f32>, KokoroError> {
            let mut guard = session
                .lock()
                .map_err(|_| KokoroError::Synthesis("session mutex poisoned".to_string()))?;
            synthesize_blocking(&mut guard, &voices_dir, speed_dtype, &ipa, &voice_name)
        })
        .await
        .map_err(|e| KokoroError::Synthesis(format!("blocking join: {e}")))??;

        let duration_ms = u32::try_from(start.elapsed().as_millis()).unwrap_or(u32::MAX);
        if pcm.is_empty() {
            return Err(KokoroError::EmptyOutput);
        }

        Ok(Synthesis {
            pcm,
            duration_ms,
            sample_rate: crate::OUTPUT_SAMPLE_RATE,
        })
    }
}

fn init_err(ctx: &'static str, e: &dyn std::fmt::Display) -> KokoroError {
    KokoroError::Init(format!("{ctx}: {e}"))
}

fn detect_speed_dtype(session: &Session) -> Result<SpeedDtype, KokoroError> {
    let speed_input = session
        .inputs
        .iter()
        .find(|i| i.name == "speed")
        .ok_or_else(|| {
            KokoroError::Init(
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
        other => Err(KokoroError::Init(format!(
            "ONNX `speed` input has unexpected type {other:?} — expected int32 or float32"
        ))),
    }
}

fn synthesize_blocking(
    session: &mut Session,
    voices_dir: &Path,
    speed_dtype: SpeedDtype,
    ipa: &str,
    voice_name: &str,
) -> Result<Vec<f32>, KokoroError> {
    let token_ids = tokens::ipa_to_token_ids(ipa);
    if token_ids.is_empty() {
        return Err(KokoroError::Synthesis(
            "no vocab-valid phonemes after espeak-ng filter".to_string(),
        ));
    }
    if token_ids.len() > tokens::MAX_PHONEMES {
        return Err(KokoroError::Synthesis(format!(
            "phonemized text exceeds {} tokens ({})",
            tokens::MAX_PHONEMES,
            token_ids.len(),
        )));
    }

    let mut input_ids = Vec::with_capacity(token_ids.len() + 2);
    input_ids.push(PAD_TOKEN);
    input_ids.extend_from_slice(&token_ids);
    input_ids.push(PAD_TOKEN);
    let input_ids_arr = Array2::from_shape_vec((1, input_ids.len()), input_ids)
        .map_err(|e| KokoroError::Synthesis(format!("input_ids shape: {e}")))?;

    // `voice[len(tokens)]` row — tokens count BEFORE boundary pads,
    // matching kokoro-onnx's indexing so the style vector aligns with
    // the un-padded sequence length.
    let voice_path = voices_dir.join(format!("{voice_name}.bin"));
    let voice = Voice::load(&voice_path)?;
    let style = voice.style_for_token_count(token_ids.len());

    let session_outputs = match speed_dtype {
        SpeedDtype::Int32 => {
            let speed = Array1::<i32>::from_vec(vec![1]);
            session
                .run(ort::inputs![
                    "input_ids" => TensorRef::from_array_view(&input_ids_arr)
                        .map_err(|e| KokoroError::Synthesis(format!("input_ids tensor: {e}")))?,
                    "style" => TensorRef::from_array_view(&style)
                        .map_err(|e| KokoroError::Synthesis(format!("style tensor: {e}")))?,
                    "speed" => TensorRef::from_array_view(&speed)
                        .map_err(|e| KokoroError::Synthesis(format!("speed tensor: {e}")))?,
                ])
                .map_err(|e| KokoroError::Synthesis(format!("session run: {e}")))?
        }
        SpeedDtype::Float32 => {
            let speed = Array1::<f32>::from_vec(vec![1.0]);
            session
                .run(ort::inputs![
                    "input_ids" => TensorRef::from_array_view(&input_ids_arr)
                        .map_err(|e| KokoroError::Synthesis(format!("input_ids tensor: {e}")))?,
                    "style" => TensorRef::from_array_view(&style)
                        .map_err(|e| KokoroError::Synthesis(format!("style tensor: {e}")))?,
                    "speed" => TensorRef::from_array_view(&speed)
                        .map_err(|e| KokoroError::Synthesis(format!("speed tensor: {e}")))?,
                ])
                .map_err(|e| KokoroError::Synthesis(format!("session run: {e}")))?
        }
    };

    // Output name varies between ONNX exports; `waveform` is the
    // canonical name from the kokoro-onnx export script but custom
    // re-exports can rename it.
    let output_key = if session_outputs.contains_key("waveform") {
        "waveform"
    } else {
        session_outputs
            .keys()
            .next()
            .ok_or_else(|| KokoroError::Synthesis("session produced no outputs".to_string()))?
    };

    let waveform_value = &session_outputs[output_key];
    let (shape, data) = waveform_value
        .try_extract_tensor::<f32>()
        .map_err(|e| KokoroError::Synthesis(format!("extract waveform: {e}")))?;
    if shape.is_empty() {
        return Err(KokoroError::EmptyOutput);
    }
    let pcm: Vec<f32> = data.to_vec();
    if pcm.is_empty() {
        return Err(KokoroError::EmptyOutput);
    }
    Ok(pcm)
}
