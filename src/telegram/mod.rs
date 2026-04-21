//! Telegram Bot API client on `hyper` + `hyper-rustls` + `ring`.
//!
//! Hand-rolled instead of `reqwest` to keep the dependency graph small and
//! the binary reproducible. Bot token lives in the URL path (per the Bot
//! API) and never appears in logs or `Debug` output — see
//! `redact_network_error`.

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

/// Extra time above the caller's long-poll window for connect + headers.
/// Per-request deadline so a 60 s long-poll waits 65 s, not a hardcoded 35.
const REQUEST_HEADROOM: Duration = Duration::from_secs(5);

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const TCP_KEEPALIVE: Duration = Duration::from_mins(1);
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
/// Ceiling for `download_file`. Cloud Bot API serves files up to 20 MiB
/// and the local Bot API up to 50 MiB — we size for the local cap so
/// self-hosted deployments work without special-casing. Distinct from
/// [`MAX_RESPONSE_BYTES`] (JSON API responses) so tightening one doesn't
/// silently truncate user audio.
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

    /// 401 Unauthorized — the bot token is wrong or revoked. Distinguishing
    /// this from generic `Api` errors lets `main.rs` dead-end with a
    /// paste-a-fresh-token message instead of crash-looping under launchd.
    #[must_use]
    pub const fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Api { status: 401, .. })
    }
}

pub type Result<T> = std::result::Result<T, TelegramError>;

type HyperClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

pub struct TelegramClient {
    client: HyperClient,
    /// URL prefix including the bot token. `SecretString` zeros on drop and
    /// redacts via `Debug`.
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

    /// Delete any existing webhook + drop pending updates. Must be called
    /// before `getUpdates` or the server returns 409.
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

    /// Resolve a `file_id` (from a `voice` / `audio` / `document` field)
    /// to a [`TelegramFile`] with the server-visible `file_path`. Pair
    /// with [`Self::download_file`] to fetch the bytes.
    pub async fn get_file(&self, file_id: &str) -> Result<TelegramFile> {
        self.post_with_timeout(
            "getFile",
            &GetFileRequest { file_id },
            DEFAULT_REQUEST_TIMEOUT,
        )
        .await
    }

    /// Download a file at `file_path` (from a prior `get_file` call)
    /// via the Bot API's file-serving endpoint. The URL contains the
    /// bot token — same as every other Telegram URL — and is routed
    /// through the same `redact_network_error` redactor so network
    /// errors can never leak it to the journal.
    ///
    /// Caller should pre-check `file_size` against
    /// `TELEGRAM_STT_MAX_BYTES`; this method additionally enforces a
    /// [`MAX_FILE_DOWNLOAD_BYTES`] ceiling so a server lying about its
    /// size can't blow memory. The Bot API serves files up to 20 MiB
    /// (cloud) / 50 MiB (local) — the constant is sized for the latter.
    ///
    /// Retry policy: one attempt on connect-level errors, then bubble.
    /// Voice files are small (typically < 1 MB), and a single retry on
    /// transient TCP hiccups is the right shape — full `post_with_timeout`
    /// backoff is overkill and slows the user feedback loop.
    pub async fn download_file(&self, file_path: &str) -> Result<Bytes> {
        // Defensive: Telegram's API currently never returns `file_path`
        // with a leading slash, but if it ever did we'd build
        // `/file/bot<TOKEN>//voice/…`. Most servers normalize, some
        // don't; strip to be safe.
        let file_path = file_path.trim_start_matches('/');

        // `base_url` is `https://api.telegram.org/bot<TOKEN>` — swap
        // `/bot<TOKEN>` for `/file/bot<TOKEN>` to reach the file-serving
        // endpoint. The token stays wrapped in `SecretString` so `Debug`
        // redaction keeps working.
        let url = {
            let base = self.base_url.expose_secret();
            let Some(token) = base.strip_prefix("https://api.telegram.org/bot") else {
                return Err(TelegramError::Network {
                    method: "download_file".to_string(),
                    description: "unexpected base_url shape".to_string(),
                    retryable: false,
                });
            };
            format!("https://api.telegram.org/file/bot{token}/{file_path}")
        };

        let mut last_err: Option<TelegramError> = None;
        for attempt in 0..=1 {
            match self.download_file_once(&url).await {
                Ok(body) => return Ok(body),
                Err(e) => {
                    if attempt == 0 && matches!(
                        &e,
                        TelegramError::Network { retryable: true, .. }
                    ) {
                        tracing::warn!(
                            attempt,
                            err = %e,
                            "download_file: retrying once on connect-level error"
                        );
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        // Unreachable: loop always returns or continues.
        Err(last_err.unwrap_or_else(|| TelegramError::Network {
            method: "download_file".to_string(),
            description: "retry loop exited without result".to_string(),
            retryable: false,
        }))
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

    /// Surface a chat action (`"typing"`, `"upload_photo"`, …) for ~5 s
    /// on the user's screen. Refresh on a loop to keep it active.
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

        for attempt in 0..=MAX_RETRIES {
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
                // Upper-bound the server-provided `retry_after` so a
                // malformed or malicious response can't stall the poll
                // loop for days. 5 minutes covers every realistic
                // Bot-API rate-limit window.
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

/// Install rustls's process-wide crypto provider. Panics if another
/// provider is already installed — that would mean a dep silently picked
/// a different backend than we want.
pub fn install_crypto_provider() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("rustls default crypto provider already installed — unexpected");
}

/// Render a hyper-util client error into a token-safe string. Walks to the
/// root cause, then substring-checks for `/bot` or `api.telegram.org` as
/// belt-and-suspenders against future hyper regressions.
///
/// `pub(crate)` so `audio::fetch` can route its HTTP errors through the
/// same redactor — same crypto stack, same failure modes, same secret
/// leakage risk if a future hyper change starts including URIs in errors.
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
    if raw.contains("/bot") || raw.contains("api.telegram.org") {
        tracing::warn!("Network error contained URI-like data; replaced with redacted placeholder");
        return format!("{kind}: <redacted network error>");
    }
    raw
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
        assert!(!raw.contains("/bot"));
        assert!(!raw.contains("api.telegram.org"));
    }

    #[test]
    fn token_substring_triggers_redaction() {
        let leaked = "request: unexpected /bot12345:abcde in error";
        assert!(leaked.contains("/bot"));
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

    /// Tests share a process and run in arbitrary order; the real
    /// `install_crypto_provider` panics on repeat.
    fn install_crypto_provider_idempotent() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}
