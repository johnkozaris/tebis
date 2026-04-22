//! Shared "typing…" indicator loop + RAII guard.
//!
//! Telegram's `sendChatAction` shows an indicator for ~5 s then fades.
//! Refresh every 4 s to keep it continuous until cancelled.
//!
//! Use [`TypingGuard`] everywhere that needs to show typing: the guard
//! spawns the refresh loop on the shared `TaskTracker` (so shutdown
//! drains it) and auto-cancels on drop. That centralizes three
//! concerns in one type:
//!
//! 1. CLAUDE.md invariant 12: no bare `tokio::spawn` — all tasks live
//!    on the tracker so `tracker.wait()` drains them.
//! 2. RAII — callers can't forget to cancel the loop when their reply
//!    is ready; dropping the guard is sufficient.
//! 3. Optional wall-clock cap for fire-and-forget use (the hook path
//!    doesn't keep a guard; it trusts the cap).

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::telegram::TelegramClient;

const REFRESH: Duration = Duration::from_secs(4);

/// Drop-to-cancel handle for a typing-indicator refresh loop running
/// on the shared `TaskTracker`. Explicit `cancel()` is equivalent to
/// drop; callers use it when they want the loop to stop a beat before
/// the guard naturally falls out of scope.
pub struct TypingGuard {
    cancel: CancellationToken,
}

impl TypingGuard {
    /// Spawn the refresh loop; cancel on drop. Caller keeps the guard
    /// alive for as long as typing should show. `shutdown` is the
    /// daemon's root cancel token — the guard's inner token is a child
    /// of it, so a SIGTERM cancels typing immediately instead of
    /// waiting for drop (tracker.wait() would otherwise block up to
    /// the next REFRESH tick).
    pub fn start(
        tracker: &TaskTracker,
        tg: Arc<TelegramClient>,
        chat_id: i64,
        shutdown: &CancellationToken,
    ) -> Self {
        let cancel = shutdown.child_token();
        tracker.spawn(indicate(tg, chat_id, cancel.clone()));
        Self { cancel }
    }

    /// Stop the typing loop immediately. Equivalent to `drop(guard)`.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Fire-and-forget variant with a wall-clock cap. Used by the
/// hook-driven reply path: we don't know when the agent's `Stop` will
/// fire, so we show typing for at most `cap` and let go. Sending a
/// real message client-side also clears the indicator, so the cap is
/// only load-bearing when the hook fails silently.
///
/// Both the refresh loop and the deadline timer spawn on `tracker`,
/// and both listen to the daemon's `shutdown` token — so shutdown
/// drain doesn't wait up to `cap` for the timer to fire naturally.
pub fn spawn_with_cap(
    tracker: &TaskTracker,
    tg: Arc<TelegramClient>,
    chat_id: i64,
    cap: Duration,
    shutdown: &CancellationToken,
) {
    let cancel = shutdown.child_token();
    tracker.spawn(indicate(tg, chat_id, cancel.clone()));
    tracker.spawn(async move {
        tokio::select! {
            () = tokio::time::sleep(cap) => cancel.cancel(),
            () = cancel.cancelled() => {}
        }
    });
}

/// Refresh `typing` in `chat_id` until `cancel` fires. Transient API
/// errors are logged at debug and swallowed — a missed refresh just
/// shortens the indicator's lifetime, it's not worth escalating.
pub async fn indicate(tg: Arc<TelegramClient>, chat_id: i64, cancel: CancellationToken) {
    loop {
        if let Err(e) = tg.send_chat_action(chat_id, "typing").await {
            tracing::debug!(err = %e, "typing indicator refresh failed");
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(REFRESH) => {}
        }
    }
}
