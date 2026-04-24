//! OpenAI-compatible remote TTS. Invariants 6 (redact) + 10 (cap + timeout).

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, Uri};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioTimer};
use rustls::ClientConfig;
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use tokio::time::timeout;

use super::TtsError;
use crate::audio::codec;

const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Body-read deadline — covers the slow-body-after-fast-headers case.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

type HyperClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

pub struct RemoteTts {
    client: HyperClient,
    base_url: String,
    api_key: Option<SecretString>,
    model: String,
    voice: String,
    timeout: Duration,
}

#[derive(Serialize)]
struct SpeechRequest<'a> {
    model: &'a str,
    input: &'a str,
    voice: &'a str,
    response_format: &'a str,
}

impl RemoteTts {
    /// Scheme enforcement is `config.rs`'s job — `https_or_http()` here accepts both.
    pub fn new(
        url: String,
        api_key: Option<SecretString>,
        model: String,
        voice: String,
        timeout_sec: u32,
    ) -> Result<Self, TtsError> {
        let base_url = url.trim_end_matches('/').to_string();
        let _: Uri = base_url.parse().map_err(|e: hyper::http::uri::InvalidUri| {
            TtsError::Init(format!("invalid remote TTS URL: {e}"))
        })?;

        let tls = ClientConfig::builder()
            .with_webpki_roots()
            .with_no_client_auth();

        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(CONNECT_TIMEOUT));
        http.set_nodelay(true);

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);

        let client: HyperClient = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(2)
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new())
            .build(https);

        Ok(Self {
            client,
            base_url,
            api_key,
            model,
            voice,
            timeout: Duration::from_secs(u64::from(timeout_sec)),
        })
    }

    pub fn voice(&self) -> &str {
        &self.voice
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Whether a Bearer token is configured — dashboard shows `set`/`unset`, never the value.
    pub const fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    /// Synthesize `text` to OGG/Opus bytes plus the audio duration in
    /// seconds. The bytes are pass-through-ready for Telegram's
    /// `sendVoice`; no re-encode.
    pub async fn synthesize_to_opus(&self, text: &str) -> Result<(Bytes, u32), TtsError> {
        if text.trim().is_empty() {
            return Err(TtsError::Synthesis("empty text".to_string()));
        }

        let body_json = serde_json::to_vec(&SpeechRequest {
            model: &self.model,
            input: text,
            voice: &self.voice,
            response_format: "opus",
        })
        .map_err(|e| TtsError::Synthesis(format!("serialize request: {e}")))?;

        let uri: Uri = format!("{}/v1/audio/speech", self.base_url)
            .parse()
            .map_err(|e: hyper::http::uri::InvalidUri| {
                TtsError::Synthesis(format!("build request URI: {e}"))
            })?;

        let mut req_builder = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/json")
            .header("accept", "audio/ogg");
        if let Some(key) = &self.api_key {
            req_builder = req_builder.header(
                "authorization",
                format!("Bearer {}", key.expose_secret()),
            );
        }
        let req = req_builder
            .body(Full::<Bytes>::from(Bytes::from(body_json)))
            .map_err(|e| TtsError::Synthesis(format!("build request: {e}")))?;

        let fut = self.client.request(req);
        let resp = match timeout(self.timeout, fut).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return Err(TtsError::Synthesis(format!(
                    "network: {}",
                    redact_network_error(&e)
                )));
            }
            Err(_) => {
                return Err(TtsError::Synthesis(format!(
                    "remote TTS timed out after {:?}",
                    self.timeout
                )));
            }
        };

        let status = resp.status();
        if status != StatusCode::OK {
            let body = match timeout(Duration::from_secs(2), resp.collect()).await {
                Ok(Ok(c)) => c.to_bytes(),
                _ => Bytes::new(),
            };
            let trimmed: String = String::from_utf8_lossy(&body).chars().take(200).collect();
            return Err(TtsError::Synthesis(format!(
                "HTTP {}: {trimmed}",
                status.as_u16()
            )));
        }

        let collected = timeout(BODY_READ_TIMEOUT, resp.collect())
            .await
            .map_err(|_| TtsError::Synthesis("body read timed out".to_string()))?
            .map_err(|e| TtsError::Synthesis(format!("body: {e}")))?;
        let bytes = collected.to_bytes();

        if bytes.is_empty() {
            return Err(TtsError::EmptyOutput);
        }
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(TtsError::Synthesis(format!(
                "remote response too large: {} > {MAX_RESPONSE_BYTES} bytes",
                bytes.len()
            )));
        }

        // Decode-for-duration — cap ~1 h @ 16 kHz against bitrate-stuffed blobs.
        const MAX_DECODED_SAMPLES: usize = 3600 * 16_000;
        let pcm = codec::decode_opus_to_pcm16k(&bytes, MAX_DECODED_SAMPLES)
            .map_err(|e| TtsError::Synthesis(format!("decode ogg duration: {e}")))?;
        let duration_sec = u32::try_from(pcm.len() / 16_000).unwrap_or(u32::MAX);
        Ok((bytes, duration_sec))
    }
}

