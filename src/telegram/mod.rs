//! Telegram Bot API client on `reqwest` + rustls. Bot token lives in the URL
//! path; `redact_network_error` keeps it out of logs.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use reqwest::StatusCode;

pub mod types;

use types::{
    ApiResponse, BotUser, DeleteWebhookRequest, GetFileRequest, GetUpdatesRequest,
    LinkPreviewOptions, Message, ReactionType, SendChatActionRequest, SendMessageRequest,
    SetMessageReactionRequest, TelegramFile, Update,
};

/// Headroom above the caller's long-poll window for connect + headers.
const REQUEST_HEADROOM: Duration = Duration::from_secs(5);

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const TCP_KEEPALIVE: Duration = Duration::from_mins(1);
const MAX_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;
/// Ceiling for `download_file`. Sized for the local Bot API (50 MiB); cloud is 20 MiB.
const MAX_FILE_DOWNLOAD_BYTES: u64 = 50 * 1024 * 1024;
const MAX_RETRIES: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum TelegramError {
    #[error("{method}: API error {status}: {description}")]
    Api {
        method: String,
        status: u16,
        description: String,
    },

    /// `description` is pre-redacted — raw reqwest errors can carry URL data.
    #[error("{method}: network error: {description}")]
    Network {
        method: String,
        description: String,
        retryable: bool,
    },

    #[error("{method}: response parse error: {description}")]
    Parse { method: String, description: String },

    #[error("{method}: ok=true but result missing")]
    EmptyResult { method: String },

    #[error("{method}: max retries exceeded")]
    MaxRetries { method: String },
}

impl TelegramError {
    /// 409 Conflict — another poller is holding the long-poll.
    #[must_use]
    pub const fn is_conflict(&self) -> bool {
        matches!(self, Self::Api { status: 409, .. })
    }

    /// 401 Unauthorized — `main.rs` dead-ends on this instead of crash-looping.
    #[must_use]
    pub const fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Api { status: 401, .. })
    }
}

pub type Result<T> = std::result::Result<T, TelegramError>;

pub struct TelegramClient {
    client: reqwest::Client,
    /// URL prefix including the bot token. Private; never logged
    /// directly — `redact_network_error` strips it from error chains
    /// and the manual `Debug` impl below redacts it.
    base_url: String,
}

#[derive(Debug)]
enum FetchError {
    Request {
        description: String,
        retryable: bool,
    },
    /// Body-read failure — not retryable (the request may have had effects).
    Body(String),
}

impl TelegramClient {
    /// Build a client. Reqwest's rustls integration uses the ring crypto
    /// provider via the `__rustls-ring` feature internally; no explicit
    /// install needed.
    pub fn new(token: &Arc<str>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(2)
            .tcp_keepalive(TCP_KEEPALIVE)
            .tcp_nodelay(true)
            .https_only(true)
            .build()
            .expect("reqwest::Client::build: rustls + ring features are static");

        let base_url = format!("https://api.telegram.org/bot{token}");
        Self { client, base_url }
    }

