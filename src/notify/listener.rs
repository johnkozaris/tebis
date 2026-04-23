//! Accept loop + per-connection protocol. Platform-specific bind /
//! peer-auth lives in `crate::platform::peer_listener`; this file is
//! pure protocol: newline-framed JSON (invariant 11), byte-capped
//! (invariant 10), forwarded through the `Forwarder` trait.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::platform::peer_listener::{Conn, Listener};

use super::{Forwarder, Payload};

/// Invariant 10: 16 KiB max, ~10× the advertised 1500-char body.
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub fn spawn<F: Forwarder>(
    tracker: &TaskTracker,
    shutdown: CancellationToken,
    socket_path: PathBuf,
    forwarder: Arc<F>,
) -> Result<()> {
    let listener = Listener::bind(&socket_path)
        .with_context(|| format!("binding notify listener at {}", socket_path.display()))?;

    tracing::info!(
        path = %socket_path.display(),
        "Notify listener bound (platform::peer_listener)"
    );

    let tracker_for_conns = tracker.clone();
    tracker.spawn(accept_loop(
        listener,
        forwarder,
        tracker_for_conns,
        shutdown,
    ));
    Ok(())
}

/// Cooldown between failed accepts — sticky EMFILE etc. would otherwise spin.
const ACCEPT_ERROR_COOLDOWN: Duration = Duration::from_millis(100);

async fn accept_loop<F: Forwarder>(
    listener: Listener,
    forwarder: Arc<F>,
    tracker: TaskTracker,
    shutdown: CancellationToken,
) {
    loop {
        let accept = tokio::select! {
            a = listener.accept() => a,
            () = shutdown.cancelled() => {
                tracing::info!("Notify listener shutting down");
                return;
            }
        };

        match accept {
            Ok(conn) => {
                if !listener.is_trusted_peer(&conn) {
                    // Warning already logged by is_trusted_peer.
                    tracker.spawn(reject_connection(conn));
                    continue;
                }
                let f = forwarder.clone();
                tracker.spawn(async move {
                    handle_connection(conn, f).await;
                });
            }
            Err(e) => {
                tracing::warn!(err = %e, "Notify accept failed");
                tokio::select! {
                    () = tokio::time::sleep(ACCEPT_ERROR_COOLDOWN) => {}
                    () = shutdown.cancelled() => return,
                }
            }
        }
    }
}

async fn reject_connection(mut conn: Conn) {
    let _ = conn
        .write_all(b"{\"ok\":false,\"error\":\"forbidden\"}\n")
        .await;
}

async fn handle_connection<S, F>(mut stream: S, forwarder: Arc<F>)
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: Forwarder,
{
    let payload = match read_payload(&mut stream).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(err = %e, "Notify payload read/parse failed");
            let _ = stream
                .write_all(b"{\"ok\":false,\"error\":\"bad_request\"}\n")
                .await;
            return;
        }
    };

    // Invariant 5: metadata only; never the text.
    tracing::debug!(
        bytes = payload.text.len(),
        has_cwd = payload.cwd.is_some(),
        has_session = payload.session.is_some(),
        kind = ?payload.kind,
        "Notify forwarding"
    );

    match forwarder.forward(payload).await {
        Ok(()) => {
            let _ = stream.write_all(b"{\"ok\":true}\n").await;
        }
        Err(e) => {
            tracing::warn!(err = %e, "Notify delivery failed");
            let _ = stream
                .write_all(b"{\"ok\":false,\"error\":\"send_failed\"}\n")
                .await;
        }
    }
}

async fn read_payload<S>(stream: &mut S) -> Result<Payload>
where
    S: AsyncRead + Unpin,
{
    let mut reader = BufReader::with_capacity(4096, stream);
    let mut buf = Vec::with_capacity(2048);

    let n = tokio::time::timeout(
        CONNECT_TIMEOUT,
        read_until_bounded(&mut reader, b'\n', &mut buf, MAX_PAYLOAD_BYTES),
    )
    .await
    .context("read timed out")?
    .context("read error")?;

    if n == 0 {
        anyhow::bail!("empty request");
    }

    let payload: Payload = serde_json::from_slice(&buf).context("invalid JSON")?;
    if payload.text.is_empty() {
        anyhow::bail!("empty text field");
    }
    Ok(payload)
}

