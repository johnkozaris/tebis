//! Per-message behavior: rate-limit → permit → parse → execute → reply.

pub mod autoreply;
pub mod handler;
pub mod session;
pub mod typing;

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use autoreply::AutoreplyConfig;
use handler::Response;
use session::SessionState;

use crate::audio::AudioSubsystem;
use crate::metrics::Metrics;
use crate::platform::multiplexer::Mux;
use crate::sanitize;
use crate::security::RateLimiter;
use crate::telegram::TelegramClient;

pub enum Payload {
    Text(String),
    Voice {
        file_id: String,
        duration_sec: u32,
        size_bytes: Option<u32>,
    },
}

/// Cap on concurrent handlers — bounds subprocess fan-out on bursts.
pub const MAX_CONCURRENT_HANDLERS: usize = 8;

pub struct HandlerContext {
    pub tg: Arc<TelegramClient>,
    pub tmux: Arc<Mux>,
    pub session: Arc<SessionState>,
    pub rate_limiter: Arc<RateLimiter>,
    pub handler_sem: Arc<Semaphore>,
    pub started_at: Instant,
    pub metrics: Arc<Metrics>,
    pub autoreply: Option<Arc<AutoreplyConfig>>,
    /// Invariant 12: every spawn uses this so shutdown drains them.
    pub tracker: TaskTracker,
    pub shutdown: CancellationToken,
    pub audio: Option<Arc<AudioSubsystem>>,
    /// Required for runtime config writes (`/tts`, inspect Settings).
    pub env_file_path: Option<std::path::PathBuf>,
}

