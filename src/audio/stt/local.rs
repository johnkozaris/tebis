//! whisper-rs in-process. `Arc<WhisperContext>` loaded once; fresh
//! `WhisperState` per call to reset KV. Inference on `spawn_blocking`.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use tokio::task;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::{SttError, Transcription};

pub struct LocalStt {
    ctx: Arc<WhisperContext>,
    threads: i32,
    default_language: String,
}

impl LocalStt {
    /// Blocking (~300 ms base.en on M4) — run once at subsystem init.
    pub fn load(
        model_path: &Path,
        threads: u32,
        default_language: &str,
    ) -> Result<Self, SttError> {
        let path_str = model_path.to_str().ok_or_else(|| {
            SttError::LocalInference(format!(
                "model path is not valid UTF-8: {}",
                model_path.display()
            ))
        })?;

        let params = WhisperContextParameters::default();
        let ctx = WhisperContext::new_with_params(path_str, params)
            .map_err(|e| SttError::LocalInference(format!("model load failed: {e}")))?;

        Ok(Self {
            ctx: Arc::new(ctx),
            threads: i32::try_from(threads).unwrap_or(i32::MAX),
            default_language: default_language.to_string(),
        })
    }
}

impl super::Stt for LocalStt {
    async fn transcribe(&self, pcm: &[f32], lang: &str) -> Result<Transcription, SttError> {
        let pcm = pcm.to_vec();
        let lang = if lang.trim().is_empty() {
            self.default_language.clone()
        } else {
            lang.to_string()
        };
        let threads = self.threads;
        let ctx = Arc::clone(&self.ctx);

        task::spawn_blocking(move || Self::infer(&ctx, threads, &lang, &pcm))
            .await
            .map_err(|e| SttError::LocalInference(format!("spawn_blocking join: {e}")))?
    }
}

impl LocalStt {
    fn infer(
        ctx: &WhisperContext,
        threads: i32,
        lang: &str,
        pcm: &[f32],
    ) -> Result<Transcription, SttError> {
        let start = Instant::now();

        let mut state = ctx
            .create_state()
            .map_err(|e| SttError::LocalInference(format!("create_state: {e}")))?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(threads);
        // `None` for empty/"auto" — whisper-rs takes the language as `Option<&str>`.
        if !lang.is_empty() && lang != "auto" {
            params.set_language(Some(lang));
        } else {
            params.set_language(None);
        }
        params.set_translate(false);
        // Each voice note is independent — reset KV.
        params.set_no_context(true);
        params.set_single_segment(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);

        state
            .full(params, pcm)
            .map_err(|e| SttError::LocalInference(format!("inference: {e}")))?;

        let mut text = String::new();
        for seg in state.as_iter() {
            match seg.to_str_lossy() {
                Ok(s) => text.push_str(&s),
                Err(e) => {
                    return Err(SttError::LocalInference(format!(
                        "segment text extraction failed: {e}"
                    )));
                }
            }
        }

        let elapsed = start.elapsed();
        Ok(Transcription {
            text: text.trim().to_string(),
            duration_ms: u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX),
            language: lang.to_string(),
        })
    }
}
