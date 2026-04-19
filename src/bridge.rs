//! Per-message behavior: rate-limit → permit → parse → execute → reply.
//!
//! Lifted out of `main.rs` so the bridge's *plumbing* (poll loop, tracing,
//! signal handling) and its *behavior* (what a message does) don't share a
//! file. Tests and future changes to command semantics live here; `main.rs`
//! owns only the lifecycle.
//!
//! Concurrency:
//!
//! - **Rate limit first** — cheap check, per-chat GCRA. Rate-limited replies
//!   don't consume a `handler_sem` slot, so a spam burst can't starve real
//!   traffic.
//! - **`handler_sem` afterwards** — global in-flight cap across *all*
//!   handlers that actually touch tmux / Telegram. Bounds subprocess fan-out
//!   regardless of Telegram burst size.
//!
//! Observability: every stage increments the shared [`Metrics`] so the
//! inspect dashboard can report live activity.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Semaphore;

use crate::handler::{self, Response};
use crate::metrics::Metrics;
use crate::security::RateLimiter;
use crate::session::SessionState;
use crate::telegram::TelegramClient;
use crate::tmux::Tmux;

/// Per-handler dependencies. Built fresh by `main`'s poll loop for each
/// inbound update and moved into the spawned task — never cloned.
pub struct HandlerContext {
    pub tg: Arc<TelegramClient>,
    pub tmux: Arc<Tmux>,
    pub session: Arc<SessionState>,
    pub rate_limiter: Arc<RateLimiter>,
    /// Global cap on concurrent handlers doing real work (post-rate-limit).
    /// Bounds tmux subprocess fan-out in a bursty-delivery scenario.
    pub handler_sem: Arc<Semaphore>,
    /// Process start — copied (not cloned) into each handler's `Deps` for
    /// `/status` uptime reporting.
    pub started_at: Instant,
    /// Shared activity counters for the inspect dashboard.
    pub metrics: Arc<Metrics>,
}

/// Entry point for a single inbound message. Returns `()` — errors are
/// converted to Telegram replies inside [`handler::execute`] or logged here
/// so the spawned task never propagates a failure up to the poll loop.
pub async fn handle_update(ctx: HandlerContext, chat_id: i64, message_id: i64, text: String) {
    let handler_start = Instant::now();
    ctx.metrics.record_update_received();

    if let Err(retry_after) = ctx.rate_limiter.check(chat_id) {
        ctx.metrics.record_rate_limited();
        // Round up so sub-second waits don't render as "0s".
        let secs = retry_after.as_secs().max(1);
        let reply = format!("Rate limited. Try again in {secs}s.");
        let _ = ctx.tg.send_message(chat_id, &reply).await;
        return;
    }

    // Acquire a global work-permit *after* the rate-limit check so a burst
    // of rate-limited spam doesn't queue up behind real work. The permit
    // releases on drop at end-of-function — no explicit release needed.
    //
    // `acquire` only fails on close (not done on this path); if we ever add
    // shutdown-close this is where the drain signal would land.
    let Ok(_permit) = ctx.handler_sem.acquire().await else {
        tracing::warn!("handler semaphore closed; dropping update");
        return;
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
            // Body is guaranteed ≤ 4000 chars by `sanitize::wrap_and_truncate`
            // (or by being a short constant) — single sendMessage is sufficient.
            if let Err(e) = ctx.tg.send_message(chat_id, &body).await {
                ctx.metrics.record_handler_error();
                tracing::error!(err = %e, "Failed to send response");
            }
        }
        Response::ReactSuccess => {
            if let Err(e) = ctx.tg.set_message_reaction(chat_id, message_id, "👍").await {
                ctx.metrics.record_handler_error();
                tracing::warn!(err = %e, "Failed to set reaction");
            }
        }
    }

    let duration_ms = u64::try_from(handler_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    ctx.metrics.record_handler_completed(duration_ms);
}