pub async fn handle_update(ctx: HandlerContext, chat_id: i64, message_id: i64, payload: Payload) {
    let handler_start = Instant::now();
    ctx.metrics.record_update_received();

    if let Err(retry_after) = ctx.rate_limiter.check(chat_id) {
        ctx.metrics.record_rate_limited();
        let secs = retry_after.as_secs().max(1);
        let reply = sanitize::escape_html(&format!("Rate limited. Try again in {secs}s."));
        let _ = ctx.tg.send_message(chat_id, &reply).await;
        return;
    }

    // Acquire after rate-limit so spam doesn't starve real work.
    let Ok(_permit) = ctx.handler_sem.acquire().await else {
        tracing::warn!("handler semaphore closed; dropping update");
        return;
    };

    let inbound_was_voice = matches!(payload, Payload::Voice { .. });
    let text = match payload {
        Payload::Text(t) => t,
        Payload::Voice {
            file_id,
            duration_sec,
            size_bytes,
        } => {
            ctx.metrics.record_voice_received();
            match transcribe_voice(&ctx, chat_id, &file_id, duration_sec, size_bytes).await {
                Ok(t) => t,
                Err(reply) => {
                    ctx.metrics.record_stt_failure();
                    let body = sanitize::escape_html(&reply);
                    if let Err(e) = ctx.tg.send_message(chat_id, &body).await {
                        ctx.metrics.record_handler_error();
                        tracing::error!(err = %e, "Failed to send voice-error reply");
                    }
                    let duration_ms =
                        u64::try_from(handler_start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    ctx.metrics.record_handler_completed(duration_ms);
                    return;
                }
            }
        }
    };

    let cmd = handler::parse(&text);
    let response = if let handler::Command::Tts(verb) = cmd {
        handle_tts_command(&ctx, &verb)
    } else {
        let deps = handler::Deps {
            tmux: &ctx.tmux,
            session: &ctx.session,
            started_at: ctx.started_at,
        };
        handler::execute(cmd, &deps).await
    };

    match response {
        Response::Text(body) => {
            let send_ok = match ctx.tg.send_message(chat_id, &body).await {
                Ok(_) => true,
                Err(e) => {
                    ctx.metrics.record_handler_error();
                    tracing::error!(err = %e, "Failed to send response");
                    false
                }
            };
            if send_ok
                && let Some(audio) = ctx.audio.as_ref()
                && audio.should_tts_reply(inbound_was_voice)
            {
                let tg = ctx.tg.clone();
                let metrics = ctx.metrics.clone();
                let audio = audio.clone();
                ctx.tracker.spawn(async move {
                    synthesize_and_send_voice_detached(&tg, &metrics, &audio, chat_id, &body).await;
                });
            }
        }
        Response::ReactSuccess => {
            react_ok(&ctx, chat_id, message_id).await;
        }
        Response::Sent { session, baseline } => {
            if ctx.session.is_hooked(&session) {
                typing::spawn_with_cap(
                    &ctx.tracker,
                    ctx.tg.clone(),
                    chat_id,
                    HOOK_TYPING_CAP,
                    &ctx.shutdown,
                );
            } else if let Some(cfg) = ctx.autoreply.clone() {
                ctx.tracker.spawn(autoreply::watch_and_forward(
                    ctx.tracker.clone(),
                    ctx.tg.clone(),
                    ctx.tmux.clone(),
                    session,
                    chat_id,
                    message_id,
                    baseline,
                    cfg,
                    ctx.shutdown.clone(),
                ));
            } else {
                react_ok(&ctx, chat_id, message_id).await;
            }
        }
    }

    let duration_ms = u64::try_from(handler_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    ctx.metrics.record_handler_completed(duration_ms);
}

async fn react_ok(ctx: &HandlerContext, chat_id: i64, message_id: i64) {
    if let Err(e) = ctx.tg.set_message_reaction(chat_id, message_id, "👍").await {
        ctx.metrics.record_handler_error();
        tracing::warn!(err = %e, "Failed to set reaction");
    }
}

/// `/tts` verb dispatcher. Mutative verbs write env + trigger graceful restart.
/// Native backends are rejected on the wrong OS so users can't brick the config.
fn handle_tts_command(ctx: &HandlerContext, v: &handler::TtsVerb) -> Response {
    use handler::TtsVerb;
    use sanitize::escape_html;

    // `"none"` from the subsystem maps to `"off"` in user-facing copy.
    let current_label = match ctx.audio.as_ref().map(|a| a.tts_backend_kind()) {
        None | Some("none") => "off",
        Some(other) => other,
    };

    match v {
        TtsVerb::Status => {
            let msg = format!(
                "TTS: <code>{}</code>\n\nPick one with:\n  /tts off\n  /tts say\n  /tts winrt\n  /tts kokoro-local\n  /tts kokoro-remote",
                escape_html(current_label),
            );
            Response::Text(msg)
        }
        TtsVerb::Unknown(got) => {
            let msg = format!(
                "Unknown /tts argument: <code>{}</code>\n\nValid: off, say, winrt, kokoro-local, kokoro-remote, status.",
                escape_html(got),
            );
            Response::Text(msg)
        }
        #[cfg(not(target_os = "macos"))]
        TtsVerb::Say => Response::Text(
            "/tts say is macOS-only — try winrt on Windows, or kokoro-local/kokoro-remote."
                .to_string(),
        ),
        #[cfg(target_os = "macos")]
        TtsVerb::Say => switch_tts_backend(ctx, "say"),
        #[cfg(not(target_os = "windows"))]
        TtsVerb::WinRt => Response::Text(
            "/tts winrt is Windows-only — try say on macOS, or kokoro-local/kokoro-remote."
                .to_string(),
        ),
        #[cfg(target_os = "windows")]
        TtsVerb::WinRt => switch_tts_backend(ctx, "winrt"),
        TtsVerb::Off => switch_tts_backend(ctx, "none"),
        TtsVerb::KokoroLocal => switch_tts_backend(ctx, "kokoro-local"),
        TtsVerb::KokoroRemote => switch_tts_backend(ctx, "kokoro-remote"),
    }
}

/// Persist `TELEGRAM_TTS_BACKEND=<value>` and trigger a graceful restart. `kokoro-local`
/// also probes + writes `ORT_DYLIB_PATH` (ort's dyld search misses /opt/homebrew/lib).
fn switch_tts_backend(ctx: &HandlerContext, value: &str) -> Response {
    let Some(env_path) = ctx.env_file_path.as_ref() else {
        return Response::Text(
            "Can't switch TTS at runtime: BRIDGE_ENV_FILE isn't set. \
             Set it in the service environment (Task Scheduler, systemd, or launchd) \
             and restart tebis."
                .to_string(),
        );
    };

    if value == "kokoro-remote"
        && let Err(msg) = validate_remote_tts_env(env_path)
    {
        return Response::Text(msg);
    }

    let mut updates: Vec<(&str, String)> = vec![("TELEGRAM_TTS_BACKEND", value.to_string())];
    if value == "kokoro-local" {
        match validate_kokoro_local_tts_env() {
            Ok(ort_path) => {
                updates.push(("ORT_DYLIB_PATH", ort_path.to_string_lossy().into_owned()))
            }
            Err(msg) => return Response::Text(msg),
        }
    }

    if let Err(e) = crate::env_file::upsert_keys(env_path, &updates) {
        ctx.metrics.record_handler_error();
        tracing::error!(err = %e, "/tts: env-file write failed");
        return Response::Text(
            "Failed to write env file — see server logs. TTS unchanged.".to_string(),
        );
    }

    // Best-effort stale cleanup; non-Kokoro paths ignore this key.
    if value != "kokoro-local"
        && let Err(e) = crate::env_file::remove_keys(env_path, &["ORT_DYLIB_PATH"])
    {
        tracing::warn!(err = %e, "/tts: failed to clear stale ORT_DYLIB_PATH");
    }

    tracing::warn!(
        new_backend = %value,
        path = %env_path.display(),
        "/tts: env updated, scheduling graceful restart"
    );
    crate::shutdown::schedule_graceful_restart(&ctx.shutdown);
    let msg = format!(
        "TTS → <code>{}</code>. Restarting in ~300 ms to apply.",
        sanitize::escape_html(value),
    );
    Response::Text(msg)
}

fn validate_kokoro_local_tts_env() -> Result<std::path::PathBuf, String> {
    #[cfg(not(feature = "kokoro-local"))]
    {
        Err(
            "Can't switch to <code>kokoro-local</code>: this binary was built without the \
             <code>kokoro-local</code> cargo feature. Rebuild with \
             <code>cargo build --features kokoro-local</code>, or use \
             <code>kokoro-remote</code>."
                .to_string(),
        )
    }

    #[cfg(feature = "kokoro-local")]
    {
        if crate::audio::espeak::probe().is_none() {
            return Err(kokoro_local_missing_espeak_msg());
        }
        crate::setup::onnxruntime::probe().ok_or_else(kokoro_local_missing_ort_msg)
    }
}

#[cfg(feature = "kokoro-local")]
fn kokoro_local_missing_espeak_msg() -> String {
    #[cfg(windows)]
    {
        "Can't switch to <code>kokoro-local</code>: <code>espeak-ng</code> is not on \
         PATH. Windows Kokoro-local is manual/Advanced only; install espeak-ng, \
         open a new terminal, then retry. <code>kokoro-remote</code> or \
         <code>winrt</code> are the recommended Windows paths."
            .to_string()
    }
    #[cfg(not(windows))]
    {
        "Can't switch to <code>kokoro-local</code>: <code>espeak-ng</code> is not on \
         PATH. Install it with your OS package manager or rerun <code>tebis setup</code>."
            .to_string()
    }
}

#[cfg(feature = "kokoro-local")]
fn kokoro_local_missing_ort_msg() -> String {
    #[cfg(windows)]
    {
        "Can't switch to <code>kokoro-local</code>: <code>onnxruntime.dll</code> is not \
         on any known path. Set <code>ORT_DYLIB_PATH=C:\\path\\to\\onnxruntime.dll</code> \
         or place it under <code>%LOCALAPPDATA%\\Programs\\onnxruntime\\lib\\</code> or \
         <code>%ProgramFiles%\\onnxruntime\\lib\\</code>, then retry."
            .to_string()
    }
    #[cfg(not(windows))]
    {
        "Can't switch to <code>kokoro-local</code>: <code>libonnxruntime</code> is not \
         on any known path. Run <code>tebis setup</code> or set \
         <code>ORT_DYLIB_PATH=/path/to/libonnxruntime</code>, then retry."
            .to_string()
    }
}

fn validate_remote_tts_env(env_path: &Path) -> Result<(), String> {
    let url = read_env_key_for_tts(env_path, "TELEGRAM_TTS_REMOTE_URL")?;
    let Some(url) = url.filter(|s| !s.trim().is_empty()) else {
        return Err("Can't switch to <code>kokoro-remote</code>: set \
             <code>TELEGRAM_TTS_REMOTE_URL=https://...</code> in the env file \
             or run <code>tebis setup</code> first."
            .to_string());
    };

    let allow_http = match read_env_key_for_tts(env_path, "TELEGRAM_TTS_REMOTE_ALLOW_HTTP")? {
        Some(raw) => crate::env_file::parse_toggle(&raw)
            .map_err(|e| {
                format!(
                    "Can't switch to <code>kokoro-remote</code>: \
                     <code>TELEGRAM_TTS_REMOTE_ALLOW_HTTP</code> is invalid: <code>{}</code>.",
                    sanitize::escape_html(&e.to_string())
                )
            })?
            .unwrap_or(false),
        None => false,
    };

    if let Some(raw) = read_env_key_for_tts(env_path, "TELEGRAM_TTS_REMOTE_TIMEOUT_SEC")? {
        let timeout_sec = raw.parse::<u32>().map_err(|_| {
            "Can't switch to <code>kokoro-remote</code>: \
             <code>TELEGRAM_TTS_REMOTE_TIMEOUT_SEC</code> must be a positive integer."
                .to_string()
        })?;
        if !(1..=300).contains(&timeout_sec) {
            return Err("Can't switch to <code>kokoro-remote</code>: \
                 <code>TELEGRAM_TTS_REMOTE_TIMEOUT_SEC</code> must be 1..=300."
                .to_string());
        }
    }

    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("https://") || allow_http && lower.starts_with("http://")) {
        return Err("Can't switch to <code>kokoro-remote</code>: \
         <code>TELEGRAM_TTS_REMOTE_URL</code> must start with <code>https://</code> \
         (or set <code>TELEGRAM_TTS_REMOTE_ALLOW_HTTP=true</code> for LAN HTTP)."
            .to_string());
    }

    Ok(())
}

fn read_env_key_for_tts(env_path: &Path, key: &str) -> Result<Option<String>, String> {
    crate::env_file::read_key(env_path, key).map_err(|e| {
        format!(
            "Can't switch TTS at runtime: failed to read env file: <code>{}</code>.",
            sanitize::escape_html(&e.to_string())
        )
    })
}

/// Invariant 18: cap transcript bytes fed into `parse` so long voice
/// notes can't bypass text-size limits.
const MAX_TRANSCRIPT_BYTES: usize = 4000;

const PCM_SAMPLE_RATE: usize = 16_000;

/// Invariant 5: never log transcript text (secrets can land there too).
async fn transcribe_voice(
    ctx: &HandlerContext,
    chat_id: i64,
    file_id: &str,
    duration_sec: u32,
    size_bytes: Option<u32>,
) -> Result<String, String> {
    use crate::audio::codec;

    let Some(audio) = ctx.audio.as_ref() else {
        return Err(
            "Voice messages aren't enabled. Set TELEGRAM_STT=on and restart tebis.".to_string(),
        );
    };
    let Some(limits) = audio.stt_limits() else {
        return Err("Voice transcription is unavailable right now.".to_string());
    };

    if duration_sec > limits.max_duration_sec {
        return Err(format!(
            "Voice message is too long ({duration}s > {cap}s cap). Send a shorter clip or raise TELEGRAM_STT_MAX_DURATION_SEC.",
            duration = duration_sec,
            cap = limits.max_duration_sec,
        ));
    }
    if let Some(bytes) = size_bytes
        && bytes > limits.max_bytes
    {
        return Err(format!(
            "Voice file is too large ({bytes} B > {cap} B cap). Raise TELEGRAM_STT_MAX_BYTES to accept it.",
            cap = limits.max_bytes,
        ));
    }

    let file = ctx
        .tg
        .get_file(file_id)
        .await
        .map_err(|e| format!("Could not fetch voice file: {e}"))?;
    let Some(path) = file.file_path else {
        return Err(
            "Voice file expired on Telegram's side (>1 h since upload). Resend it.".to_string(),
        );
    };

    let oga_bytes = ctx
        .tg
        .download_file(&path)
        .await
        .map_err(|e| format!("Voice download failed: {e}"))?;

    let actual_bytes = u32::try_from(oga_bytes.len()).unwrap_or(u32::MAX);
    if actual_bytes > limits.max_bytes {
        return Err(format!(
            "Voice file is too large ({actual} B > {cap} B cap). Raise TELEGRAM_STT_MAX_BYTES to accept it.",
            actual = actual_bytes,
            cap = limits.max_bytes,
        ));
    }

    tracing::debug!(
        chat_id,
        oga_bytes = oga_bytes.len(),
        duration_sec,
        "Voice downloaded"
    );

    // ×2 sample budget covers Opus preskip + trailing silence whisper ignores.
    let max_samples = (limits.max_duration_sec as usize)
        .saturating_mul(16_000)
        .saturating_mul(2);
    let pcm = codec::decode_opus_to_pcm16k(&oga_bytes, max_samples).map_err(|e| {
        format!("Voice decode failed: {e}. Tebis only accepts OGG/Opus voice notes — music files in other formats aren't supported.")
    })?;

    let actual_duration_sec = u32::try_from(pcm.len() / PCM_SAMPLE_RATE).unwrap_or(u32::MAX);
    if actual_duration_sec > limits.max_duration_sec {
        return Err(format!(
            "Voice message is longer than claimed ({actual}s decoded > {cap}s cap).",
            actual = actual_duration_sec,
            cap = limits.max_duration_sec,
        ));
    }

    let language = audio.stt_language().unwrap_or("");
    let transcription = audio
        .transcribe(&pcm, language)
        .await
        .map_err(|e| format!("Transcription failed: {e}"))?;

    let mut text = transcription.text;
    // whisper.cpp emits `[BLANK_AUDIO]`/`[Music]`/`(silence)` when no speech.
    let trimmed = text.trim();
    let is_silence_token = trimmed.is_empty()
        || trimmed.starts_with('[')
        || trimmed.starts_with('(')
        || trimmed.eq_ignore_ascii_case("silence");
    if is_silence_token {
        return Err("Could not transcribe voice message (no speech detected).".to_string());
    }
    if text.len() > MAX_TRANSCRIPT_BYTES {
        let mut end = MAX_TRANSCRIPT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }

    tracing::debug!(
        chat_id,
        duration_ms = transcription.duration_ms,
        transcript_bytes = text.len(),
        "Transcription complete"
    );
    ctx.metrics.record_stt_success(transcription.duration_ms);
    Ok(text)
}

async fn synthesize_and_send_voice_detached(
    tg: &crate::telegram::TelegramClient,
    metrics: &crate::metrics::Metrics,
    audio: &crate::audio::AudioSubsystem,
    chat_id: i64,
    body: &str,
) {
    let plain = strip_html_for_tts(body);
    if plain.trim().is_empty() {
        return;
    }
    let synth_start = std::time::Instant::now();
    let (voice_bytes, duration_sec) = match audio.synthesize(&plain).await {
        Ok(pair) => pair,
        Err(e) => {
            metrics.record_tts_failure();
            tracing::warn!(err = %e, "TTS synthesis failed; text reply already sent");
            return;
        }
    };
    let synth_ms = u64::try_from(synth_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Err(e) = tg
        .send_voice(chat_id, voice_bytes, Some(duration_sec))
        .await
    {
        metrics.record_tts_failure();
        tracing::warn!(err = %e, "sendVoice failed; text reply already sent");
        return;
    }
    metrics.record_tts_success(synth_ms);
}

/// Strip `<pre>`/`<code>` wrappers and decode entities from `escape_html`. Sentinel-swap
/// `&amp;` to avoid double-decoding inputs like `&amp;lt;` (must stay `&lt;`, not `<`).
fn strip_html_for_tts(body: &str) -> String {
    let no_tags = body
        .replace("<pre>", "")
        .replace("</pre>", "")
        .replace("<code>", "")
        .replace("</code>", "");
    let step1 = no_tags.replace("&amp;", &AMP_SENTINEL.to_string());
    let step2 = step1
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    step2.replace(AMP_SENTINEL, "&")
}

const AMP_SENTINEL: char = '\u{0001}';

const HOOK_TYPING_CAP: std::time::Duration = std::time::Duration::from_secs(20);

#[cfg(test)]
mod strip_html_tests {
    use super::strip_html_for_tts;

    #[test]
    fn strips_pre_and_code_tags() {
        assert_eq!(strip_html_for_tts("<pre>hello</pre>"), "hello");
        assert_eq!(
            strip_html_for_tts("before <code>mid</code> after"),
            "before mid after"
        );
    }

    #[test]
    fn decodes_basic_entities() {
        assert_eq!(strip_html_for_tts("&lt;tag&gt;"), "<tag>");
        assert_eq!(strip_html_for_tts("&quot;quoted&quot;"), "\"quoted\"");
        assert_eq!(strip_html_for_tts("it&#39;s"), "it's");
        assert_eq!(strip_html_for_tts("a &amp; b"), "a & b");
    }

    #[test]
    fn amp_decoded_last_avoids_double_decode() {
        assert_eq!(strip_html_for_tts("&amp;lt;"), "&lt;");
        assert_eq!(strip_html_for_tts("&amp;amp;"), "&amp;");
    }

    #[test]
    fn handles_empty_and_unescaped() {
        assert_eq!(strip_html_for_tts(""), "");
        assert_eq!(
            strip_html_for_tts("plain text with & and < intact"),
            "plain text with & and < intact"
        );
    }

    #[test]
    fn escape_then_strip_is_identity() {
        for input in [
            "plain",
            "a & b",
            "1 < 2 > 0",
            "\"quote\" and 'apos'",
            "mixed & < > \" ' all",
            "",
            "long-ish text with multiple & characters & repeats",
        ] {
            let escaped = crate::sanitize::escape_html(input);
            let round = strip_html_for_tts(&escaped);
            assert_eq!(round, input, "round-trip failed for {input:?}");
        }
    }

    #[test]
    fn wrapped_body_roundtrips_to_original() {
        for raw in [
            "a & b",
            "error: 1 < 2",
            "multi\nline\npayload",
            "\"quoted\" & 'apos'",
        ] {
            let escaped = crate::sanitize::escape_html(raw);
            let wrapped = format!("<pre>{escaped}</pre>");
            let spoken = strip_html_for_tts(&wrapped);
            assert_eq!(spoken, raw, "pre-wrapper roundtrip failed for {raw:?}");

            let code_wrapped = format!("prefix <code>{escaped}</code> suffix");
            let spoken = strip_html_for_tts(&code_wrapped);
            assert_eq!(
                spoken,
                format!("prefix {raw} suffix"),
                "code-wrapper roundtrip failed for {raw:?}"
            );
        }
    }
}

#[cfg(test)]
mod tts_switch_tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::validate_remote_tts_env;

    fn env_file(body: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tebis-tts-remote-{pid}-{nonce:x}-{seq}",
            pid = std::process::id(),
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("env");
        fs::write(&path, body).unwrap();
        path
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn kokoro_remote_switch_requires_url() {
        let path = env_file("TELEGRAM_TTS_BACKEND=none\n");
        let err = validate_remote_tts_env(&path).unwrap_err();
        assert!(err.contains("TELEGRAM_TTS_REMOTE_URL"));
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_accepts_https_url() {
        let path = env_file("TELEGRAM_TTS_REMOTE_URL=https://kokoro.example.com\n");
        validate_remote_tts_env(&path).unwrap();
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_rejects_http_by_default() {
        let path = env_file("TELEGRAM_TTS_REMOTE_URL=http://127.0.0.1:8880\n");
        let err = validate_remote_tts_env(&path).unwrap_err();
        assert!(err.contains("https://"));
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_allows_http_when_enabled() {
        let path = env_file(
            "TELEGRAM_TTS_REMOTE_URL=http://127.0.0.1:8880\n\
             TELEGRAM_TTS_REMOTE_ALLOW_HTTP=true\n",
        );
        validate_remote_tts_env(&path).unwrap();
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_rejects_invalid_allow_http_toggle() {
        let path = env_file(
            "TELEGRAM_TTS_REMOTE_URL=http://127.0.0.1:8880\n\
             TELEGRAM_TTS_REMOTE_ALLOW_HTTP=maybe\n",
        );
        let err = validate_remote_tts_env(&path).unwrap_err();
        assert!(err.contains("TELEGRAM_TTS_REMOTE_ALLOW_HTTP"));
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_rejects_invalid_timeout() {
        let path = env_file(
            "TELEGRAM_TTS_REMOTE_URL=https://kokoro.example.com\n\
             TELEGRAM_TTS_REMOTE_TIMEOUT_SEC=slow\n",
        );
        let err = validate_remote_tts_env(&path).unwrap_err();
        assert!(err.contains("TELEGRAM_TTS_REMOTE_TIMEOUT_SEC"));
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_rejects_out_of_range_timeout() {
        let path = env_file(
            "TELEGRAM_TTS_REMOTE_URL=https://kokoro.example.com\n\
             TELEGRAM_TTS_REMOTE_TIMEOUT_SEC=301\n",
        );
        let err = validate_remote_tts_env(&path).unwrap_err();
        assert!(err.contains("1..=300"));
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_accepts_timeout_range_edges() {
        for timeout in ["1", "300"] {
            let path = env_file(&format!(
                "TELEGRAM_TTS_REMOTE_URL=https://kokoro.example.com\n\
                 TELEGRAM_TTS_REMOTE_TIMEOUT_SEC={timeout}\n"
            ));
            validate_remote_tts_env(&path).unwrap();
            cleanup(&path);
        }
    }
}
