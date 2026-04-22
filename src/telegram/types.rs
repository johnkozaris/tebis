use serde::{Deserialize, Serialize};

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

#[derive(Debug, Deserialize)]
pub struct BotUser {
    pub id: i64,
    pub first_name: String,
    pub username: Option<String>,
}

// serde ignores unknown fields by default, so new Bot API additions pass through harmlessly.

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
    /// Mic voice note — always OGG/Opus.
    pub voice: Option<Voice>,
    /// User-uploaded audio file — codec varies; only OGG/Opus survives our codec layer.
    pub audio: Option<Audio>,
}

#[derive(Debug, Deserialize)]
pub struct Voice {
    pub file_id: String,
    pub duration: u32,
    pub mime_type: Option<String>,
    pub file_size: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct Audio {
    pub file_id: String,
    pub duration: u32,
    pub mime_type: Option<String>,
    pub title: Option<String>,
    pub file_size: Option<u32>,
}

/// Result of `getFile`. `file_path` is `None` if the file has been GC'd (~1h retention).
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

#[derive(Debug, Serialize)]
pub struct GetUpdatesRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
    /// Static slice — no per-poll allocation.
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

#[derive(Debug, Serialize)]
pub struct SendChatActionRequest<'a> {
    pub chat_id: i64,
    pub action: &'a str,
}
