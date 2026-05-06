//! HTTP GET + streaming SHA-256 + atomic rename. For HF model downloads.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use ring::digest::{Context as Sha256Ctx, SHA256};
use tokio_util::sync::CancellationToken;

use super::cache;

const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;

/// 10 min — ~8 Mbps finishes a 488 MB model comfortably.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(10);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

const MAX_REDIRECTS: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// Description is pre-redacted of bot-token / api.telegram.org substrings.
    #[error("download failed: {0}")]
    Network(String),

    #[error("download failed: HTTP {status} for {url_host}")]
    HttpStatus { status: u16, url_host: String },

    #[error("download failed: checksum mismatch (expected sha256 {expected}, got {got})")]
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
    client: reqwest::Client,
}

impl Default for FetchClient {
    fn default() -> Self {
        Self::new()
    }
}

impl FetchClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(1)
            .tcp_nodelay(true)
            .https_only(true)
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .user_agent("tebis-audio-fetch/0.1")
            .build()
            .expect("reqwest::Client::build: rustls features are static");
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
        let send_fut = self.client.get(url).send();
        let mut response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(FetchError::Cancelled),
            r = send_fut => r.map_err(|e| FetchError::Network(redact_fetch_error(&e)))?,
        };
        let host = response
            .url()
            .host_str()
            .map(str::to_string)
            .unwrap_or_else(|| "<unknown>".to_string());
        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::HttpStatus {
                status: status.as_u16(),
                url_host: host,
            });
        }

        let content_length = response.content_length();
        if let Some(len) = content_length
            && len > MAX_DOWNLOAD_BYTES
        {
            return Err(FetchError::ResponseTooLarge);
        }

        if let Some(parent) = tmp_path.parent() {
            crate::platform::secure_file::ensure_private_dir(parent)
                .map_err(FetchError::from_io)?;
        }
        let file = cache::open_model_tmp(tmp_path).map_err(FetchError::from_io)?;
        let mut tee = TeeWriter::new(file);

        loop {
            let chunk = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(FetchError::Cancelled),
                c = response.chunk() => match c {
                    Ok(Some(c)) => c,
                    Ok(None) => break,
                    Err(e) => return Err(FetchError::Network(redact_fetch_error(&e))),
                },
            };
            if tee.bytes_written + chunk.len() as u64 > MAX_DOWNLOAD_BYTES {
                return Err(FetchError::ResponseTooLarge);
            }
            tee.write_all(&chunk).map_err(FetchError::from_io)?;
            progress(tee.bytes_written, content_length);
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

fn redact_fetch_error(err: &reqwest::Error) -> String {
    crate::sanitize::redact_hyper_error_string(&err.to_string(), |s| {
        crate::sanitize::contains_bot_token_shape(s) || s.contains("api.telegram.org")
    })
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
