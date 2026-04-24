//! HTTP GET + streaming SHA-256 + atomic rename. For HF model downloads.
//! Invariants 6 (network error redaction), 10 (cap + timeout), 12 (cancel-safe).

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioTimer};
use ring::digest::{Context as Sha256Ctx, SHA256};
use rustls::ClientConfig;
use tokio_util::sync::CancellationToken;

use super::cache;

const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;

/// 10 min — ~8 Mbps finishes a 488 MB model comfortably.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(10);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

const MAX_REDIRECTS: u8 = 5;

type HyperClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// Description is pre-redacted via [`crate::telegram::redact_network_error`].
    #[error("download failed: {0}")]
    Network(String),

    #[error("download failed: HTTP {status} for {url_host}")]
    HttpStatus { status: u16, url_host: String },

    #[error(
        "download failed: checksum mismatch (expected sha256 {expected}, got {got})"
    )]
    ChecksumMismatch { expected: String, got: String },

    #[error("download failed: response body ended after {bytes_read} bytes")]
    UnexpectedEof { bytes_read: u64 },

    #[error("download failed: response would exceed {} MiB cap", MAX_DOWNLOAD_BYTES / (1024 * 1024))]
    ResponseTooLarge,

    #[error("download failed: io: {0}")]
    Io(String),

    #[error("download failed: overall deadline ({}s) exceeded", DOWNLOAD_TIMEOUT.as_secs())]
    Timeout,

    #[error("download cancelled by caller")]
    Cancelled,
}

impl FetchError {
    #[allow(
        clippy::needless_pass_by_value,
        reason = "used as a function pointer in `.map_err(FetchError::from_io)`"
    )]
    fn from_io(e: io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// HTTPS-only client, decoupled from Telegram's so redaction/TLS can diverge.
pub struct FetchClient {
    client: HyperClient,
}

impl Default for FetchClient {
    fn default() -> Self {
        Self::new()
    }
}

impl FetchClient {
    /// Build a client. `install_crypto_provider` must have run first.
    pub fn new() -> Self {
        let tls = ClientConfig::builder()
            .with_webpki_roots()
            .with_no_client_auth();

        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(CONNECT_TIMEOUT));
        http.set_nodelay(true);

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_only()
            .enable_http1()
            .wrap_connector(http);

        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(1)
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new())
            .build(https);

        Self { client }
    }

    /// Download → SHA verify → atomic rename. Callers should rate-limit `progress` output.
    pub async fn download_verified(
        &self,
        url: &str,
        expected_sha256_hex: &str,
        tmp_path: &Path,
        final_path: &Path,
        mut progress: impl FnMut(u64, Option<u64>) + Send,
        cancel: CancellationToken,
    ) -> Result<(), FetchError> {
        let result = tokio::time::timeout(
            DOWNLOAD_TIMEOUT,
            self.run(
                url,
                expected_sha256_hex,
                tmp_path,
                final_path,
                &mut progress,
                cancel,
            ),
        )
        .await;

        let outcome = result.unwrap_or(Err(FetchError::Timeout));

        if outcome.is_err() {
            let _ = fs::remove_file(tmp_path);
        }
        outcome
    }

    async fn run(
        &self,
        url: &str,
        expected_sha256_hex: &str,
        tmp_path: &Path,
        final_path: &Path,
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
        cancel: CancellationToken,
    ) -> Result<(), FetchError> {
        // hyper-util's legacy Client doesn't auto-redirect — follow manually.
        let mut current_url = url.to_string();
        let mut hops: u8 = 0;
        let response = loop {
            if hops > MAX_REDIRECTS {
                return Err(FetchError::Network(format!(
                    "too many redirects (> {MAX_REDIRECTS}) starting from original URL"
                )));
            }
            let uri: hyper::Uri = current_url.parse().map_err(
                |e: hyper::http::uri::InvalidUri| FetchError::Network(e.to_string()),
            )?;
            let host = uri.host().unwrap_or("<unknown>").to_string();

            let req = Request::builder()
                .method(Method::GET)
                .uri(&current_url)
                .header(hyper::header::USER_AGENT, "tebis-audio-fetch/0.1")
                .header(hyper::header::ACCEPT, "*/*")
                .body(Full::<Bytes>::new(Bytes::new()))
                .map_err(|e| FetchError::Network(e.to_string()))?;

            // Cancel during redirects too — a chain would burn ~5× connect_timeout otherwise.
            let resp = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(FetchError::Cancelled),
                r = self.client.request(req) => r.map_err(|e| {
                    FetchError::Network(crate::telegram::redact_network_error(&e))
                })?,
            };
            let status = resp.status();

            if status.is_redirection() {
                let Some(location) = resp
                    .headers()
                    .get(hyper::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
                else {
                    return Err(FetchError::Network(format!(
                        "redirect {status} missing Location header"
                    )));
                };
                tracing::debug!(from = %host, to = %location, "following redirect");
                current_url = location;
                hops = hops.saturating_add(1);
                continue;
            }
            if !status.is_success() {
                return Err(FetchError::HttpStatus {
                    status: status.as_u16(),
                    url_host: host,
                });
            }
            break resp;
        };

        let content_length = response
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());

        if let Some(len) = content_length
            && len > MAX_DOWNLOAD_BYTES
        {
            return Err(FetchError::ResponseTooLarge);
        }

        if let Some(parent) = tmp_path.parent() {
            cache::ensure_dir_0700(parent).map_err(FetchError::from_io)?;
        }
        let file = cache::open_model_tmp(tmp_path).map_err(FetchError::from_io)?;
        let mut tee = TeeWriter::new(file);

        let mut body = response.into_body();
        loop {
            tokio::select! {
                // `biased` — saturated network could consume thousands of chunks before cancel.
                biased;
                () = cancel.cancelled() => return Err(FetchError::Cancelled),
                frame = body.frame() => {
                    match frame {
                        None => break,
                        Some(Err(e)) => {
                            // invariant 6 — uniform redaction even for HF URLs.
                            return Err(FetchError::Network(crate::sanitize::redact_hyper_error_string(
                                &e.to_string(),
                                |s| crate::sanitize::contains_bot_token_shape(s) || s.contains("api.telegram.org"),
                            )));
                        }
                        Some(Ok(f)) => {
                            if let Ok(chunk) = f.into_data() {
                                if tee.bytes_written + chunk.len() as u64 > MAX_DOWNLOAD_BYTES {
                                    return Err(FetchError::ResponseTooLarge);
                                }
                                tee.write_all(&chunk).map_err(FetchError::from_io)?;
                                progress(tee.bytes_written, content_length);
                            }
                        }
                    }
                }
            }
        }

        if let Some(len) = content_length
            && tee.bytes_written < len
        {
            return Err(FetchError::UnexpectedEof {
                bytes_read: tee.bytes_written,
            });
        }

        let (mut file, digest) = tee.finalize();
        file.flush().map_err(FetchError::from_io)?;
        file.sync_all().map_err(FetchError::from_io)?;
        drop(file);

        let got_hex = hex_encode(digest.as_ref());
        let expected = expected_sha256_hex.to_ascii_lowercase();
        if got_hex != expected {
            return Err(FetchError::ChecksumMismatch {
                expected,
                got: got_hex,
            });
        }

        cache::install_model_atomic(tmp_path, final_path)
            .map_err(|e| FetchError::Io(e.to_string()))?;
        Ok(())
    }
}

