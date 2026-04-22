//! Typing-indicator refresh (4 s loop) + RAII guard. All spawns on the
//! shared tracker (invariant 12).

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::telegram::TelegramClient;

const REFRESH: Duration = Duration::from_secs(4);

pub struct TypingGuard {
    cancel: CancellationToken,
}

impl TypingGuard {
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

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Fire-and-forget typing with a wall-clock cap (hook path — no guard).
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