/// Remote-TTS flavour of invariant 6: redact URI/auth substrings.
fn redact_network_error(err: &hyper_util::client::legacy::Error) -> String {
    crate::sanitize::redact_hyper_error(err, |raw| {
        raw.contains("://") || raw.contains("Bearer ") || raw.contains("Authorization")
    })
}

impl std::fmt::Debug for RemoteTts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Omit `client` — hyper's Debug could leak URI/auth.
        f.debug_struct("RemoteTts")
            .field("base_url", &"<redacted>")
            .field("model", &self.model)
            .field("voice", &self.voice)
            .field("timeout", &self.timeout)
            .field(
                "api_key",
                &if self.api_key.is_some() {
                    "<set>"
                } else {
                    "<unset>"
                },
            )
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_crypto_provider_idempotent() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn debug_redacts_url_and_key() {
        install_crypto_provider_idempotent();
        let rt = RemoteTts::new(
            "https://example.com/path".to_string(),
            Some(SecretString::from("mysecret".to_string())),
            "kokoro".to_string(),
            "af_sarah".to_string(),
            10,
        )
        .expect("construct");
        let dbg = format!("{rt:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(dbg.contains("<set>"));
        assert!(!dbg.contains("mysecret"));
        assert!(!dbg.contains("example.com"));
    }

    #[test]
    fn debug_shows_unset_for_no_api_key() {
        install_crypto_provider_idempotent();
        let rt = RemoteTts::new(
            "https://example.com".to_string(),
            None,
            "m".to_string(),
            "v".to_string(),
            5,
        )
        .expect("construct");
        let dbg = format!("{rt:?}");
        assert!(dbg.contains("<unset>"));
    }

    #[test]
    fn base_url_trailing_slashes_stripped() {
        install_crypto_provider_idempotent();
        let rt = RemoteTts::new(
            "https://example.com///".to_string(),
            None,
            "m".to_string(),
            "v".to_string(),
            5,
        )
        .expect("construct");
        assert_eq!(rt.base_url, "https://example.com");
    }

    #[test]
    fn invalid_url_rejected_at_construct() {
        install_crypto_provider_idempotent();
        let err = RemoteTts::new(
            "not a uri".to_string(),
            None,
            "m".to_string(),
            "v".to_string(),
            5,
        )
        .unwrap_err();
        match err {
            TtsError::Init(msg) => assert!(msg.contains("invalid")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn accessors_expose_configured_values() {
        install_crypto_provider_idempotent();
        let rt = RemoteTts::new(
            "https://example.com".to_string(),
            Some(SecretString::from("k".to_string())),
            "kokoro-v2".to_string(),
            "af_sarah".to_string(),
            15,
        )
        .expect("construct");
        assert_eq!(rt.model(), "kokoro-v2");
        assert_eq!(rt.voice(), "af_sarah");
        assert!(rt.has_api_key());
    }

    #[tokio::test]
    async fn empty_text_rejected_without_network_call() {
        install_crypto_provider_idempotent();
        let rt = RemoteTts::new(
            "https://127.0.0.1:1".to_string(), // unreachable; would fail if we got here
            None,
            "m".to_string(),
            "v".to_string(),
            1,
        )
        .expect("construct");
        let err = rt.synthesize_to_opus("   ").await.unwrap_err();
        match err {
            TtsError::Synthesis(msg) => assert!(msg.contains("empty")),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
