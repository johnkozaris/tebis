//! Voice bridge subsystem: STT (inbound) and TTS (outbound, Phase 4).
//!
//! Phase 1 status: local STT works end-to-end via `whisper-rs`; remote
//! providers (Groq / `OpenAI` / `openai_compat`) are declared in config
//! but will be wired in Phase 2.
//!
//! - `manifest.rs` — embedded JSON of pinned asset URLs + SHAs.
//! - `cache.rs` — `$XDG_DATA_HOME/tebis/models/` filesystem layout,
//!   atomic model install, stale-tmp reaping.
//! - `fetch.rs` — HTTPS streaming download with SHA-256 verification.
//! - `codec.rs` — OGG/Opus ↔ PCM for Telegram voice (stub, Phase 3).
//! - `stt/` — Phase 1: `whisper-rs` in-process + remote stubs.
//! - `tts/` — Phase 4: `any-tts` in-process + remote stubs.
//!
//! See `/PLAN-VOICE.md` for the end-to-end design, including invariant
//! compliance (CLAUDE.md 4, 5, 6, 9, 10, 12) and the rollout phases.

pub mod cache;
pub mod codec;
pub mod fetch;
pub mod manifest;
pub mod stt;

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use self::stt::{SttConfig, SttError, SttProvider, Transcription};

/// Composite config consumed by [`AudioSubsystem::new`]. Built from env
/// in `config::load_audio_config`.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// `None` means STT is disabled (the master flag `TELEGRAM_STT=off`).
    pub stt: Option<SttConfig>,
    // Phase 4 will add `pub tts: Option<TtsConfig>`.
}

impl AudioConfig {
    /// Quick check used by main.rs to decide whether to bother constructing
    /// the subsystem at all — if both branches are off, the whole subsystem
    /// stays uninitialized.
    pub const fn any_enabled(&self) -> bool {
        self.stt.is_some()
    }
}

