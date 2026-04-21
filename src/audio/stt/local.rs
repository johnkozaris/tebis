//! whisper-rs backend — in-process transcription.
//!
//! Holds an [`Arc<WhisperContext>`] (loaded once at subsystem init) and
//! spins up a fresh [`WhisperState`] per call. State creation is cheap
//! compared to model load (~ms vs ~300ms), and per-call state resets
//! the KV cache — which is what we want, since each Telegram voice note
//! is an independent utterance.
//!
//! Inference runs on [`tokio::task::spawn_blocking`] because whisper.cpp
//! `full` is CPU/GPU-bound and would stall the runtime otherwise. The
//! model is shared across concurrent calls via `Arc`, but the underlying
//! `ctx.create_state()` serializes internally — we don't need extra
//! locking on our side.
//!
//! Whisper.cpp's own log output is routed through the `tracing` bridge
//! installed in `src/main.rs` (via
//! [`whisper_rs::install_logging_hooks`]), so model-load warnings /
//! tensor shape mismatches surface at `warn` in tebis's journal rather
//! than disappearing to stderr.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use tokio::task;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::{SttError, Transcription};

pub struct LocalStt {
    ctx: Arc<WhisperContext>,
    threads: i32,
    /// Used when the per-call `lang` arg is empty.
    default_language: String,
}

impl LocalStt {
    /// Load the model at `model_path` into memory. Blocking (~300 ms for
    /// `base.en` on M4 with Metal; ~1 s for `small.en` on Ubuntu CPU).
    /// Caller should run this once at subsystem init, **not** per request.
    ///
    /// `threads` is clamped to `i32::MAX` — whisper.cpp takes `c_int` and
    /// any sensible config value fits anyway.
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
        // Whisper interprets empty string as "auto" via `Some("auto")` — we
        // normalize here so the spawn_blocking closure sees a stable value.
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
        // Empty / "auto" both mean detect; whisper-rs takes the string via
        // `Option<&'a str>`. We pass `None` for empty so the C side sees
        // a null pointer rather than an empty C string.
        if !lang.is_empty() && lang != "auto" {
            params.set_language(Some(lang));
        } else {
            params.set_language(None);
        }
        params.set_translate(false);
        // Each Telegram voice note is an independent utterance — reset KV.
        params.set_no_context(true);
        params.set_single_segment(false);
        // Don't spam stderr with per-segment details; we re-emit at `debug!`.
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);

        state
            .full(params, pcm)
            .map_err(|e| SttError::LocalInference(format!("inference: {e}")))?;

        // Concatenate every segment's text (to_str_lossy replaces invalid
        // UTF-8 with U+FFFD rather than erroring — better UX on pathological
        // model outputs).
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
