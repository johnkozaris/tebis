//! Per-message behavior: rate-limit → permit → parse → execute → reply.
//!
//! `main.rs` owns the lifecycle; everything that turns an inbound update
//! into a tmux side effect and a reply lives here.
//!
//! The auto-reply path is the "generic reply-back-to-Telegram" mechanism
//! we control via tmux (no per-project hooks needed). After a successful
//! send, we poll `capture-pane` until the normalized content stops
//! changing ("settle"), then forward the tail. Works for Claude, Aider,
//! Copilot CLI, any TUI — the only input is the pane buffer.

pub mod autoreply;
pub mod handler;
pub mod session;
pub mod typing;

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;

use autoreply::AutoreplyConfig;
use handler::Response;
use session::SessionState;

use crate::audio::AudioSubsystem;
use crate::metrics::Metrics;
use crate::sanitize;
use crate::security::RateLimiter;
use crate::telegram::TelegramClient;
use crate::tmux::Tmux;

/// What kind of content tebis received in one Telegram update. Text
/// arrives already decoded; voice/audio messages arrive as an opaque
/// `file_id` that needs `getFile` + `downloadFile` + OGG/Opus decode
/// + STT before they can drive the handler.
pub enum Payload {
    Text(String),
    Voice {
        file_id: String,
        duration_sec: u32,
        size_bytes: Option<u32>,
    },
}

/// Per-handler dependencies. Fresh per inbound update, moved into the task.
pub struct HandlerContext {
    pub tg: Arc<TelegramClient>,
    pub tmux: Arc<Tmux>,
    pub session: Arc<SessionState>,
    pub rate_limiter: Arc<RateLimiter>,
    /// Global cap on concurrent handlers. Bounds tmux subprocess fan-out
    /// when Telegram delivers a burst.
    pub handler_sem: Arc<Semaphore>,
    pub started_at: Instant,
    pub metrics: Arc<Metrics>,
    /// Pane-settle auto-reply config. `None` disables the feature.
    pub autoreply: Option<Arc<AutoreplyConfig>>,
    /// Shared task tracker. Every background task we spawn (typing
    /// indicator, pane-settle watcher) goes here so `tracker.wait()`
    /// at shutdown drains them deterministically. Violating this was
    /// CLAUDE.md invariant 12.
    pub tracker: TaskTracker,
    /// `None` when the user has `TELEGRAM_STT=off` (default). When
    /// present, voice/audio payloads get transcribed in-process and
    /// fed through the text handler.
    pub audio: Option<Arc<AudioSubsystem>>,
}

