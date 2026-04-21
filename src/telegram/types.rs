use serde::{Deserialize, Serialize};

// --- Response envelope ---

#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    pub ok: bool,
    pub result: Option<T>,
    pub description: Option<String>,
    pub parameters: Option<ResponseParameters>,
}

#[derive(Debug, Deserialize)]
pub struct ResponseParameters {
    pub retry_after: Option<u64>,
}

// --- getMe response ---

#[derive(Debug, Deserialize)]
pub struct BotUser {
    pub id: i64,
    pub first_name: String,
    pub username: Option<String>,
}

// --- Update types ---
// serde ignores unknown fields by default, so new Bot API additions
// (business_connection, reactions, stories, gifts...) pass through harmlessly.

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)] // `message_id` mirrors the Bot API schema
pub struct Message {
    pub message_id: i64,
    pub from: Option<User>,
    pub chat: Chat,
    pub text: Option<String>,
    /// Voice note (from microphone). Always OGG/Opus when present.
    pub voice: Option<Voice>,
    /// Music-file upload — different UX in Telegram clients, different
    /// codecs possible. Tebis treats this the same as `voice` by
    /// default (pass through STT).
    pub audio: Option<Audio>,
}

/// Voice message (mic recording). Mime type is always `"audio/ogg"`,
/// codec always Opus — per Telegram Bot API. We still read the field
/// so `Debug` logs show what the bot saw.
#[derive(Debug, Deserialize)]
pub struct Voice {
    pub file_id: String,
    pub duration: u32,
    pub mime_type: Option<String>,
    pub file_size: Option<u32>,
}

/// Music-file attachment (user-uploaded). Codec may vary (MP3, M4A,
/// OGG/Opus, etc.). Tebis only decodes OGG/Opus, so non-Opus audio
/// files get rejected at the codec layer.
#[derive(Debug, Deserialize)]
pub struct Audio {
    pub file_id: String,
    pub duration: u32,
    pub mime_type: Option<String>,
    pub title: Option<String>,
    pub file_size: Option<u32>,
}

/// Result of `getFile`. `file_path` is `None` if the file has been
/// garbage-collected (Telegram holds them ≥ 1 hour). Maximum cloud-bot
/// download size is 20 MiB; we enforce that client-side in `config`.
#[derive(Debug, Deserialize)]
pub struct TelegramFile {
    pub file_id: String,
    pub file_path: Option<String>,
    pub file_size: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct GetFileRequest<'a> {
    pub file_id: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
}

// --- Request types ---

#[derive(Debug, Serialize)]
pub struct GetUpdatesRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
    /// Borrowed slice so callers can pass a `&'static` list without
    /// allocating a `Vec<String>` per poll. Serde serializes it as a JSON
    /// string array identically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_updates: Option<&'static [&'static str]>,
}

#[derive(Debug, Serialize)]
pub struct SendMessageRequest<'a> {
    pub chat_id: i64,
    pub text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_preview_options: Option<LinkPreviewOptions>,
}

#[derive(Debug, Serialize)]
pub struct LinkPreviewOptions {
    pub is_disabled: bool,
}

#[derive(Debug, Serialize)]
pub struct DeleteWebhookRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drop_pending_updates: Option<bool>,
}

// --- setMessageReaction (Bot API 7.0+) ---
// Lightweight ack — react with 👍 on success, avoiding chat clutter for
// fire-and-forget actions. Errors still fall through to a text reply.

#[derive(Debug, Serialize)]
pub struct SetMessageReactionRequest<'a> {
    pub chat_id: i64,
    pub message_id: i64,
    pub reaction: Vec<ReactionType<'a>>,
    pub is_big: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReactionType<'a> {
    Emoji { emoji: &'a str },
}

// --- sendChatAction ---
// Shows "typing…" / "recording…" in the chat. Auto-expires after ~5s
// on Telegram's side, so refresh every 4s for a continuous indicator.
#[derive(Debug, Serialize)]
pub struct SendChatActionRequest<'a> {
    pub chat_id: i64,
    pub action: &'a str,
}
