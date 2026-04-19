//! Outbound-notification listener with a pluggable delivery sink.
//!
//! # Why this exists
//!
//! The bridge is long-poll-only for inbound (Telegram → tmux). For outbound
//! (local process → Telegram), we expose a Unix-domain socket that scripts
//! on the same machine — notably Claude Code's `Stop`, `SubagentStop`, and
//! `Notification` hooks — push a JSON line into, which we forward as a
//! Telegram message.
//!
//! UDS (not TCP) is the whole point: the listener is unreachable over the
//! network, period. File permissions (0600) rule out other local users.
//!
//! # Module layout
//!
//! - `mod.rs` (this file) — public `spawn` entrypoint, the [`Forwarder`]
//!   trait (test seam), [`TelegramForwarder`] (the production sink), the
//!   [`Payload`] wire type.
//! - `format.rs` — pure HTML-body formatting. No I/O, heavy unit tests.
//! - `listener.rs` — UDS bind, accept loop, per-connection protocol.
//!
//! # Lifecycle
//!
//! ## Startup
//! 1. [`crate::config::Config::from_env`] reads `NOTIFY_CHAT_ID` and
//!    (optionally) `NOTIFY_SOCKET_PATH`.
//! 2. `main` calls [`spawn`] with the socket path and an [`Arc<Forwarder>`].
//! 3. [`spawn`] unlinks any stale socket, binds, `chmod 0600`, and registers
//!    the accept loop on the shared [`TaskTracker`].
//!
//! ## Per connection
//! 1. Client (hook script) connects and writes one line of JSON followed by
//!    `\n`. See [`Payload`] for the schema.
//! 2. Accept loop spawns a **tracked** per-connection task so shutdown
//!    drains it.
//! 3. Handler reads up to `\n` with a 5 s timeout and 16 KiB cap, parses as
//!    [`Payload`], calls [`Forwarder::forward`].
//! 4. Handler writes `{"ok":true}\n` or `{"ok":false,"error":"..."}\n`
//!    and closes.
//!
//! ## Shutdown
//! 1. `CancellationToken::cancel()` fires on SIGTERM/Ctrl-C.
//! 2. The accept loop's `tokio::select!` observes cancellation, unlinks the
//!    socket file, and returns.
//! 3. Per-connection tasks finish in-flight work; `tracker.wait()` in `main`
//!    gives them up to 15 s to drain before the runtime exits.
//!
//! # Why a trait
//!
//! [`Forwarder`] is the seam that separates "UDS protocol machinery" from
//! "where the message goes". Tests swap in a mock that records payloads and
//! never touches the network; production uses [`TelegramForwarder`]. If you
//! ever want a second sink (logfile, Slack, stdout-debug), implement the
//! trait — no listener changes needed.

mod format;
mod listener;

pub use listener::spawn;

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

use crate::telegram::TelegramClient;

/// Opt-in configuration for the outbound-notification listener. Populated
/// from env vars by [`crate::config`] but owned here because the
/// [`spawn`] entrypoint + [`TelegramForwarder`] are what actually consume
/// these fields.
pub struct NotifyConfig {
    pub socket_path: PathBuf,
    pub chat_id: i64,
}

/// Wire-format payload the hook script sends. All string fields are
/// HTML-escaped by [`format::body`] before reaching Telegram.
///
/// - `text`: required, the body (already truncated by the hook to ≤ 1200
///   chars in the usual case).
/// - `cwd`: Claude session's working directory; the basename is shown in
///   the header.
/// - `session`: Claude `session_id`, shown in the header.
/// - `kind`: event classification (`"stop"`, `"subagent_stop"`,
///   `"permission_prompt"`, `"idle_prompt"`). Renders as a `[tag]` prefix
///   in the header. Unknown kinds render with no tag.
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

/// Delivery errors. We collapse upstream [`crate::telegram::TelegramError`]
/// into a single `String` because the listener only needs to know
/// succeeded/failed — it doesn't act on error shape. The underlying
/// structured error is already logged where it happens.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    #[error("delivery failed: {0}")]
    Delivery(String),
}

/// Sink for notifications. See the module-level "Why a trait" note.
///
/// Implementations must be `Send + Sync + 'static` so the listener can share
/// them across connections via `Arc`.
pub trait Forwarder: Send + Sync + 'static {
    /// Deliver `payload` somewhere. The listener only interprets the
    /// `Result` tag (ok vs err) to write a response back to the hook client.
    fn forward(
        &self,
        payload: Payload,
    ) -> impl std::future::Future<Output = Result<(), ForwardError>> + Send;
}

/// Production [`Forwarder`] that composes the payload into HTML and sends
/// it via the existing [`TelegramClient`] (which handles 429/5xx retries).
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