/// Entry point for one inbound message. Never propagates errors — the
/// spawned task is the terminal of the failure channel.
pub async fn handle_update(
    ctx: HandlerContext,
    chat_id: i64,
    message_id: i64,
    payload: Payload,
) {
    let handler_start = Instant::now();
    ctx.metrics.record_update_received();

    if let Err(retry_after) = ctx.rate_limiter.check(chat_id) {
        ctx.metrics.record_rate_limited();
        let secs = retry_after.as_secs().max(1);
        let reply = format!("Rate limited. Try again in {secs}s.");
        let _ = ctx.tg.send_message(chat_id, &reply).await;
        return;
    }

    // Acquire the work-permit AFTER rate-limit so spam doesn't starve
    // real work. Permit releases on drop at end-of-function.
    let Ok(_permit) = ctx.handler_sem.acquire().await else {
        tracing::warn!("handler semaphore closed; dropping update");
        return;
    };

    // Payload dispatch. Voice/audio goes through STT first and then
    // re-enters the text path with the transcribed string — all
    // downstream code is unchanged from the text-only era.
    //
    // `inbound_was_voice` drives whether the outbound `Response::Text`
    // also gets synthesized + sent as a voice reply.
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
                    // Error bodies go through escape_html (parse_mode=HTML).
                    // Transcript text itself, when we do send something, is
                    // handed to `handler::parse` below — same path as typed.
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
    let deps = handler::Deps {
        tmux: &ctx.tmux,
        session: &ctx.session,
        started_at: ctx.started_at,
    };
    let response = handler::execute(cmd, &deps).await;

    match response {
        Response::Text(body) => {
            if let Err(e) = ctx.tg.send_message(chat_id, &body).await {
                ctx.metrics.record_handler_error();
                tracing::error!(err = %e, "Failed to send response");
            }
            // TTS: if the user's inbound was a voice note (or they opted
            // into `respond_to_all`), also send a voice reply. The
            // synthesize + sendVoice is best-effort — a failure here
            // does NOT retry or escalate; the user already has the text.
            if let Some(audio) = ctx.audio.as_ref()
                && audio.should_tts_reply(inbound_was_voice)
            {
                synthesize_and_send_voice(&ctx, chat_id, &body).await;
            }
        }
        Response::ReactSuccess => {
            react_ok(&ctx, chat_id, message_id).await;
        }
        Response::Sent { session, baseline } => {
            if ctx.session.is_hooked(&session) {
                // Reply arrives via the agent's Stop hook → UDS →
                // notify listener. Show "typing…" with a deadline so
                // the user sees feedback until the real message lands.
                //
                // No 👍 fallback here. In hook mode the user expects
                // prose back, so a thumbs-up reaction is the wrong
                // signal — it implies "delivered successfully" when
                // the actual state is "delivered but the hook never
                // replied." If the typing indicator stops without a
                // message, the user investigates via /read or /status.
                typing::spawn_with_cap(&ctx.tracker, ctx.tg.clone(), chat_id, HOOK_TYPING_CAP);
            } else if let Some(cfg) = ctx.autoreply.clone() {
                // Auto-reply IS the ack — skip the 👍 so the user isn't
                // getting a reaction plus a reply plus typing dots.
                ctx.tracker.spawn(autoreply::watch_and_forward(
                    ctx.tracker.clone(),
                    ctx.tg.clone(),
                    ctx.tmux.clone(),
                    session,
                    chat_id,
                    baseline,
                    cfg,
                ));
            } else {
                // No auto-reply configured → the 👍 is the only signal
                // that we delivered. Keep it.
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

/// Maximum transcript **bytes** (not chars) we'll feed into
/// `handler::parse`. Matches `TELEGRAM_MAX_OUTPUT_CHARS`'s upper bound
/// — a noisy long recording should not be able to paste 100 KiB of text
/// into tmux and bypass the existing plumbing limits (proposed
/// invariant 19). Named "BYTES" because `text.len()` is bytes; the
/// config key uses "CHARS" for historical reasons.
const MAX_TRANSCRIPT_BYTES: usize = 4000;

/// Samples per second the Opus decoder emits (we configure it at 16 kHz
/// to match whisper-rs input). Used for the post-decode duration sanity
/// check below.
const PCM_SAMPLE_RATE: usize = 16_000;

/// Voice/audio dispatch: downloads the file from Telegram, decodes
/// OGG/Opus → PCM, runs whisper-rs, returns either the transcript (to
/// feed into the text path) or an already-user-facing error message
/// (caller escapes HTML and sends).
///
/// Never logs the transcript text — [CLAUDE.md invariant 5] applies to
/// voice-derived text exactly as it does to `message.text`.
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
        // AudioSubsystem present but STT off inside — shouldn't happen
        // given we only construct the subsystem when enabled, but be
        // defensive: any None here is a misconfiguration, not user error.
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
            "Voice file expired on Telegram's side (>1 h since upload). Resend it."
                .to_string(),
        );
    };

    let oga_bytes = ctx
        .tg
        .download_file(&path)
        .await
        .map_err(|e| format!("Voice download failed: {e}"))?;

    // Metadata-only log. Never the transcript.
    tracing::debug!(
        chat_id,
        oga_bytes = oga_bytes.len(),
        duration_sec,
        "Voice downloaded"
    );

    let pcm = codec::decode_opus_to_pcm16k(&oga_bytes).map_err(|e| {
        format!("Voice decode failed: {e}. Tebis only accepts OGG/Opus voice notes — music files in other formats aren't supported.")
    })?;

    // Defense-in-depth: `duration_sec` above is sender-supplied per the
    // Telegram Bot API (not server-verified). A malicious client can
    // lie about duration to bypass the pre-download cap. After we have
    // real PCM samples, compute actual duration from sample count and
    // re-check. Opus decode is cheap (~20x real-time), so this adds
    // negligible latency for honest clients and immediately bails on
    // exploit attempts before we pay for whisper inference.
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
    if text.trim().is_empty() {
        return Err("Could not transcribe voice message (no speech detected).".to_string());
    }
    if text.len() > MAX_TRANSCRIPT_BYTES {
        // Char-boundary-safe truncation — `text.truncate` panics on
        // multi-byte boundaries. At most 3 iterations (UTF-8 max 4 bytes).
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

/// Synthesize `body` → OGG/Opus → `sendVoice`. Best-effort; logs and
/// records a metric on failure but never retries (user already has the
/// text reply). HTML in the body (from `escape_html`) is stripped to
/// plain text before synthesis so the TTS engine doesn't read `&lt;`
/// and friends aloud. Tebis's outbound text is simple — wrapping in
/// `<pre>`/`<code>` for the /read path is the main offender — so a
/// straightforward "drop angle-bracket tags" pass suffices.
async fn synthesize_and_send_voice(ctx: &HandlerContext, chat_id: i64, body: &str) {
    let Some(audio) = ctx.audio.as_ref() else {
        return;
    };
    let plain = strip_html_for_tts(body);
    if plain.trim().is_empty() {
        return;
    }
    let voice_bytes = match audio.synthesize(&plain).await {
        Ok(b) => b,
        Err(e) => {
            ctx.metrics.record_tts_failure();
            tracing::warn!(err = %e, "TTS synthesis failed; text reply already sent");
            return;
        }
    };
    let duration_guess = u32::try_from(voice_bytes.len() / (2_000)).unwrap_or(0);
    if let Err(e) = ctx.tg.send_voice(chat_id, voice_bytes, Some(duration_guess)).await {
        ctx.metrics.record_tts_failure();
        tracing::warn!(err = %e, "sendVoice failed; text reply already sent");
        return;
    }
    ctx.metrics.record_tts_success();
}

/// Minimal HTML-strip for TTS input. The bodies we send go through
/// `sanitize::escape_html` (invariant 4) so `<` becomes `&lt;`, but we
/// also wrap tmux output in `<pre>` / `<code>` tags on some paths.
/// For synthesis we want the inner text verbatim and no entity names.
fn strip_html_for_tts(body: &str) -> String {
    // Drop obvious tags; decode common entities. This isn't a general
    // HTML parser — the only tags we produce are `<pre>` and `<code>`
    // (both single-word, no attributes) and we escape everything else.
    let no_tags = body
        .replace("<pre>", "")
        .replace("</pre>", "")
        .replace("<code>", "")
        .replace("</code>", "");
    no_tags
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Maximum wall-clock the typing indicator will refresh on the
/// hook-driven reply path. Once the real reply arrives (via the
/// notify listener → `send_message`), Telegram clients auto-clear
/// the indicator. If the hook never delivers, we stop pinging after
/// this cap so the chat doesn't show typing-dots indefinitely.
///
/// 20 s balances:
/// - **Typing-on-phone patience**: 45 s of no-content typing-dots
///   reads as "hung". 20 s is long enough for most Claude turns,
///   short enough that silent failures surface fast.
/// - **Slow tool loops**: if Claude takes longer than 20 s, the
///   typing indicator stops but the real reply still lands when the
///   hook fires — we just don't drive typing past the cap. A user
///   who wants confirmation can `/read` the pane or `/status`.
const HOOK_TYPING_CAP: std::time::Duration = std::time::Duration::from_secs(20);
