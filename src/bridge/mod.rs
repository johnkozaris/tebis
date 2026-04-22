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
use tokio_util::sync::CancellationToken;
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

/// Global cap on concurrent handler tasks. Single-user workload:
/// realistic concurrency is 1–2. 8 bounds subprocess fan-out when
/// Telegram delivers a burst (e.g. queued messages after a phone
/// reconnect). Lives in the consumer module so the policy sits with
/// `HandlerContext::handler_sem`.
pub const MAX_CONCURRENT_HANDLERS: usize = 8;

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
    /// Daemon's root cancel token. Threaded into typing guards + pane
    /// watcher so shutdown drain doesn't wait up to `HOOK_TYPING_CAP`
    /// or the next `REFRESH` for their timers to fire naturally.
    pub shutdown: CancellationToken,
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
        // Literal text is safe, but `send_message` sets parse_mode=HTML,
        // so every body should route through escape_html defensively
        // — if a future change drops in a formatted variable, it won't
        // accidentally enable HTML injection on the rate-limit path.
        let reply = sanitize::escape_html(&format!("Rate limited. Try again in {secs}s."));
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
            let send_ok = match ctx.tg.send_message(chat_id, &body).await {
                Ok(_) => true,
                Err(e) => {
                    ctx.metrics.record_handler_error();
                    tracing::error!(err = %e, "Failed to send response");
                    false
                }
            };
            // TTS only fires when the text reply actually landed —
            // otherwise the user sees "voice succeeded but text failed"
            // which is confusing. Also detached on the tracker so the
            // handler releases its permit immediately (synth + sendVoice
            // can take seconds).
            if send_ok
                && let Some(audio) = ctx.audio.as_ref()
                && audio.should_tts_reply(inbound_was_voice)
            {
                let tg = ctx.tg.clone();
                let metrics = ctx.metrics.clone();
                let audio = audio.clone();
                ctx.tracker.spawn(async move {
                    synthesize_and_send_voice_detached(
                        &tg, &metrics, &audio, chat_id, &body,
                    )
                    .await;
                });
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
                typing::spawn_with_cap(
                    &ctx.tracker,
                    ctx.tg.clone(),
                    chat_id,
                    HOOK_TYPING_CAP,
                    &ctx.shutdown,
                );
            } else if let Some(cfg) = ctx.autoreply.clone() {
                // Auto-reply IS the ack when it produces content —
                // skip the 👍 up front. If the pane has nothing new,
                // `watch_and_forward` falls back to the 👍 on
                // `message_id` itself, so the user always gets one
                // of: reply / reaction / failure log.
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

    // Post-download size check. The pre-download guard above uses
    // `size_bytes` from the `Voice`/`Audio` Bot API field — which is
    // `Option<u32>` and can be absent entirely. When absent we still
    // get the actual bytes from `download_file` (bounded at
    // MAX_FILE_DOWNLOAD_BYTES=50 MiB) but a user who tightened
    // TELEGRAM_STT_MAX_BYTES expects THEIR cap to apply. Enforce here.
    let actual_bytes = u32::try_from(oga_bytes.len()).unwrap_or(u32::MAX);
    if actual_bytes > limits.max_bytes {
        return Err(format!(
            "Voice file is too large ({actual} B > {cap} B cap). Raise TELEGRAM_STT_MAX_BYTES to accept it.",
            actual = actual_bytes,
            cap = limits.max_bytes,
        ));
    }

    // Metadata-only log. Never the transcript.
    tracing::debug!(
        chat_id,
        oga_bytes = oga_bytes.len(),
        duration_sec,
        "Voice downloaded"
    );

    // × 2 on the sample budget covers Opus preskip + trailing silence
    // that whisper ignores. Beyond that we assume adversarial input.
    let max_samples = (limits.max_duration_sec as usize).saturating_mul(16_000).saturating_mul(2);
    let pcm = codec::decode_opus_to_pcm16k(&oga_bytes, max_samples).map_err(|e| {
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
    // whisper.cpp emits special tokens like `[BLANK_AUDIO]`,
    // `[Music]`, `(silence)`, `[AUDIO OUT]` when it can't find speech.
    // Treat them all as empty so the user gets a clean "no speech"
    // error instead of the internal token string.
    let trimmed = text.trim();
    let is_silence_token = trimmed.is_empty()
        || trimmed.starts_with('[')
        || trimmed.starts_with('(')
        || trimmed.eq_ignore_ascii_case("silence");
    if is_silence_token {
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
///
/// Takes primitive shared handles rather than `&HandlerContext` because
/// it runs on the task tracker after the parent handler has already
/// returned and released its concurrency permit — by design. See the
/// spawn-site in `handle_update`.
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
    if let Err(e) = tg.send_voice(chat_id, voice_bytes, Some(duration_sec)).await {
        metrics.record_tts_failure();
        tracing::warn!(err = %e, "sendVoice failed; text reply already sent");
        return;
    }
    metrics.record_tts_success(synth_ms);
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
    // Decode entities. `&amp;` MUST come last: consider input
    // "&amp;lt;" (which should decode to literal "&lt;"). If we
    // decoded `&lt;` first we'd double-decode to "&<". Sentinel
    // approach: swap `&amp;` for U+0001 first, do the others, then
    // swap the sentinel back to `&`. U+0001 Start-of-Heading is a C0
    // control char already stripped by `sanitize::escape_html` on
    // inbound data, so there's no risk of collision with user text.
    let step1 = no_tags.replace("&amp;", &AMP_SENTINEL.to_string());
    let step2 = step1
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    step2.replace(AMP_SENTINEL, "&")
}

/// Placeholder used by `strip_html_for_tts` to protect `&amp;` from
/// double-decoding (see function-level comment).
const AMP_SENTINEL: char = '\u{0001}';

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

#[cfg(test)]
mod strip_html_tests {
    use super::strip_html_for_tts;

    #[test]
    fn strips_pre_and_code_tags() {
        assert_eq!(
            strip_html_for_tts("<pre>hello</pre>"),
            "hello"
        );
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
        // "&amp;lt;" represents the literal text `&lt;`, not `<`. If we
        // decoded &amp; before &lt; we'd get `<` (wrong). The sentinel
        // pass preserves the correct literal.
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

    /// Contract test: every text that goes through `escape_html` (for
    /// Telegram HTML mode) must come back out of `strip_html_for_tts`
    /// as the original string. If someone adds a new entity to
    /// `escape_html` without updating the decoder here, Kokoro will
    /// read the raw `&newent;` tokens aloud — catches that early.
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

    /// Real-body roundtrip: simulate the path used by error / code
    /// responses where the escaped payload is wrapped in `<pre>...</pre>`
    /// before send. The stripper must drop the wrapper tags and decode
    /// entities so TTS speaks the original text, not `"less-than pre
    /// greater-than"`. If someone adds a new wrapper tag (e.g. `<b>`)
    /// without updating `strip_html_for_tts`, this test flags it.
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
