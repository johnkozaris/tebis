//! Inbound-notify listener (via `platform::peer_listener` — UDS on Unix,
//! Named Pipe on Windows) + `Forwarder` sink to Telegram. Invariants
//! 9–12 apply to both transports; see `platform::peer_listener` for
//! how each backend realizes the three-layer peer defense.

mod format;
mod listener;
mod markdown;

pub use listener::spawn;

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

use crate::telegram::TelegramClient;

pub struct NotifyConfig {
    pub socket_path: PathBuf,
    pub chat_id: i64,
}

/// Hook wire format. `kind` renders as a `[tag]` prefix; unknown → no tag.
#[derive(Deserialize, Debug, Clone)]
pub struct Payload {
    pub text: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    #[error("delivery failed: {0}")]
    Delivery(String),
}

/// Test seam: production wires `TelegramForwarder`; tests inject a recorder.
pub trait Forwarder: Send + Sync + 'static {
    fn forward(
        &self,
        payload: Payload,
    ) -> impl std::future::Future<Output = Result<(), ForwardError>> + Send;
}

pub struct TelegramForwarder {
    tg: Arc<TelegramClient>,
    chat_id: i64,
    /// 2 permits cap fan-out under hook storms (Stop + several `SubagentStops`).
    send_sem: Arc<tokio::sync::Semaphore>,
}

impl TelegramForwarder {
    pub fn new(tg: Arc<TelegramClient>, chat_id: i64) -> Self {
        Self {
            tg,
            chat_id,
            send_sem: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
}

impl Forwarder for TelegramForwarder {
    async fn forward(&self, payload: Payload) -> Result<(), ForwardError> {
        let _permit = self
            .send_sem
            .acquire()
            .await
            .map_err(|e| ForwardError::Delivery(format!("semaphore closed: {e}")))?;
        let body = format::body(&payload);
        self.tg
            .send_message(self.chat_id, &body)
            .await
            .map_err(|e| ForwardError::Delivery(e.to_string()))?;
        Ok(())
    }
}
