//! Voice bridge subsystem. STT via whisper-rs; TTS via native / Kokoro / remote backends.

pub mod cache;
pub mod codec;
pub mod espeak;
pub mod fetch;
pub mod manifest;
mod progress;
pub mod stt;
pub mod tts;

use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use self::stt::{Stt as _, SttConfig, SttError, Transcription};
#[cfg(any(target_os = "macos", target_os = "windows", feature = "kokoro-local"))]
use self::tts::Tts as _;
use self::tts::{TtsConfig, TtsError};

#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub stt: Option<SttConfig>,
    pub tts: Option<TtsConfig>,
}

impl AudioConfig {
    pub const fn any_enabled(&self) -> bool {
        self.stt.is_some() || self.tts.is_some()
    }
}

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
    stt: Option<stt::local::LocalStt>,
    tts: Option<tts::Backend>,
    stt_model_name: Option<String>,
    stt_limits: Option<SttLimits>,
    stt_language: Option<String>,
    tts_voice: Option<String>,
    tts_respond_to_all: bool,
    tts_backend_kind: &'static str,
    tts_detail: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct SttLimits {
    pub max_duration_sec: u32,
    pub max_bytes: u32,
}

impl AudioSubsystem {
    /// Downloads models synchronously on first use (~53s for base.en on fresh install).
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

        // TTS init failure must not take STT down.
        let (tts, tts_voice, tts_respond_to_all, tts_backend_kind, tts_detail) = match &cfg.tts {
            None => (None, None, false, "none", None),
            Some(tcfg) => match build_tts(tcfg, &shutdown).await {
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

    pub async fn transcribe(&self, pcm: &[f32], lang: &str) -> Result<Transcription, AudioError> {
        let stt = self
            .stt
            .as_ref()
            .ok_or(AudioError::NotEnabled { feature: "stt" })?;
        Ok(stt.transcribe(pcm, lang).await?)
    }

    pub fn stt_model_name(&self) -> Option<&str> {
        self.stt_model_name.as_deref()
    }

    pub const fn stt_limits(&self) -> Option<SttLimits> {
        self.stt_limits
    }

    pub fn stt_language(&self) -> Option<&str> {
        self.stt_language.as_deref()
    }

    pub fn tts_voice(&self) -> Option<&str> {
        self.tts.as_ref()?;
        self.tts_voice.as_deref()
    }

    pub const fn tts_respond_to_all(&self) -> bool {
        self.tts.is_some() && self.tts_respond_to_all
    }

    pub const fn tts_backend_kind(&self) -> &'static str {
        if self.tts.is_none() {
            "none"
        } else {
            self.tts_backend_kind
        }
    }

    pub fn tts_detail(&self) -> Option<&str> {
        self.tts.as_ref()?;
        self.tts_detail.as_deref()
    }

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
            #[cfg(target_os = "windows")]
            tts::Backend::WinRt(b) => {
                let voice = self.tts_voice.as_deref().unwrap_or("");
                let synthesis = b.synthesize(text, voice).await?;
                let duration_sec = synthesis.audio_duration_sec();
                let opus = codec::encode_pcm_to_opus(&synthesis.pcm, synthesis.sample_rate)?;
                Ok((opus, duration_sec))
            }
            tts::Backend::Remote(b) => Ok(b.synthesize_to_opus(text).await?),
            #[cfg(feature = "kokoro-local")]
            tts::Backend::Kokoro(b) => {
                let voice = self.tts_voice.as_deref().unwrap_or("");
                let synthesis = b.synthesize(text, voice).await?;
                let duration_sec = synthesis.audio_duration_sec();
                let opus = codec::encode_pcm_to_opus(&synthesis.pcm, synthesis.sample_rate)?;
                Ok((opus, duration_sec))
            }
        }
    }

    pub const fn should_tts_reply(&self, is_voice_reply: bool) -> bool {
        self.tts.is_some() && (is_voice_reply || self.tts_respond_to_all)
    }
}