    /// Delete any existing webhook + drop pending updates. Required before `getUpdates` or 409.
    pub async fn delete_webhook(&self) -> Result<()> {
        let _: bool = self
            .post_with_timeout(
                "deleteWebhook",
                &DeleteWebhookRequest {
                    drop_pending_updates: Some(true),
                },
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?;
        tracing::info!("Webhook deleted, pending updates dropped");
        Ok(())
    }

    pub async fn get_me(&self) -> Result<BotUser> {
        self.post_with_timeout(
            "getMe",
            &serde_json::Value::Object(serde_json::Map::new()),
            DEFAULT_REQUEST_TIMEOUT,
        )
        .await
    }

    pub async fn get_updates(&self, offset: Option<i64>, timeout: u32) -> Result<Vec<Update>> {
        const ALLOWED: &[&str] = &["message"];
        let deadline = Duration::from_secs(u64::from(timeout))
            .checked_add(REQUEST_HEADROOM)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        self.post_with_timeout(
            "getUpdates",
            &GetUpdatesRequest {
                offset,
                timeout: Some(timeout),
                allowed_updates: Some(ALLOWED),
            },
            deadline,
        )
        .await
    }

    /// HTML `parse_mode`, link previews disabled.
    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<Message> {
        self.post_with_timeout(
            "sendMessage",
            &SendMessageRequest {
                chat_id,
                text,
                parse_mode: Some("HTML"),
                link_preview_options: Some(LinkPreviewOptions { is_disabled: true }),
            },
            DEFAULT_REQUEST_TIMEOUT,
        )
        .await
    }

    pub async fn set_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        emoji: &str,
    ) -> Result<()> {
        let _: bool = self
            .post_with_timeout(
                "setMessageReaction",
                &SetMessageReactionRequest {
                    chat_id,
                    message_id,
                    reaction: vec![ReactionType::Emoji { emoji }],
                    is_big: false,
                },
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?;
        Ok(())
    }

    /// Resolve a `file_id` to a [`TelegramFile`]. Pair with [`Self::download_file`].
    pub async fn get_file(&self, file_id: &str) -> Result<TelegramFile> {
        self.post_with_timeout(
            "getFile",
            &GetFileRequest { file_id },
            DEFAULT_REQUEST_TIMEOUT,
        )
        .await
    }

    /// Download a file via the Bot API's file-serving endpoint. Enforces
    /// [`MAX_FILE_DOWNLOAD_BYTES`] regardless of server claims; one retry on connect errors.
    pub async fn download_file(&self, file_path: &str) -> Result<Bytes> {
        let file_path = file_path.trim_start_matches('/');

        // Reject traversal / full-URL injection in server-provided path.
        if file_path.split('/').any(|seg| seg == "..") || file_path.contains("://") {
            return Err(TelegramError::Network {
                method: "download_file".to_string(),
                description: "rejected file_path with suspicious content".to_string(),
                retryable: false,
            });
        }

        // Swap `/bot<TOKEN>` → `/file/bot<TOKEN>`.
        let url = {
            let base = self.base_url.as_str();
            debug_assert!(
                base.starts_with("https://api.telegram.org/bot"),
                "base_url shape invariant broken"
            );
            let Some(token) = base.strip_prefix("https://api.telegram.org/bot") else {
                return Err(TelegramError::Network {
                    method: "download_file".to_string(),
                    description: "unexpected base_url shape".to_string(),
                    retryable: false,
                });
            };
            format!("https://api.telegram.org/file/bot{token}/{file_path}")
        };

        match self.download_file_once(&url).await {
            Ok(body) => return Ok(body),
            Err(e)
                if matches!(
                    &e,
                    TelegramError::Network {
                        retryable: true,
                        ..
                    }
                ) =>
            {
                tracing::warn!(err = %e, "download_file: retrying once on connect-level error");
            }
            Err(e) => return Err(e),
        }
        self.download_file_once(&url).await
    }

    async fn download_file_once(&self, url: &str) -> Result<Bytes> {
        let response = match self
            .client
            .get(url)
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let retryable = e.is_connect() || e.is_timeout();
                return Err(TelegramError::Network {
                    method: "download_file".to_string(),
                    description: redact_reqwest_error(&e),
                    retryable,
                });
            }
        };
        let status = response.status();
        if !status.is_success() {
            return Err(TelegramError::Api {
                method: "download_file".to_string(),
                status: status.as_u16(),
                description: "non-success HTTP status".to_string(),
            });
        }
        if let Some(len) = response.content_length()
            && len > MAX_FILE_DOWNLOAD_BYTES
        {
            return Err(TelegramError::Network {
                method: "download_file".to_string(),
                description: format!("response too large: {len} > {MAX_FILE_DOWNLOAD_BYTES}"),
                retryable: false,
            });
        }
        let body = collect_capped(response, MAX_FILE_DOWNLOAD_BYTES, "download_file").await?;
        Ok(body)
    }

    /// Send an OGG/Opus voice note. Any other codec degrades to a file
    /// attachment in the Telegram UI.
    pub async fn send_voice(
        &self,
        chat_id: i64,
        voice_ogg: Bytes,
        duration_sec: Option<u32>,
    ) -> Result<Message> {
        let url = format!("{}/sendVoice", self.base_url);

        let voice_part = reqwest::multipart::Part::bytes(voice_ogg.to_vec())
            .file_name("voice.oga")
            .mime_str("audio/ogg")
            .expect("audio/ogg is a static valid MIME");

        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("voice", voice_part);
        if let Some(d) = duration_sec {
            form = form.text("duration", d.to_string());
        }

        let response = match self
            .client
            .post(&url)
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .multipart(form)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let retryable = e.is_connect() || e.is_timeout();
                return Err(TelegramError::Network {
                    method: "sendVoice".to_string(),
                    description: redact_reqwest_error(&e),
                    retryable,
                });
            }
        };

        let status = response.status();
        let body_bytes = collect_capped(response, MAX_RESPONSE_BYTES, "sendVoice").await?;
        let envelope: ApiResponse<Message> =
            serde_json::from_slice(&body_bytes).map_err(|e| TelegramError::Parse {
                method: "sendVoice".to_string(),
                description: format!("response parse: {e}"),
            })?;

        if !envelope.ok {
            return Err(TelegramError::Api {
                method: "sendVoice".to_string(),
                status: status.as_u16(),
                description: envelope.description.unwrap_or_default(),
            });
        }
        envelope.result.ok_or_else(|| TelegramError::EmptyResult {
            method: "sendVoice".to_string(),
        })
    }

    /// Chat action (`typing`, `upload_photo`, …) — auto-expires after ~5s.
    pub async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<()> {
        let _: bool = self
            .post_with_timeout(
                "sendChatAction",
                &SendChatActionRequest { chat_id, action },
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await?;
        Ok(())
    }

    async fn fetch_once<Req>(
        &self,
        url: &str,
        body: &Req,
        request_timeout: Duration,
    ) -> std::result::Result<(StatusCode, Bytes), FetchError>
    where
        Req: serde::Serialize + Sync,
    {
        let response = match self
            .client
            .post(url)
            .timeout(request_timeout)
            .json(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(FetchError::Request {
                    retryable: e.is_connect() || e.is_timeout(),
                    description: redact_reqwest_error(&e),
                });
            }
        };
        let status = response.status();
        let body_bytes = match collect_capped(response, MAX_RESPONSE_BYTES, "fetch").await {
            Ok(b) => b,
            Err(TelegramError::Network { description, .. }) => {
                return Err(FetchError::Body(description));
            }
            Err(e) => return Err(FetchError::Body(e.to_string())),
        };
        Ok((status, body_bytes))
    }

    /// Retry policy: 429 honors `retry_after`; 5xx and retryable network
    /// errors back off up to `MAX_RETRIES`; 409 and other API errors bubble.
    async fn post_with_timeout<Req, Resp>(
        &self,
        method: &str,
        body: &Req,
        request_timeout: Duration,
    ) -> Result<Resp>
    where
        Req: serde::Serialize + Sync,
        Resp: serde::de::DeserializeOwned + Send,
    {
        let url = format!("{}/{}", self.base_url, method);

        const MAX_RATE_LIMIT_HITS: usize = 10;
        // Wall-clock cap so retries can't run forever on a flapping link.
        const TOTAL_RETRY_BUDGET: Duration = Duration::from_mins(3);

        let mut backoff = Duration::from_secs(1);
        let mut rate_limit_hits: usize = 0;
        let start = std::time::Instant::now();

        for attempt in 0..=MAX_RETRIES {
            if start.elapsed() > TOTAL_RETRY_BUDGET {
                return Err(TelegramError::Network {
                    method: method.into(),
                    description: format!("exceeded {TOTAL_RETRY_BUDGET:?} retry budget"),
                    retryable: false,
                });
            }

            let (status, body_bytes_resp) = match self.fetch_once(&url, body, request_timeout).await
            {
                Ok(pair) => pair,
                Err(FetchError::Request {
                    description,
                    retryable,
                }) => {
                    if attempt < MAX_RETRIES && retryable {
                        tracing::warn!(
                            attempt,
                            backoff_secs = backoff.as_secs(),
                            "Network error on {method}, retrying: {description}"
                        );
                        tokio::time::sleep(backoff).await;
                        backoff = backoff.saturating_mul(2).min(Duration::from_mins(1));
                        continue;
                    }
                    return Err(TelegramError::Network {
                        method: method.into(),
                        description,
                        retryable,
                    });
                }
                Err(FetchError::Body(description)) => {
                    return Err(TelegramError::Network {
                        method: method.into(),
                        description,
                        retryable: false,
                    });
                }
            };

            let api_response: ApiResponse<Resp> = serde_json::from_slice(&body_bytes_resp)
                .map_err(|e| TelegramError::Parse {
                    method: method.into(),
                    description: e.to_string(),
                })?;

            if api_response.ok {
                return api_response
                    .result
                    .ok_or_else(|| TelegramError::EmptyResult {
                        method: method.into(),
                    });
            }

            if status.as_u16() == 429 {
                rate_limit_hits += 1;
                if rate_limit_hits > MAX_RATE_LIMIT_HITS {
                    return Err(TelegramError::Network {
                        method: method.into(),
                        description: format!(
                            "exceeded {MAX_RATE_LIMIT_HITS} consecutive 429 rate limits"
                        ),
                        retryable: false,
                    });
                }
                // Cap server-provided `retry_after` so a malformed value can't stall for days.
                const MAX_RETRY_AFTER_SECS: u64 = 300;
                let retry_after = api_response
                    .parameters
                    .and_then(|p| p.retry_after)
                    .unwrap_or(10)
                    .min(MAX_RETRY_AFTER_SECS);
                tracing::warn!(
                    retry_after,
                    hits = rate_limit_hits,
                    "{method}: rate limited"
                );
                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                continue;
            }

            if status.is_server_error() && attempt < MAX_RETRIES {
                tracing::warn!(status = %status, "{method}: server error, retrying in 10s");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }

            let desc = api_response
                .description
                .unwrap_or_else(|| "unknown error".to_string());
            return Err(TelegramError::Api {
                method: method.into(),
                status: status.as_u16(),
                description: desc,
            });
        }

        Err(TelegramError::MaxRetries {
            method: method.into(),
        })
    }
}

