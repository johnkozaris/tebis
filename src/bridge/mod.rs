//! Per-message behavior: parse → execute → reply.

pub mod handler;
pub mod session;
mod tts_validation;
pub mod typing;
pub mod voice_pref;

use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use handler::Response;
use session::SessionState;
use typing::TypingRegistry;
use voice_pref::VoicePref;

use crate::audio::AudioSubsystem;
use crate::metrics::Metrics;
use crate::platform::multiplexer::Mux;
use crate::sanitize;
use crate::telegram::TelegramClient;

pub enum Payload {
    Text(String),
    Voice {
        file_id: String,
        duration_sec: u32,
        size_bytes: Option<u32>,
    },
}

pub struct HandlerContext {
    pub tg: Arc<TelegramClient>,
    pub tmux: Arc<Mux>,
    pub session: Arc<SessionState>,
    pub started_at: Instant,
    pub metrics: Arc<Metrics>,
    /// Every spawn uses this so shutdown drains them.
    pub tracker: TaskTracker,
    pub shutdown: CancellationToken,
    pub audio: Option<Arc<AudioSubsystem>>,
    /// Required for runtime config writes (`/tts`, inspect Settings).
    pub env_file_path: Option<std::path::PathBuf>,
    pub typing: Arc<TypingRegistry>,
    /// Drives TTS for both sync replies and async hook replies.
    pub voice_pref: Arc<VoicePref>,
}

pub async fn handle_update(ctx: HandlerContext, chat_id: i64, message_id: i64, payload: Payload) {
    let handler_start = Instant::now();
    ctx.metrics.record_update_received();

    // Fire typing up-front so every reply path inherits an active animation.
    // Faster paths (`Response::Text`/`ReactSuccess`) cancel before sending so
    // the 4 s refresh can't flash typing after the message lands.
    ctx.typing.start(
        &ctx.tracker,
        ctx.tg.clone(),
        chat_id,
        SEND_TYPING_CAP,
        &ctx.shutdown,
    );

    let inbound_was_voice = matches!(payload, Payload::Voice { .. });
    ctx.voice_pref.set(chat_id, inbound_was_voice);
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
                    let body = sanitize::escape_html(&reply);
                    ctx.typing.cancel(chat_id);
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
            ctx.typing.cancel(chat_id);
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
                let shutdown = ctx.shutdown.clone();
                ctx.tracker.spawn(async move {
                    synthesize_and_send_voice_detached(
                        &tg, &metrics, &audio, &shutdown, chat_id, &body,
                    )
                    .await;
                });
            }
        }
        Response::ReactSuccess => {
            ctx.typing.cancel(chat_id);
            react_ok(&ctx, chat_id, message_id).await;
        }
        Response::Sent { session: _ } => {
            // Hooks deliver the reply asynchronously; typing stays live until
            // the forwarder cancels it (or `SEND_TYPING_CAP` elapses).
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
            let mut options = String::from("  /tts off\n");
            #[cfg(target_os = "macos")]
            options.push_str("  /tts say\n");
            #[cfg(target_os = "windows")]
            options.push_str("  /tts winrt\n");
            options.push_str("  /tts kokoro-local\n  /tts kokoro-remote");
            let msg = format!(
                "TTS: <code>{}</code>\n\nPick one with:\n{options}",
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
        return Response::Text(format!(
            "Can't switch TTS at runtime: BRIDGE_ENV_FILE isn't set. \
             Set it in the service environment ({}) and restart tebis.",
            platform_service_name(),
        ));
    };

    if value == "kokoro-remote"
        && let Err(msg) = tts_validation::validate_remote_tts_env(env_path)
    {
        return Response::Text(msg);
    }

    let mut updates: Vec<(&str, String)> = vec![("TELEGRAM_TTS_BACKEND", value.to_string())];
    if value == "kokoro-local" {
        match tts_validation::validate_kokoro_local_tts_env() {
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

/// Cap transcript bytes fed into `parse` so long voice
/// notes can't bypass text-size limits.
const MAX_TRANSCRIPT_BYTES: usize = 4000;

const PCM_SAMPLE_RATE: usize = 16_000;

/// Never log transcript text (secrets can land there too).
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
        ctx.metrics.record_stt_failure();
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
    let transcription = audio.transcribe(&pcm, language).await.map_err(|e| {
        ctx.metrics.record_stt_failure();
        format!("Transcription failed: {e}")
    })?;

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

pub(crate) async fn synthesize_and_send_voice_detached(
    tg: &crate::telegram::TelegramClient,
    metrics: &crate::metrics::Metrics,
    audio: &crate::audio::AudioSubsystem,
    shutdown: &tokio_util::sync::CancellationToken,
    chat_id: i64,
    body: &str,
) {
    let plain = strip_html_for_tts(body);
    if plain.trim().is_empty() {
        return;
    }
    let synth_start = std::time::Instant::now();
    // Race synthesize against shutdown so TTS can't keep the bridge alive
    // past the drain window. The text reply was already sent.
    let synth_result = tokio::select! {
        biased;
        () = shutdown.cancelled() => {
            tracing::debug!("shutdown observed before TTS synthesis; skipping voice reply");
            return;
        }
        res = audio.synthesize(&plain) => res,
    };
    let (voice_bytes, duration_sec) = match synth_result {
        Ok(pair) => pair,
        Err(e) => {
            metrics.record_tts_failure();
            tracing::warn!(err = %e, "TTS synthesis failed; text reply already sent");
            return;
        }
    };
    let synth_ms = u64::try_from(synth_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let send_result = tokio::select! {
        biased;
        () = shutdown.cancelled() => {
            tracing::debug!("shutdown observed before sendVoice; skipping voice reply");
            return;
        }
        res = tg.send_voice(chat_id, voice_bytes, Some(duration_sec)) => res,
    };
    if let Err(e) = send_result {
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

fn platform_service_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "launchd"
    }
    #[cfg(target_os = "linux")]
    {
        "systemd"
    }
    #[cfg(target_os = "windows")]
    {
        "Task Scheduler"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "your OS service manager"
    }
}

const SEND_TYPING_CAP: std::time::Duration = std::time::Duration::from_secs(45);

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