/// Unified error surface for the audio subsystem. Sub-modules keep their
/// own typed errors (`FetchError`, `CodecError`, `SttError`) for
/// pattern-matching; this enum is the one we expose to `bridge`, which
/// flattens to an HTML-escaped reply string.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error(transparent)]
    Fetch(#[from] fetch::FetchError),

    #[error(transparent)]
    Codec(#[from] codec::CodecError),

    #[error(transparent)]
    Stt(#[from] SttError),

    #[error("audio subsystem config: {0}")]
    Config(String),

    #[error("audio subsystem not initialized for feature `{feature}` — set TELEGRAM_STT=on")]
    NotEnabled { feature: &'static str },
}

/// Closed enum over STT backends. Each Phase adds variants:
/// Phase 1 ships `Local`; Phase 2 adds `OpenAiCompat`/`Groq`/`OpenAi`.
///
/// Enum dispatch (vs `Box<dyn Stt>`) so the `Stt` trait can keep its
/// ergonomic AFIT shape. The trait still exists in `stt::mod` as a
/// test seam and shape contract for each concrete backend.
enum SttBackend {
    Local(stt::local::LocalStt),
}

impl SttBackend {
    async fn transcribe(&self, pcm: &[f32], lang: &str) -> Result<Transcription, SttError> {
        use stt::Stt as _;
        match self {
            Self::Local(b) => b.transcribe(pcm, lang).await,
        }
    }
}

pub struct AudioSubsystem {
    stt: Option<SttBackend>,
    // tts: Option<TtsBackend>, // Phase 4
    stt_model_name: Option<String>,
}

impl AudioSubsystem {
    /// Lazy init. If neither STT nor TTS is enabled in `cfg`, returns a
    /// subsystem that answers every call with `NotEnabled`. If local STT
    /// is enabled and the model isn't cached yet, downloads it
    /// synchronously before returning (blocks startup for ~53 s on a
    /// fresh install with the default `base.en` model).
    ///
    /// `_tracker` and `_shutdown` are carried for future cancellation
    /// plumbing during the model download — Phase 1 doesn't split
    /// startup-download into a background task, so they're unused here
    /// but the signature stays future-compatible.
    pub async fn new(
        cfg: &AudioConfig,
        _tracker: &TaskTracker,
        shutdown: CancellationToken,
    ) -> Result<Arc<Self>> {
        let (stt, stt_model_name) = match &cfg.stt {
            None => (None, None),
            Some(scfg) => {
                let (backend, model_name) = build_stt(scfg, shutdown.clone()).await?;
                (Some(backend), Some(model_name))
            }
        };

        Ok(Arc::new(Self {
            stt,
            stt_model_name,
        }))
    }

    /// Transcribe 16 kHz mono `f32` PCM samples. Returns
    /// [`AudioError::NotEnabled`] if STT was not initialized.
    pub async fn transcribe(&self, pcm: &[f32], lang: &str) -> Result<Transcription, AudioError> {
        let stt = self.stt.as_ref().ok_or(AudioError::NotEnabled { feature: "stt" })?;
        Ok(stt.transcribe(pcm, lang).await?)
    }

    /// Which STT model is loaded (for dashboard display). `None` if
    /// STT is disabled.
    pub fn stt_model_name(&self) -> Option<&str> {
        self.stt_model_name.as_deref()
    }
}

/// Returns `(backend_enum, model_name_for_display)`.
async fn build_stt(
    cfg: &SttConfig,
    shutdown: CancellationToken,
) -> Result<(SttBackend, String)> {
    match cfg.provider {
        SttProvider::Local => build_local_stt(cfg, shutdown).await,
        SttProvider::OpenAiCompat | SttProvider::Groq | SttProvider::OpenAi => {
            // Phase 2 unlocks these. Fail loudly at startup so the user
            // doesn't get a surprise at first voice note.
            bail!(
                "STT provider `{}` will arrive in Phase 2 — set TELEGRAM_STT_PROVIDER=local for now",
                cfg.provider.as_str()
            )
        }
    }
}

async fn build_local_stt(
    cfg: &SttConfig,
    shutdown: CancellationToken,
) -> Result<(SttBackend, String)> {
    let manifest = manifest::get();
    manifest
        .validate_stt_usable(&cfg.model)
        .context("local STT model is not pin-validated")?;

    let asset = manifest
        .stt_model(&cfg.model)
        .context("resolving local STT model from manifest")?;

    let file_name = filename_from_url(&asset.url);
    let model_path = cache::model_path(&file_name)
        .context("resolving model cache path")?;

    cache::reap_stale_tmps(model_path.parent().unwrap_or(&model_path))
        .context("reaping stale .tmp files in models dir")?;

    if model_path.exists() {
        tracing::info!(model = %cfg.model, path = %model_path.display(), "Using cached STT model");
    } else {
        let client = fetch::FetchClient::new();
        let tmp = cache::tmp_path_for(&model_path);
        tracing::info!(
            model = %cfg.model,
            size_mb = asset.size_bytes / (1024 * 1024),
            "Downloading {}…",
            asset.display_name
        );

        let mut last_logged = 0u64;
        client
            .download_verified(
                &asset.url,
                &asset.sha256,
                &tmp,
                &model_path,
                |bytes, total| {
                    // Throttled progress log: at most once every 8 MiB.
                    const LOG_EVERY: u64 = 8 * 1024 * 1024;
                    if bytes.saturating_sub(last_logged) >= LOG_EVERY {
                        last_logged = bytes;
                        if let Some(t) = total {
                            tracing::info!(
                                "  …downloaded {} / {} MB",
                                bytes / (1024 * 1024),
                                t / (1024 * 1024),
                            );
                        } else {
                            tracing::info!("  …downloaded {} MB", bytes / (1024 * 1024));
                        }
                    }
                },
                shutdown,
            )
            .await
            .context("downloading local STT model")?;
        tracing::info!(model = %cfg.model, "Model download + verification complete");
    }

    let backend = stt::local::LocalStt::load(&model_path, cfg.threads, &cfg.language)
        .context("loading whisper-rs context")?;
    Ok((SttBackend::Local(backend), cfg.model.clone()))
}

/// Extract the filename from an HF download URL (the basename of the
/// path). HF uses `https://.../resolve/main/<filename>` with no query
/// string, so `rsplit('/').next()` is sufficient.
fn filename_from_url(url: &str) -> String {
    // Strip optional query string defensively, in case an upstream
    // URL ever grows one.
    let no_query = url.split('?').next().unwrap_or(url);
    no_query
        .rsplit('/')
        .next()
        .unwrap_or("model.bin")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_from_hf_url() {
        assert_eq!(
            filename_from_url(
                "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin"
            ),
            "ggml-base.en.bin"
        );
    }

    #[test]
    fn filename_from_url_strips_query() {
        assert_eq!(
            filename_from_url("https://example.com/path/file.bin?download=1"),
            "file.bin"
        );
    }

    #[test]
    fn filename_from_url_fallback_when_no_slash() {
        assert_eq!(filename_from_url("nosuchurl"), "nosuchurl");
    }

    #[test]
    fn audio_config_any_enabled_tracks_stt() {
        let off = AudioConfig { stt: None };
        assert!(!off.any_enabled());
    }
}