#[cfg_attr(not(feature = "kokoro-local"), allow(unused_variables))]
async fn build_tts(
    cfg: &TtsConfig,
    shutdown: &CancellationToken,
) -> Result<tts::Backend, TtsError> {
    match &cfg.backend {
        tts::BackendConfig::Say { .. } => {
            #[cfg(target_os = "macos")]
            {
                tts::say::SayTts::probe().await?;
                Ok(tts::Backend::Say(tts::say::SayTts::new()))
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(TtsError::UnsupportedPlatform)
            }
        }
        tts::BackendConfig::WinRt { voice } => {
            #[cfg(target_os = "windows")]
            {
                tts::winrt::WinRtTts::probe(voice).await?;
                Ok(tts::Backend::WinRt(tts::winrt::WinRtTts::new()))
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = voice;
                Err(TtsError::UnsupportedPlatform)
            }
        }
        tts::BackendConfig::KokoroLocal { model, voice } => {
            #[cfg(feature = "kokoro-local")]
            {
                // Thread shutdown so Ctrl-C cancels the 346 MB download promptly.
                build_kokoro_local(model, voice, shutdown.clone()).await
            }
            #[cfg(not(feature = "kokoro-local"))]
            {
                let _ = (model, voice);
                Err(TtsError::Init(
                    "backend=kokoro-local needs the `kokoro-local` cargo feature \
                     (rebuild with `cargo build --features kokoro-local`). \
                     Alternatively use a native backend or kokoro-remote."
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
    let model_path = cache::model_path(&file_name).context("resolving model cache path")?;

    let models_dir = cache::models_dir()?;
    cache::reap_stale_tmps(&models_dir).context("reaping stale .tmp files in models dir")?;

    let was_cached = model_path.exists();
    if was_cached {
        tracing::info!(model = %cfg.model, path = %model_path.display(), "Using cached STT model");
    } else {
        download_stt_model(&cfg.model, asset, &model_path, shutdown.clone()).await?;
    }

    match stt::local::LocalStt::load(&model_path, cfg.threads, &cfg.language) {
        Ok(backend) => Ok((backend, cfg.model.clone())),
        Err(load_err) if was_cached => {
            // Corrupt cache (truncation, tampering) — refetch once.
            tracing::warn!(
                err = %load_err,
                path = %model_path.display(),
                "Cached STT model failed to load — assuming corrupt, re-downloading"
            );
            let _ = std::fs::remove_file(&model_path);
            download_stt_model(&cfg.model, asset, &model_path, shutdown).await?;
            let backend = stt::local::LocalStt::load(&model_path, cfg.threads, &cfg.language)
                .context("loading whisper-rs context after re-download")?;
            Ok((backend, cfg.model.clone()))
        }
        Err(e) => Err(e).context("loading whisper-rs context"),
    }
}

/// SHA-verified download with progress log. Shared between happy path and corrupt-cache retry.
async fn download_stt_model(
    model_key: &str,
    asset: &manifest::SttModel,
    model_path: &std::path::Path,
    shutdown: CancellationToken,
) -> Result<()> {
    let client = fetch::FetchClient::new();
    let tmp = cache::tmp_path_for(model_path);
    tracing::info!(
        model = %model_key,
        size_mb = asset.size_bytes / (1024 * 1024),
        "Downloading {}…",
        asset.display_name
    );

    let mut reporter =
        progress::Reporter::new(&format!("Whisper {model_key}"), Some(asset.size_bytes));
    let result = client
        .download_verified(
            &asset.url,
            &asset.sha256,
            &tmp,
            model_path,
            |bytes, total| reporter.update(bytes, total),
            shutdown,
        )
        .await
        .context("downloading local STT model");
    match &result {
        Ok(()) => reporter.finish("done"),
        Err(_) => reporter.finish("failed"),
    }
    result?;
    tracing::info!(model = %model_key, "Model download + verification complete");
    Ok(())
}

/// Probe espeak-ng early; SHA-verified model + voice download; ort load.
#[cfg(feature = "kokoro-local")]
async fn build_kokoro_local(
    model_key: &str,
    voice: &str,
    shutdown: CancellationToken,
) -> Result<tts::Backend, TtsError> {
    // Early probe — no point loading 346 MB ONNX if we can't phonemize.
    if espeak::probe().is_none() {
        return Err(TtsError::Init(
            "espeak-ng not found on PATH — local Kokoro requires it. \
             Install espeak-ng for your OS and open a new terminal. Alternatively set \
             TELEGRAM_TTS_BACKEND=kokoro-remote or none."
                .to_string(),
        ));
    }

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

    // Voices colocate with ONNX in `models/` — `KokoroTts::load` just concatenates.
    let models_dir = cache::models_dir().map_err(|e| TtsError::Init(format!("models dir: {e}")))?;
    cache::reap_stale_tmps(&models_dir)
        .map_err(|e| TtsError::Init(format!("reap stale tmps: {e}")))?;
    let model_file_name = filename_from_url(&model_entry.onnx_url);
    let model_path = cache::model_path(&model_file_name)
        .map_err(|e| TtsError::Init(format!("model path: {e}")))?;
    let voice_file_name = filename_from_url(&voice_asset.url);
    let voice_path = cache::model_path(&voice_file_name)
        .map_err(|e| TtsError::Init(format!("voice path: {e}")))?;

    let client = fetch::FetchClient::new();

    let was_cached = model_path.exists() && voice_path.exists();

    download_kokoro_if_missing(
        &client,
        model_key,
        voice,
        model_entry,
        voice_asset,
        &model_path,
        &voice_path,
        shutdown.clone(),
    )
    .await?;

    // ~500ms cold start → spawn_blocking. Refetch-once on corrupt cache.
    let load_attempt = {
        let model_path_c = model_path.clone();
        let voices_dir_c = models_dir.clone();
        tokio::task::spawn_blocking(move || {
            tts::kokoro::KokoroTts::load(&model_path_c, voices_dir_c)
        })
        .await
        .map_err(|e| TtsError::Init(format!("Kokoro load join: {e}")))?
    };
    let backend = match load_attempt {
        Ok(b) => b,
        Err(load_err) if was_cached => {
            tracing::warn!(
                err = %load_err,
                model = %model_path.display(),
                voice = %voice_path.display(),
                "Cached Kokoro files failed to load — assuming corrupt, re-downloading"
            );
            let _ = std::fs::remove_file(&model_path);
            let _ = std::fs::remove_file(&voice_path);
            download_kokoro_if_missing(
                &client,
                model_key,
                voice,
                model_entry,
                voice_asset,
                &model_path,
                &voice_path,
                shutdown,
            )
            .await?;
            let model_path_c = model_path.clone();
            let voices_dir_c = models_dir;
            tokio::task::spawn_blocking(move || {
                tts::kokoro::KokoroTts::load(&model_path_c, voices_dir_c)
            })
            .await
            .map_err(|e| TtsError::Init(format!("Kokoro load retry join: {e}")))?
            .map_err(|e| TtsError::Init(format!("Kokoro load retry after refetch: {e}")))?
        }
        Err(e) => return Err(TtsError::Init(format!("Kokoro load: {e}"))),
    };

    Ok(tts::Backend::Kokoro(Box::new(backend)))
}

#[cfg(feature = "kokoro-local")]
#[allow(
    clippy::too_many_arguments,
    reason = "keeps the retry path's call-site free of helper-struct plumbing"
)]
async fn download_kokoro_if_missing(
    client: &fetch::FetchClient,
    model_key: &str,
    voice: &str,
    model_entry: &manifest::TtsModel,
    voice_asset: &manifest::VoiceAsset,
    model_path: &std::path::Path,
    voice_path: &std::path::Path,
    shutdown: CancellationToken,
) -> Result<(), TtsError> {
    if !model_path.exists() {
        tracing::info!(
            model = %model_key,
            size_mb = model_entry.onnx_size_bytes / (1024 * 1024),
            "Downloading Kokoro model (~{} MB, one-time)…",
            model_entry.onnx_size_bytes / (1024 * 1024),
        );
        let tmp = cache::tmp_path_for(model_path);
        let mut reporter = progress::Reporter::new(
            &format!("Kokoro {model_key}"),
            Some(model_entry.onnx_size_bytes),
        );
        let result = client
            .download_verified(
                &model_entry.onnx_url,
                &model_entry.onnx_sha256,
                &tmp,
                model_path,
                |bytes, total| reporter.update(bytes, total),
                shutdown.clone(),
            )
            .await
            .map_err(|e| TtsError::Init(format!("download Kokoro model: {e}")));
        match &result {
            Ok(()) => reporter.finish("done"),
            Err(_) => reporter.finish("failed"),
        }
        result?;
        tracing::info!("Kokoro model downloaded + SHA-verified");
    }

    if !voice_path.exists() {
        tracing::info!(voice = %voice, "Downloading Kokoro voice (~510 KB)…");
        let tmp = cache::tmp_path_for(voice_path);
        let mut reporter =
            progress::Reporter::new(&format!("Voice {voice}"), Some(voice_asset.size_bytes));
        let result = client
            .download_verified(
                &voice_asset.url,
                &voice_asset.sha256,
                &tmp,
                voice_path,
                |bytes, total| reporter.update(bytes, total),
                shutdown,
            )
            .await
            .map_err(|e| TtsError::Init(format!("download voice `{voice}`: {e}")));
        match &result {
            Ok(()) => reporter.finish("done"),
            Err(_) => reporter.finish("failed"),
        }
        result?;
    }
    Ok(())
}

/// Dashboard detail — remote URLs are host-only (path could carry secrets).
fn display_detail_for(cfg: &tts::BackendConfig) -> Option<String> {
    match cfg {
        tts::BackendConfig::Say { .. } | tts::BackendConfig::WinRt { .. } => None,
        tts::BackendConfig::KokoroLocal { model, .. } => Some(model.clone()),
        tts::BackendConfig::Remote { url, .. } => Some(redacted_host_from_url(url)),
    }
}

fn redacted_host_from_url(url: &str) -> String {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    after_scheme[..host_end].to_string()
}

fn filename_from_url(url: &str) -> String {
    let no_query = url.split('?').next().unwrap_or(url);
    no_query
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
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
        let off = AudioConfig {
            stt: None,
            tts: None,
        };
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
        assert_eq!(redacted_host_from_url("bare-host"), "bare-host");
    }
}