/// Read the response body up to `max_bytes`. Surfaces network errors as
/// retryable=false (the request may have already had server-side effects).
async fn collect_capped(
    response: reqwest::Response,
    max_bytes: u64,
    method: &str,
) -> Result<Bytes> {
    if let Some(len) = response.content_length()
        && len > max_bytes
    {
        return Err(TelegramError::Network {
            method: method.to_string(),
            description: format!("response too large: {len} > {max_bytes}"),
            retryable: false,
        });
    }
    let bytes = response.bytes().await.map_err(|e| TelegramError::Network {
        method: method.to_string(),
        description: format!("body read: {}", redact_reqwest_error(&e)),
        retryable: false,
    })?;
    if bytes.len() as u64 > max_bytes {
        return Err(TelegramError::Network {
            method: method.to_string(),
            description: format!("response exceeded {max_bytes} bytes"),
            retryable: false,
        });
    }
    Ok(bytes)
}

/// Redact `/bot<digit>` or `api.telegram.org` from reqwest errors before logging.
pub(crate) fn redact_reqwest_error(err: &reqwest::Error) -> String {
    let kind = if err.is_connect() {
        "connect"
    } else if err.is_timeout() {
        "timeout"
    } else {
        "request"
    };
    let raw = format!("{kind}: {err}");
    crate::sanitize::redact_hyper_error_string(&raw, |s| {
        crate::sanitize::contains_bot_token_shape(s) || s.contains("api.telegram.org")
    })
}

impl std::fmt::Debug for TelegramClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramClient")
            .field("base_url", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_passthrough_on_clean_error() {
        let raw = "request: io: connection reset by peer";
        assert!(!crate::sanitize::contains_bot_token_shape(raw));
        assert!(!raw.contains("api.telegram.org"));
    }

    #[test]
    fn token_substring_triggers_redaction() {
        let leaked = "request: unexpected /bot12345:abcde in error";
        assert!(crate::sanitize::contains_bot_token_shape(leaked));
    }

    #[test]
    fn debug_impl_redacts_base_url() {
        let token: Arc<str> = Arc::from("12345:TESTTESTTEST");
        let client = TelegramClient::new(&token);
        let dbg = format!("{client:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("TESTTESTTEST"));
    }
}
