//! Per-chat typing-indicator manager. One animation per chat; starting a
//! new one cancels the previous, and `cancel()` clears it before sending
//! the real reply (otherwise the 4 s refresh re-fires after delivery and
//! clients flash "typing…" on top of the message).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::telegram::TelegramClient;

const REFRESH: Duration = Duration::from_secs(4);

/// Skip the first `sendChatAction` if the reply lands within this window.
/// Fast paths (`/list`, `/help`, etc.) reply in <100 ms; without the grace
/// the action would linger on the client for ~5 s after the message.
const STARTUP_GRACE: Duration = Duration::from_millis(250);

pub struct TypingRegistry {
    chats: Mutex<HashMap<i64, CancellationToken>>,
}

impl TypingRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            chats: Mutex::new(HashMap::new()),
        })
    }

    /// Replace any in-flight typing with a fresh capped animation. The new
    /// token is parented under `shutdown` so drain works.
    pub fn start(
        &self,
        tracker: &TaskTracker,
        tg: Arc<TelegramClient>,
        chat_id: i64,
        cap: Duration,
        shutdown: &CancellationToken,
    ) {
        let new_token = shutdown.child_token();
        {
            let mut chats = self.chats.lock().expect("typing registry poisoned");
            if let Some(old) = chats.insert(chat_id, new_token.clone()) {
                old.cancel();
            }
        }
        tracker.spawn(indicate(tg, chat_id, new_token.clone()));
        let cap_token = new_token;
        tracker.spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(cap) => cap_token.cancel(),
                () = cap_token.cancelled() => {}
            }
        });
    }

    /// Called by reply paths so the next refresh tick can't fire after the
    /// message lands.
    pub fn cancel(&self, chat_id: i64) {
        if let Some(tok) = self
            .chats
            .lock()
            .expect("typing registry poisoned")
            .remove(&chat_id)
        {
            tok.cancel();
        }
    }
}

async fn indicate(tg: Arc<TelegramClient>, chat_id: i64, cancel: CancellationToken) {
    // Skip the first action entirely if the reply lands within the grace.
    tokio::select! {
        () = cancel.cancelled() => return,
        () = tokio::time::sleep(STARTUP_GRACE) => {}
    }
    loop {
        // Biased select drops an in-flight send the moment cancel fires,
        // so the action can't land after the reply.
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            r = tg.send_chat_action(chat_id, "typing") => {
                if let Err(e) = r {
                    tracing::debug!(err = %e, "typing indicator refresh failed");
                }
            }
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(REFRESH) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_clears_entry() {
        let reg = TypingRegistry::new();
        let tok = CancellationToken::new();
        reg.chats.lock().unwrap().insert(7, tok.clone());
        reg.cancel(7);
        assert!(tok.is_cancelled());
        assert!(reg.chats.lock().unwrap().get(&7).is_none());
    }

    #[tokio::test]
    async fn cancel_is_noop_for_unknown_chat() {
        let reg = TypingRegistry::new();
        reg.cancel(42);
    }

    #[tokio::test]
    async fn replace_cancels_previous() {
        let reg = TypingRegistry::new();
        let old = CancellationToken::new();
        reg.chats.lock().unwrap().insert(1, old.clone());

        let new = CancellationToken::new();
        if let Some(prev) = reg.chats.lock().unwrap().insert(1, new.clone()) {
            prev.cancel();
        }
        assert!(old.is_cancelled());
        assert!(!new.is_cancelled());
    }
}