/// Invariant 11: newline-framed, NOT EOF-framed. macOS's stock `nc` lacks
/// `-N` for UDS half-close, so hook scripts use `nc -U -w 2` and depend on
/// `\n` to flush. Hard cap bounds a client that never sends one.
async fn read_until_bounded<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    delim: u8,
    out: &mut Vec<u8>,
    max: usize,
) -> Result<usize> {
    let mut total = 0;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        let (consume, done) =
            memchr(delim, available).map_or((available.len(), false), |i| (i + 1, true));
        if total + consume > max {
            anyhow::bail!("payload exceeds {max} bytes");
        }
        out.extend_from_slice(&available[..consume]);
        total += consume;
        reader.consume(consume);
        if done {
            return Ok(total);
        }
    }
}

fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

#[cfg(test)]
mod tests {
    //! Protocol tests via `tokio::io::duplex()` — cross-platform;
    //! peer-auth lives in `platform::peer_listener` and is tested there.

    use super::super::{ForwardError, Forwarder, Payload};
    use super::*;

    use std::sync::Mutex;
    use tokio::io::AsyncReadExt;

    struct Recorder {
        calls: Mutex<Vec<Payload>>,
        fail: bool,
    }

    impl Recorder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                fail: false,
            })
        }
        fn failing() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                fail: true,
            })
        }
        fn calls(&self) -> Vec<Payload> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Forwarder for Recorder {
        async fn forward(&self, payload: Payload) -> Result<(), ForwardError> {
            self.calls.lock().unwrap().push(payload);
            if self.fail {
                Err(ForwardError::Delivery("test-forced".into()))
            } else {
                Ok(())
            }
        }
    }

    async fn drive(forwarder: Arc<impl Forwarder + 'static>, write: &[u8]) -> Vec<u8> {
        let (server, mut client) = tokio::io::duplex(4096);
        let handle = tokio::spawn(handle_connection(server, forwarder));

        client.write_all(write).await.unwrap();
        // Half-close so the server hits EOF without waiting CONNECT_TIMEOUT.
        client.shutdown().await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        handle.await.unwrap();
        response
    }

    #[tokio::test]
    async fn valid_line_forwards_and_replies_ok() {
        let rec = Recorder::new();
        let resp = drive(rec.clone(), b"{\"text\":\"hi\",\"kind\":\"stop\"}\n").await;
        assert_eq!(resp, b"{\"ok\":true}\n");
        let calls = rec.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].text, "hi");
        assert_eq!(calls[0].kind.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn malformed_json_replies_bad_request() {
        let rec = Recorder::new();
        let resp = drive(rec.clone(), b"not-json\n").await;
        assert_eq!(resp, b"{\"ok\":false,\"error\":\"bad_request\"}\n");
        assert!(rec.calls().is_empty());
    }

    #[tokio::test]
    async fn empty_text_replies_bad_request() {
        let rec = Recorder::new();
        let resp = drive(rec.clone(), b"{\"text\":\"\"}\n").await;
        assert_eq!(resp, b"{\"ok\":false,\"error\":\"bad_request\"}\n");
        assert!(rec.calls().is_empty());
    }

    #[tokio::test]
    async fn missing_newline_but_eof_still_parses() {
        let rec = Recorder::new();
        let resp = drive(rec.clone(), b"{\"text\":\"no-newline\"}").await;
        assert_eq!(resp, b"{\"ok\":true}\n");
        assert_eq!(rec.calls().len(), 1);
    }

    #[tokio::test]
    async fn oversize_line_replies_bad_request() {
        let rec = Recorder::new();
        let mut buf = Vec::with_capacity(MAX_PAYLOAD_BYTES + 100);
        buf.extend_from_slice(b"{\"text\":\"");
        buf.resize(MAX_PAYLOAD_BYTES + 50, b'a');
        buf.extend_from_slice(b"\"}\n");
        let resp = drive(rec.clone(), &buf).await;
        assert_eq!(resp, b"{\"ok\":false,\"error\":\"bad_request\"}\n");
        assert!(rec.calls().is_empty());
    }

    #[tokio::test]
    async fn forwarder_failure_replies_send_failed() {
        let rec = Recorder::failing();
        let resp = drive(rec.clone(), b"{\"text\":\"hi\"}\n").await;
        assert_eq!(resp, b"{\"ok\":false,\"error\":\"send_failed\"}\n");
        assert_eq!(rec.calls().len(), 1);
    }
}
