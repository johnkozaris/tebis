//! Telegram Bot API client on `hyper` + `hyper-rustls` + `ring`. Bot token
//! lives in the URL path; `redact_network_error` keeps it out of logs.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioTimer};
use rustls::ClientConfig;
use secrecy::{ExposeSecret, SecretString};

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
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
/// Ceiling for `download_file`. Sized for the local Bot API (50 MiB); cloud is 20 MiB.
const MAX_FILE_DOWNLOAD_BYTES: usize = 50 * 1024 * 1024;
const MAX_RETRIES: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum TelegramError {
    #[error("{method}: API error {status}: {description}")]
    Api {
        method: String,
        status: u16,
        description: String,
    },

    /// `description` is pre-redacted — raw hyper errors can carry URL data.
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

type HyperClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

pub struct TelegramClient {
    client: HyperClient,
    /// URL prefix including the bot token. `SecretString` redacts via `Debug`.
    base_url: SecretString,
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
    /// Build a client. `install_crypto_provider` must have run first.
    pub fn new(token: &SecretString) -> Self {
        let tls = ClientConfig::builder()
            .with_webpki_roots()
            .with_no_client_auth();

        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(CONNECT_TIMEOUT));
        http.set_nodelay(true);
        http.set_keepalive(Some(TCP_KEEPALIVE));

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_only()
            .enable_http1()
            .wrap_connector(http);

        let client: HyperClient = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(2)
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new())
            .build(https);

        let base_url = SecretString::from(format!(
            "https://api.telegram.org/bot{}",
            token.expose_secret()
        ));
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
    /// [`MAX_FILE_DOWNLOAD_BYTES`] regardless of server claims; one retry
    /// on connect-level errors.
    pub async fn download_file(&self, file_path: &str) -> Result<Bytes> {
        let file_path = file_path.trim_start_matches('/');

        // Reject traversal / full-URL injection in server-provided path.
        if file_path.split('/').any(|seg| seg == "..")
            || file_path.contains("://")
        {
            return Err(TelegramError::Network {
                method: "download_file".to_string(),
                description: "rejected file_path with suspicious content".to_string(),
                retryable: false,
            });
        }

        // Swap `/bot<TOKEN>` → `/file/bot<TOKEN>`.
        let url = {
            let base = self.base_url.expose_secret();
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
            Err(e) if matches!(&e, TelegramError::Network { retryable: true, .. }) => {
                tracing::warn!(err = %e, "download_file: retrying once on connect-level error");
            }
            Err(e) => return Err(e),
        }
        self.download_file_once(&url).await
    }

    async fn download_file_once(&self, url: &str) -> Result<Bytes> {
        let deadline = tokio::time::Instant::now() + DEFAULT_REQUEST_TIMEOUT;
        let req = Request::builder()
            .method(Method::GET)
            .uri(url)
            .body(Full::<Bytes>::new(Bytes::new()))
            .expect("hyper Request::builder: inputs are known-valid");
        let response = match tokio::time::timeout_at(deadline, self.client.request(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return Err(TelegramError::Network {
                    method: "download_file".to_string(),
                    description: redact_network_error(&e),
                    retryable: e.is_connect(),
                });
            }
            Err(_) => {
                return Err(TelegramError::Network {
                    method: "download_file".to_string(),
                    description: format!("timed out after {DEFAULT_REQUEST_TIMEOUT:?}"),
                    retryable: true,
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
        let limited = Limited::new(response.into_body(), MAX_FILE_DOWNLOAD_BYTES);
        let body = match limited.collect().await {
            Ok(b) => b.to_bytes(),
            Err(e) => {
                return Err(TelegramError::Network {
                    method: "download_file".to_string(),
                    description: format!("body read: {e}"),
                    retryable: false,
                });
            }
        };
        Ok(body)
    }

    /// Send an OGG/Opus voice note. Any other codec degrades to a file attachment
    /// in the Telegram UI. Hand-rolled multipart body; boundary from ring RNG.
    pub async fn send_voice(
        &self,
        chat_id: i64,
        voice_ogg: Bytes,
        duration_sec: Option<u32>,
    ) -> Result<Message> {
        use ring::rand::{SecureRandom, SystemRandom};

        let mut boundary_bytes = [0u8; 16];
        SystemRandom::new()
            .fill(&mut boundary_bytes)
            .map_err(|_| TelegramError::Network {
                method: "sendVoice".to_string(),
                description: "ring RNG failure".to_string(),
                retryable: false,
            })?;
        let mut boundary = String::with_capacity(boundary_bytes.len() * 2);
        for byte in boundary_bytes {
            use std::fmt::Write as _;
            let _ = write!(boundary, "{byte:02x}");
        }

        let body = build_send_voice_body(chat_id, &voice_ogg, duration_sec, &boundary);
        let content_type = format!("multipart/form-data; boundary={boundary}");
        let url = format!("{}/sendVoice", self.base_url.expose_secret());

        let deadline = tokio::time::Instant::now() + DEFAULT_REQUEST_TIMEOUT;
        let req = Request::builder()
            .method(Method::POST)
            .uri(&url)
            .header(hyper::header::CONTENT_TYPE, content_type)
            .body(Full::new(body))
            .expect("hyper Request::builder: inputs are known-valid");

        let response = match tokio::time::timeout_at(deadline, self.client.request(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return Err(TelegramError::Network {
                    method: "sendVoice".to_string(),
                    description: redact_network_error(&e),
                    retryable: e.is_connect(),
                });
            }
            Err(_) => {
                return Err(TelegramError::Network {
                    method: "sendVoice".to_string(),
                    description: format!("timed out after {DEFAULT_REQUEST_TIMEOUT:?}"),
                    retryable: true,
                });
            }
        };

        let status = response.status();
        let limited = Limited::new(response.into_body(), MAX_RESPONSE_BYTES);
        let body_bytes = limited.collect().await.map_err(|e| TelegramError::Network {
            method: "sendVoice".to_string(),
            description: format!("body read: {e}"),
            retryable: false,
        })?;
        let envelope: ApiResponse<Message> = serde_json::from_slice(&body_bytes.to_bytes())
            .map_err(|e| TelegramError::Parse {
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

    async fn fetch_once(
        &self,
        url: &str,
        body_bytes: &Bytes,
        deadline: tokio::time::Instant,
        request_timeout: Duration,
    ) -> std::result::Result<(hyper::StatusCode, Bytes), FetchError> {
        let req = Request::builder()
            .method(Method::POST)
            .uri(url)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Full::new(body_bytes.clone()))
            .expect("hyper Request::builder: inputs are known-valid");

        let response = match tokio::time::timeout_at(deadline, self.client.request(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return Err(FetchError::Request {
                    retryable: e.is_connect(),
                    description: redact_network_error(&e),
                });
            }
            Err(_) => {
                return Err(FetchError::Request {
                    retryable: true,
                    description: format!("send timed out after {request_timeout:?}"),
                });
            }
        };
        let status = response.status();

        let body = Limited::new(response.into_body(), MAX_RESPONSE_BYTES);
        let body_bytes_resp = match tokio::time::timeout_at(deadline, body.collect()).await {
            Ok(Ok(c)) => c.to_bytes(),
            Ok(Err(e)) => return Err(FetchError::Body(format!("body read: {e}"))),
            Err(_) => {
                return Err(FetchError::Body(format!(
                    "body read timed out after {request_timeout:?}"
                )));
            }
        };

        Ok((status, body_bytes_resp))
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
        let url = format!("{}/{}", self.base_url.expose_secret(), method);

        let body_bytes = serde_json::to_vec(body).map_err(|e| TelegramError::Parse {
            method: method.into(),
            description: format!("request serialize: {e}"),
        })?;
        let body_bytes = Bytes::from(body_bytes);

        let mut backoff = Duration::from_secs(1);
        // Wall-clock cap so retries can't starve the handler semaphore permit.
        const TOTAL_RETRY_BUDGET: Duration = Duration::from_mins(3);
        let start = std::time::Instant::now();

        for attempt in 0..=MAX_RETRIES {
            if start.elapsed() > TOTAL_RETRY_BUDGET {
                return Err(TelegramError::Network {
                    method: method.into(),
                    description: format!(
                        "exceeded {TOTAL_RETRY_BUDGET:?} retry budget"
                    ),
                    retryable: false,
                });
            }
            let deadline = tokio::time::Instant::now() + request_timeout;

            let (status, body_bytes_resp) = match self
                .fetch_once(&url, &body_bytes, deadline, request_timeout)
                .await
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
                // Cap server-provided `retry_after` so a malformed value can't stall for days.
                const MAX_RETRY_AFTER_SECS: u64 = 300;
                let retry_after = api_response
                    .parameters
                    .and_then(|p| p.retry_after)
                    .unwrap_or(10)
                    .min(MAX_RETRY_AFTER_SECS);
                tracing::warn!(retry_after, "{method}: rate limited");
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

/// Install rustls's process-wide crypto provider. Panics if another is already installed
/// — that means a dep silently picked a different backend.
pub fn install_crypto_provider() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("rustls default crypto provider already installed — unexpected");
}

/// Build a `multipart/form-data` body for `sendVoice`. CRLF per RFC 7578.
fn build_send_voice_body(
    chat_id: i64,
    voice: &[u8],
    duration_sec: Option<u32>,
    boundary: &str,
) -> Bytes {
    let mut out: Vec<u8> = Vec::with_capacity(voice.len() + 512);

    let write_text_field = |out: &mut Vec<u8>, name: &str, value: &str| {
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    };

    write_text_field(&mut out, "chat_id", &chat_id.to_string());
    if let Some(d) = duration_sec {
        write_text_field(&mut out, "duration", &d.to_string());
    }

    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(
        b"Content-Disposition: form-data; name=\"voice\"; filename=\"voice.oga\"\r\n",
    );
    out.extend_from_slice(b"Content-Type: audio/ogg\r\n\r\n");
    out.extend_from_slice(voice);
    out.extend_from_slice(b"\r\n");

    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");

    Bytes::from(out)
}

/// Render a hyper-util error into a token-safe string. Walks to root cause,
/// redacts on `/bot<digit>` or `api.telegram.org`. Invariant 6. `pub(crate)`
/// so `audio::fetch` can route its errors through the same redactor.
pub(crate) fn redact_network_error(err: &hyper_util::client::legacy::Error) -> String {
    const MAX_SOURCE_DEPTH: usize = 16;
    let mut cur: &dyn std::error::Error = err;
    for _ in 0..MAX_SOURCE_DEPTH {
        let Some(next) = cur.source() else { break };
        cur = next;
    }
    let kind = if err.is_connect() {
        "connect"
    } else {
        "request"
    };
    let raw = format!("{kind}: {cur}");
    // Gate on `/bot<digit>` — bare `/bot` false-positives on benign paths.
    if contains_bot_token_shape(&raw) || raw.contains("api.telegram.org") {
        tracing::warn!("Network error contained URI-like data; replaced with redacted placeholder");
        return format!("{kind}: <redacted network error>");
    }
    raw
}

/// True when `s` contains `/bot` followed by an ASCII digit.
fn contains_bot_token_shape(s: &str) -> bool {
    let bytes = s.as_bytes();
    let needle = b"/bot";
    bytes
        .windows(needle.len())
        .enumerate()
        .any(|(i, w)| w == needle && bytes.get(i + needle.len()).is_some_and(u8::is_ascii_digit))
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
        assert!(!contains_bot_token_shape(raw));
        assert!(!raw.contains("api.telegram.org"));
    }

    #[test]
    fn token_substring_triggers_redaction() {
        let leaked = "request: unexpected /bot12345:abcde in error";
        assert!(contains_bot_token_shape(leaked));
    }

    #[test]
    fn bare_bot_path_does_not_trigger_false_positive() {
        for benign in [
            "connect: tcp stream closed to /bot/health",
            "request: /botanical-garden sensor error",
            "request: unrelated string ending with /bot",
            "connect: proxy at 10.0.0.1/bot-proxy refused",
        ] {
            assert!(
                !contains_bot_token_shape(benign),
                "tightened shape should not match benign input: {benign:?}"
            );
        }
    }

    #[test]
    fn bot_token_shape_requires_digit_after_bot() {
        assert!(contains_bot_token_shape("/bot0"));
        assert!(contains_bot_token_shape("/bot9123456789:XYZ"));
        assert!(!contains_bot_token_shape("/bot:"));
        assert!(!contains_bot_token_shape("/bot"));
        assert!(!contains_bot_token_shape("/botX"));
    }

    #[test]
    fn debug_impl_redacts_base_url() {
        use secrecy::SecretString;
        let token = SecretString::from("12345:TESTTESTTEST".to_string());
        install_crypto_provider_idempotent();
        let client = TelegramClient::new(&token);
        let dbg = format!("{client:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("TESTTESTTEST"));
    }

    /// Real `install_crypto_provider` panics on repeat; tests share a process.
    fn install_crypto_provider_idempotent() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn send_voice_body_shape() {
        let voice = b"OggS\x00\x02fake-opus-bytes\x00";
        let body = build_send_voice_body(987_654, voice, Some(3), "abcdef0123456789");
        let as_str = std::str::from_utf8(&body).unwrap_or("");
        assert!(as_str.contains("--abcdef0123456789\r\n"));
        assert!(as_str.contains("--abcdef0123456789--\r\n"));
        assert!(as_str.contains(
            "Content-Disposition: form-data; name=\"chat_id\"\r\n\r\n987654\r\n"
        ));
        assert!(
            as_str.contains("Content-Disposition: form-data; name=\"duration\"\r\n\r\n3\r\n")
        );
        assert!(as_str.contains(
            "Content-Disposition: form-data; name=\"voice\"; filename=\"voice.oga\"\r\n"
        ));
        assert!(as_str.contains("Content-Type: audio/ogg\r\n\r\n"));
        let offset = body
            .windows(voice.len())
            .position(|w| w == voice)
            .expect("voice bytes embedded verbatim");
        assert!(offset > 0);
    }

    #[test]
    fn send_voice_body_omits_duration_when_none() {
        let body = build_send_voice_body(1, b"x", None, "bnd");
        let as_str = std::str::from_utf8(&body).unwrap_or("");
        assert!(!as_str.contains("name=\"duration\""));
    }
}
