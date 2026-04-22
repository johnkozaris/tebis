//! Voice bridge subsystem: STT (inbound) and TTS (outbound, Phase 4).
//!
//! Current state (2026-04-22): STT is local-only via `whisper-rs`
//! (cross-platform). TTS is macOS-only via `say` shell-out; Linux
//! currently returns `TtsError::UnsupportedPlatform`. Cross-platform
//! Kokoro TTS is blocked on Rust ecosystem maturity (see `Cargo.toml`
//! comment block and `PLAN.md`).
//!
//! - `manifest.rs` — embedded JSON of pinned asset URLs + SHAs.
//! - `cache.rs` — `$XDG_DATA_HOME/tebis/models/` filesystem layout,
//!   atomic model install, stale-tmp reaping.
//! - `fetch.rs` — HTTPS streaming download with SHA-256 verification.
//! - `codec.rs` — OGG/Opus ↔ PCM for Telegram voice.
//! - `stt/` — whisper-rs in-process. The only STT backend.
//! - `tts/` — macOS `say` shell-out (the only shipped backend today).
//!
//! See `/PLAN-VOICE.md` for the end-to-end design, including invariant
//! compliance (CLAUDE.md 4, 5, 6, 9, 10, 12) and the rollout phases.

pub mod cache;
pub mod codec;
pub mod fetch;
pub mod manifest;
pub mod stt;
pub mod tts;

use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use self::stt::{Stt as _, SttConfig, SttError, Transcription};
#[cfg(any(target_os = "macos", feature = "kokoro"))]
use self::tts::Tts as _;
use self::tts::{TtsConfig, TtsError};

/// Composite config consumed by [`AudioSubsystem::new`]. Built from env
/// in `config::load_audio_config`.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// `None` means STT is disabled (the master flag `TELEGRAM_STT=off`).
    pub stt: Option<SttConfig>,
    /// `None` means TTS is disabled. Default off — voice replies are
    /// low-value for Claude's typical multi-line output.
    pub tts: Option<TtsConfig>,
}

