//! Inbound-notify listener (via `platform::peer_listener` — UDS on Unix,
//! Named Pipe on Windows) + `Forwarder` sink to Telegram. Both transports
//! share the byte cap, newline framing, drain plumbing, and tracker-spawn;
//! see `platform::peer_listener` for the per-OS peer defense.

mod format;
mod listener;
mod markdown;

pub use listener::spawn;

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::audio::AudioSubsystem;
use crate::bridge::typing::TypingRegistry;
use crate::bridge::voice_pref::VoicePref;
use crate::metrics::Metrics;
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
    typing: Arc<TypingRegistry>,
    /// `None` → TTS path skipped entirely.
    tts: Option<TtsSpawn>,
}

/// Resources for spawning a detached TTS for hook-delivered replies.
pub struct TtsSpawn {
    pub audio: Arc<AudioSubsystem>,
    pub voice_pref: Arc<VoicePref>,
    pub metrics: Arc<Metrics>,
    pub tracker: TaskTracker,
    pub shutdown: CancellationToken,
}

impl TelegramForwarder {
    pub fn new(
        tg: Arc<TelegramClient>,
        chat_id: i64,
        typing: Arc<TypingRegistry>,
        tts: Option<TtsSpawn>,
    ) -> Self {
        Self {
            tg,
            chat_id,
            typing,
            tts,
        }
    }
}

impl Forwarder for TelegramForwarder {
    async fn forward(&self, payload: Payload) -> Result<(), ForwardError> {
        let body = format::body(&payload);
        // Cancel typing first; otherwise the 4 s sendChatAction refresh
        // can re-fire *after* the reply lands and clients flash "typing…".
        self.typing.cancel(self.chat_id);
        self.tg
            .send_message(self.chat_id, &body)
            .await
            .map_err(|e| ForwardError::Delivery(e.to_string()))?;

        // Mirror the synchronous `Response::Text` TTS path for hook replies.
        if let Some(tts) = self.tts.as_ref() {
            let was_voice = tts.voice_pref.last_was_voice(self.chat_id);
            if tts.audio.should_tts_reply(was_voice) {
                let tg = self.tg.clone();
                let metrics = tts.metrics.clone();
                let audio = tts.audio.clone();
                let shutdown = tts.shutdown.clone();
                let chat_id = self.chat_id;
                let body_copy = body.clone();
                tts.tracker.spawn(async move {
                    crate::bridge::synthesize_and_send_voice_detached(
                        &tg, &metrics, &audio, &shutdown, chat_id, &body_copy,
                    )
                    .await;
                });
            }
        }
        Ok(())
    }
}
