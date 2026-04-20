//! Outbound-notification listener (UDS) with a pluggable [`Forwarder`] sink.
//!
//! Hook scripts push one JSON line per event; the listener parses, hands to
//! the forwarder, and writes a status line back. UDS-only so it's
//! unreachable from the network; mode 0600 + `peer_cred` keeps it local.
//!
//! - `mod.rs` — `spawn` entry + `Forwarder` trait + `TelegramForwarder`
//! - `format.rs` — pure `Payload` → HTML body
//! - `listener.rs` — bind + accept loop + per-connection protocol

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

/// Hook-script wire format. `kind` is the event classification
/// (`"stop"`, `"subagent_stop"`, `"permission_prompt"`, `"idle_prompt"`)
/// which renders as a `[tag]` prefix; unknown kinds render with no tag.
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

/// Listener-facing error. Structured upstream errors (`TelegramError`) are
/// collapsed to a string so the listener doesn't pattern-match on specifics.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    #[error("delivery failed: {0}")]
    Delivery(String),
}

/// Test seam: production wires `TelegramForwarder`, tests inject a recorder.
pub trait Forwarder: Send + Sync + 'static {
    fn forward(
        &self,
        payload: Payload,
    ) -> impl std::future::Future<Output = Result<(), ForwardError>> + Send;
}

pub struct TelegramForwarder {
    tg: Arc<TelegramClient>,
    chat_id: i64,
}

impl TelegramForwarder {
    pub const fn new(tg: Arc<TelegramClient>, chat_id: i64) -> Self {
        Self { tg, chat_id }
    }
}

impl Forwarder for TelegramForwarder {
    async fn forward(&self, payload: Payload) -> Result<(), ForwardError> {
        let body = format::body(&payload);
        self.tg
            .send_message(self.chat_id, &body)
            .await
            .map_err(|e| ForwardError::Delivery(e.to_string()))?;
        Ok(())
    }
}