impl AudioConfig {
    /// Quick check used by main.rs to decide whether to bother constructing
    /// the subsystem at all — if both branches are off, the whole subsystem
    /// stays uninitialized.
    pub const fn any_enabled(&self) -> bool {
        self.stt.is_some() || self.tts.is_some()
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

    #[error(transparent)]
    Tts(#[from] TtsError),

    #[error("audio subsystem config: {0}")]
    Config(String),

    #[error("audio subsystem not initialized for feature `{feature}` — set TELEGRAM_STT=on")]
    NotEnabled { feature: &'static str },
}

pub struct AudioSubsystem {
    /// `None` when STT is disabled. Local whisper.cpp is the only
    /// backend tebis ships — no cloud / LAN escape hatches.
    stt: Option<stt::local::LocalStt>,
    /// `None` when TTS is disabled or failed to initialize.
    tts: Option<tts::Backend>,
    stt_model_name: Option<String>,
    /// Snapshot of STT runtime caps. `None` when STT is off. The bridge
    /// reads these to enforce duration/size limits before downloading.
    stt_limits: Option<SttLimits>,
    /// ISO-639-1 hint to pass to whisper; `None` means auto-detect.
    stt_language: Option<String>,
    /// TTS voice name — what the backend uses. `None` when TTS is off.
    tts_voice: Option<String>,
    /// Whether TTS applies to every outbound reply or only replies to
    /// inbound voice messages.
    tts_respond_to_all: bool,
    /// Backend kind for display — `"none"`, `"say"`, `"kokoro-local"`,
    /// or `"kokoro-remote"`. Static-lifetime since the variants are
    /// fixed at compile time.
    tts_backend_kind: &'static str,
    /// Backend-specific display detail. For `kokoro-remote` this is the
    /// redacted host (`"kokoro.example.com"`); for `kokoro-local` it's
    /// the manifest model key; `None` for `say` / `none`.
    tts_detail: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct SttLimits {
    pub max_duration_sec: u32,
    pub max_bytes: u32,
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
        let (stt, stt_model_name, stt_limits, stt_language) = match &cfg.stt {
            None => (None, None, None, None),
            Some(scfg) => {
                let (backend, model_name) = build_local_stt(scfg, shutdown.clone()).await?;
                let limits = SttLimits {
                    max_duration_sec: scfg.max_duration_sec,
                    max_bytes: scfg.max_bytes,
                };
                (
                    Some(backend),
                    Some(model_name),
                    Some(limits),
                    Some(scfg.language.clone()),
                )
            }
        };

        // TTS init is decoupled from STT: a TTS failure must NOT take
        // STT down with it. A backend that doesn't fit the host platform
        // (e.g. `say` on Linux, or `kokoro-local` in a build without the
        // `kokoro` feature) downgrades to `tts = None` so STT stays fully
        // usable for those users instead of killing both branches.
        let (tts, tts_voice, tts_respond_to_all, tts_backend_kind, tts_detail) = match &cfg.tts {
            None => (None, None, false, "none", None),
            Some(tcfg) => match build_tts(tcfg, shutdown.clone()).await {
                Ok(backend) => {
                    let kind = tcfg.backend.kind_str();
                    let detail = display_detail_for(&tcfg.backend);
                    (
                        Some(backend),
                        Some(tcfg.backend.voice().to_string()),
                        tcfg.respond_to_all,
                        kind,
                        detail,
                    )
                }
                Err(TtsError::UnsupportedPlatform) => {
                    tracing::warn!(
                        backend = tcfg.backend.kind_str(),
                        "TTS backend not available on this platform; \
                         continuing with STT-only. Set TELEGRAM_TTS_BACKEND=none to silence this."
                    );
                    (None, None, false, "none", None)
                }
                Err(e) => {
                    tracing::warn!(
                        backend = tcfg.backend.kind_str(),
                        err = %e,
                        "TTS failed to initialize; continuing with STT-only. \
                         Fix the cause above or set TELEGRAM_TTS_BACKEND=none."
                    );
                    (None, None, false, "none", None)
                }
            },
        };

        Ok(Arc::new(Self {
            stt,
            tts,
            stt_model_name,
            stt_limits,
            stt_language,
            tts_voice,
            tts_respond_to_all,
            tts_backend_kind,
            tts_detail,
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

    /// STT duration + byte caps for the bridge to enforce BEFORE
    /// downloading a voice file. `None` when STT is disabled.
    pub const fn stt_limits(&self) -> Option<SttLimits> {
        self.stt_limits
    }

    /// ISO-639-1 language hint to pass to whisper. `""` (empty) means
    /// auto-detect. Returns `None` when STT is disabled.
    pub fn stt_language(&self) -> Option<&str> {
        self.stt_language.as_deref()
    }

    /// Name of the TTS voice currently loaded (e.g. `"Samantha"` for
    /// the `say` backend). `None` when TTS is disabled OR initialization
    /// failed (e.g. unsupported platform). The dashboard reads this to
    /// show real runtime state instead of what the user configured.
    pub fn tts_voice(&self) -> Option<&str> {
        self.tts.as_ref().and(self.tts_voice.as_deref())
    }

    /// Whether every outbound reply also triggers a voice reply, or
    /// only replies to inbound voice messages. Honors the subsystem's
    /// actual initialized state — returns `false` when TTS init failed.
    pub const fn tts_respond_to_all(&self) -> bool {
        self.tts.is_some() && self.tts_respond_to_all
    }

    /// Backend kind string for the dashboard. `"none"` when TTS is off
    /// or init failed; otherwise `"say"` / `"kokoro-local"` /
    /// `"kokoro-remote"`.
    pub const fn tts_backend_kind(&self) -> &'static str {
        if self.tts.is_none() {
            "none"
        } else {
            self.tts_backend_kind
        }
    }

    /// Backend-specific display detail for the dashboard. For
    /// `kokoro-remote` this is the redacted host; for `kokoro-local`
    /// it's the model key. `None` when `say` / `none`.
    pub fn tts_detail(&self) -> Option<&str> {
        self.tts.as_ref()?;
        self.tts_detail.as_deref()
    }

    /// Synthesize `text` to an OGG/Opus byte blob ready for `sendVoice`.
    /// Returns the encoded bytes **and** the accurate audio duration in
    /// seconds.
    ///
    /// Backend dispatch:
    /// - `Say` (macOS): PCM from `say` → encode to OGG/Opus via codec.
    /// - `Remote`: the server returns OGG/Opus already, pass through
    ///   verbatim; duration is extracted by decoding to PCM and counting
    ///   samples (cheap, reuses the inbound STT decode path).
    ///
    /// Returns [`AudioError::NotEnabled`] when TTS is off.
    pub async fn synthesize(&self, text: &str) -> Result<(Bytes, u32), AudioError> {
        let backend = self
            .tts
            .as_ref()
            .ok_or(AudioError::NotEnabled { feature: "tts" })?;
        match backend {
            #[cfg(target_os = "macos")]
            tts::Backend::Say(b) => {
                let voice = self.tts_voice.as_deref().unwrap_or("");
                let synthesis = b.synthesize(text, voice).await?;
                let duration_sec = synthesis.audio_duration_sec();
                let opus = codec::encode_pcm_to_opus(&synthesis.pcm, synthesis.sample_rate)?;
                Ok((opus, duration_sec))
            }
            tts::Backend::Remote(b) => Ok(b.synthesize_to_opus(text).await?),
            #[cfg(feature = "kokoro")]
            tts::Backend::Kokoro(b) => {
                let voice = self.tts_voice.as_deref().unwrap_or("");
                let synthesis = b.synthesize(text, voice).await?;
                let duration_sec = synthesis.audio_duration_sec();
                let opus = codec::encode_pcm_to_opus(&synthesis.pcm, synthesis.sample_rate)?;
                Ok((opus, duration_sec))
            }
        }
    }

    /// Whether the caller should voice-reply to a given inbound payload.
    /// `is_voice_reply` is true when the originating user message was a
    /// voice/audio payload; when false we only voice-reply if the
    /// `respond_to_all` config flag is set.
    pub const fn should_tts_reply(&self, is_voice_reply: bool) -> bool {
        self.tts.is_some() && (is_voice_reply || self.tts_respond_to_all)
    }
}

/// Construct the concrete TTS backend from the configured variant.
///
/// - `Say` on macOS → probe `say` + wrap in `SayTts`.
/// - `Say` on non-macOS → `UnsupportedPlatform` (validated at config
///   load, but the check here makes the type-system branching
///   exhaustive).
/// - `KokoroLocal` → `Init` error until the feature-gated backend lands
///   (task #46). The subsystem handles the error by falling back to
///   text-only replies (see [`AudioSubsystem::new`]).
/// - `Remote` → construct the HTTP client; no network I/O here.
async fn build_tts(
    cfg: &TtsConfig,
    shutdown: CancellationToken,
) -> Result<tts::Backend, TtsError> {
    match &cfg.backend {
        tts::BackendConfig::Say { .. } => {
            #[cfg(target_os = "macos")]
            {
                let _ = shutdown;
                tts::say::SayTts::probe().await?;
                Ok(tts::Backend::Say(tts::say::SayTts::new()))
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = shutdown;
                Err(TtsError::UnsupportedPlatform)
            }
        }
        tts::BackendConfig::KokoroLocal { model, voice } => {
            #[cfg(feature = "kokoro")]
            {
                // Thread the parent shutdown so Ctrl-C during the 346 MB
                // Kokoro model download cancels promptly instead of
                // running to completion then exiting.
                build_kokoro_local(model, voice, shutdown).await
            }
            #[cfg(not(feature = "kokoro"))]
            {
                let _ = (model, voice, shutdown);
                Err(TtsError::Init(
                    "backend=kokoro-local needs the `kokoro` cargo feature \
                     (rebuild with `cargo build --features kokoro`). \
                     Alternatively use kokoro-remote or say."
                        .to_string(),
                ))
            }
        }
        tts::BackendConfig::Remote {
            url,
            api_key,
            model,
            voice,
            timeout_sec,
        } => {
            let rt = tts::remote::RemoteTts::new(
                url.clone(),
                api_key.clone(),
                model.clone(),
                voice.clone(),
                *timeout_sec,
            )?;
            Ok(tts::Backend::Remote(Box::new(rt)))
        }
    }
}

/// Returns `(LocalStt, model_name_for_display)`. The only backend tebis
/// ships — if the model download or whisper-rs load fails, the caller
/// logs a warn and the bridge continues text-only.
async fn build_local_stt(
    cfg: &SttConfig,
    shutdown: CancellationToken,
) -> Result<(stt::local::LocalStt, String)> {
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

    // `model_path` is always `<base>/models/<file>`, so `.parent()` is
    // always `Some` — but fall back to the models dir explicitly rather
    // than trusting `unwrap_or(&model_path)` which would feed a file
    // path to `read_dir` and surface a confusing `NotADirectory` error.
    let models_dir = cache::models_dir()?;
    cache::reap_stale_tmps(&models_dir)
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
    Ok((backend, cfg.model.clone()))
}

/// Build the local Kokoro backend: probe espeak-ng, download model +
/// voice from the manifest (SHA-verified), load the ONNX session.
///
/// Only exists with the `kokoro` cargo feature. Error on missing
/// espeak-ng is surfaced early (at `AudioSubsystem::new`) instead of
/// at first-synth so users see "install espeak-ng" in the startup log
/// rather than a cryptic process-spawn failure minutes later.
#[cfg(feature = "kokoro")]
async fn build_kokoro_local(
    model_key: &str,
    voice: &str,
    shutdown: CancellationToken,
) -> Result<tts::Backend, TtsError> {
    // Early espeak-ng probe. espeak-ng is a runtime requirement for
    // every synth call; no point loading a 346 MB ONNX if it'll never
    // produce a single sample.
    if crate::setup::phonemizer::probe_espeak_ng().is_none() {
        return Err(TtsError::Init(
            "espeak-ng not found on PATH — local Kokoro requires it. \
             macOS: `brew install espeak-ng`; Linux: `apt/dnf/pacman \
             install espeak-ng`. Alternatively set \
             TELEGRAM_TTS_BACKEND=kokoro-remote or none."
                .to_string(),
        ));
    }

    // Validate + look up manifest entry.
    let manifest = manifest::get();
    manifest
        .validate_tts_usable(model_key)
        .map_err(|e| TtsError::Init(format!("TTS manifest: {e}")))?;
    let model_entry = manifest
        .tts_model(model_key)
        .map_err(|e| TtsError::Init(format!("TTS manifest: {e}")))?;
    let voice_asset = manifest
        .validate_voice(model_key, voice)
        .map_err(|e| TtsError::Init(format!("TTS voice: {e}")))?;

    // Resolve cache paths. Voice files go into `models/` alongside the
    // ONNX — same dir simplifies `KokoroTts::load`, which just
    // concatenates `<voices_dir>/<voice_name>.bin` at synth time.
    let models_dir = cache::models_dir()
        .map_err(|e| TtsError::Init(format!("models dir: {e}")))?;
    cache::reap_stale_tmps(&models_dir)
        .map_err(|e| TtsError::Init(format!("reap stale tmps: {e}")))?;
    let model_file_name = filename_from_url(&model_entry.onnx_url);
    let model_path = cache::model_path(&model_file_name)
        .map_err(|e| TtsError::Init(format!("model path: {e}")))?;
    let voice_file_name = filename_from_url(&voice_asset.url);
    let voice_path = cache::model_path(&voice_file_name)
        .map_err(|e| TtsError::Init(format!("voice path: {e}")))?;

    let client = fetch::FetchClient::new();

    // Download the ONNX model if missing. Large (~346 MB) — log every
    // 32 MiB so the operator sees progress.
    if !model_path.exists() {
        tracing::info!(
            model = %model_key,
            size_mb = model_entry.onnx_size_bytes / (1024 * 1024),
            "Downloading Kokoro model (~{} MB, one-time)…",
            model_entry.onnx_size_bytes / (1024 * 1024),
        );
        let tmp = cache::tmp_path_for(&model_path);
        let mut last_logged = 0u64;
        client
            .download_verified(
                &model_entry.onnx_url,
                &model_entry.onnx_sha256,
                &tmp,
                &model_path,
                |bytes, total| {
                    const LOG_EVERY: u64 = 32 * 1024 * 1024;
                    if bytes.saturating_sub(last_logged) >= LOG_EVERY {
                        last_logged = bytes;
                        if let Some(t) = total {
                            tracing::info!(
                                "  …{} / {} MB",
                                bytes / (1024 * 1024),
                                t / (1024 * 1024),
                            );
                        }
                    }
                },
                shutdown.clone(),
            )
            .await
            .map_err(|e| TtsError::Init(format!("download Kokoro model: {e}")))?;
        tracing::info!("Kokoro model downloaded + SHA-verified");
    }

    // Download the voice if missing. Small (~510 KB) — single-log.
    if !voice_path.exists() {
        tracing::info!(voice = %voice, "Downloading Kokoro voice (~510 KB)…");
        let tmp = cache::tmp_path_for(&voice_path);
        client
            .download_verified(
                &voice_asset.url,
                &voice_asset.sha256,
                &tmp,
                &voice_path,
                |_, _| {},
                shutdown,
            )
            .await
            .map_err(|e| TtsError::Init(format!("download voice `{voice}`: {e}")))?;
    }

    // Load ONNX — blocking, ~500 ms cold start. Spawn_blocking to
    // avoid stalling the startup event loop for the other init paths.
    let model_path_c = model_path.clone();
    let voices_dir_c = models_dir.clone();
    let backend = tokio::task::spawn_blocking(move || {
        tts::kokoro::KokoroTts::load(&model_path_c, voices_dir_c)
    })
    .await
    .map_err(|e| TtsError::Init(format!("Kokoro load join: {e}")))??;

    Ok(tts::Backend::Kokoro(Box::new(backend)))
}

/// Return a dashboard-ready detail string for the configured TTS
/// backend. For `kokoro-remote` we extract only the host portion of
/// the URL — the path and query could theoretically contain secrets
/// (someone embedding an auth token in the path on a `*.hf.space`
/// endpoint), so we clip before any `/` after the scheme.
fn display_detail_for(cfg: &tts::BackendConfig) -> Option<String> {
    match cfg {
        tts::BackendConfig::Say { .. } => None,
        tts::BackendConfig::KokoroLocal { model, .. } => Some(model.clone()),
        tts::BackendConfig::Remote { url, .. } => Some(redacted_host_from_url(url)),
    }
}

/// Extract `host[:port]` from `https://host[:port]/path?query`. Falls
/// back to the input string if no `://` is present — defensive rather
/// than an error, since config parsing already enforces the scheme.
fn redacted_host_from_url(url: &str) -> String {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host_end = after_scheme
        .find('/')
        .unwrap_or(after_scheme.len());
    after_scheme[..host_end].to_string()
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
        let off = AudioConfig { stt: None, tts: None };
        assert!(!off.any_enabled());
    }

    #[test]
    fn redacted_host_strips_path_and_query() {
        assert_eq!(
            redacted_host_from_url("https://kokoro.example.com/v1/audio/speech?q=1"),
            "kokoro.example.com"
        );
        assert_eq!(
            redacted_host_from_url("https://host:8443/path"),
            "host:8443"
        );
        assert_eq!(
            redacted_host_from_url("http://internal-lan:8880"),
            "internal-lan:8880"
        );
    }

    #[test]
    fn redacted_host_handles_missing_scheme() {
        // Defensive — config validation enforces https/http, but a
        // user hand-editing the env file could produce anything.
        assert_eq!(redacted_host_from_url("bare-host"), "bare-host");
    }
}