/// Tees each chunk into both the file and a SHA-256 hasher.
struct TeeWriter {
    file: fs::File,
    hasher: Sha256Ctx,
    bytes_written: u64,
}

impl TeeWriter {
    fn new(file: fs::File) -> Self {
        Self {
            file,
            hasher: Sha256Ctx::new(&SHA256),
            bytes_written: 0,
        }
    }

    fn finalize(self) -> (fs::File, ring::digest::Digest) {
        (self.file, self.hasher.finish())
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes_written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Lowercase hex — hand-rolled to avoid a `hex` crate dep.
fn hex_encode(bytes: &[u8]) -> String {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(LUT[(b >> 4) as usize] as char);
        out.push(LUT[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn unique_tmpdir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tebis-fetch-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn hex_encode_known_vectors() {
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0x00]), "00");
        assert_eq!(hex_encode(&[0xff]), "ff");
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(hex_encode(&[0x01, 0x02, 0x03]), "010203");
    }

    #[test]
    fn tee_writer_hashes_what_it_writes() {
        let dir = unique_tmpdir("tee");
        let path = dir.join("t.bin");
        let f = cache::open_model_tmp(&path).unwrap();
        let mut tee = TeeWriter::new(f);
        tee.write_all(b"hello ").unwrap();
        tee.write_all(b"world").unwrap();
        let (file, digest) = tee.finalize();
        file.sync_all().unwrap();

        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert_eq!(hex_encode(digest.as_ref()), expected);
        assert_eq!(fs::read(&path).unwrap(), b"hello world");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tee_writer_tracks_bytes_written() {
        let dir = unique_tmpdir("tee-count");
        let path = dir.join("t.bin");
        let f = cache::open_model_tmp(&path).unwrap();
        let mut tee = TeeWriter::new(f);
        tee.write_all(&[0u8; 100]).unwrap();
        tee.write_all(&[0u8; 25]).unwrap();
        assert_eq!(tee.bytes_written, 125);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sha_of_empty_input_matches_nist_vector() {
        let mut h = Sha256Ctx::new(&SHA256);
        h.update(b"");
        let d = h.finish();
        assert_eq!(
            hex_encode(d.as_ref()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
