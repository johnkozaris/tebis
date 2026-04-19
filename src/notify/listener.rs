//! UDS binding, accept loop, and per-connection protocol handler.
//!
//! Parameterized over `F: Forwarder` so tests can inject a fake sink; the
//! production code path uses [`super::TelegramForwarder`].

use anyhow::{Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::{Forwarder, Payload};

/// Hard cap on a single line. 16 KiB is ~10× the advertised 1500-char body
/// limit; anything bigger is a bug or abuse.
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;

/// Per-connection read budget. Hooks run locally and should finish writing
/// within milliseconds; 5 s leaves room for a pathological client.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Bind the socket and register the accept loop on the shared
/// [`TaskTracker`]. Per-connection tasks are also tracked so graceful
/// shutdown drains any in-flight deliveries.
pub fn spawn<F: Forwarder>(
    tracker: &TaskTracker,
    shutdown: CancellationToken,
    socket_path: PathBuf,
    forwarder: Arc<F>,
) -> Result<()> {
    let listener = bind(&socket_path)?;

    tracing::info!(
        path = %socket_path.display(),
        "Notify listener bound (UDS, mode 0600)"
    );

    let tracker_for_conns = tracker.clone();
    tracker.spawn(accept_loop(
        listener,
        socket_path,
        forwarder,
        tracker_for_conns,
        shutdown,
    ));
    Ok(())
}

/// Unlink any stale socket at `path`, bind a fresh `UnixListener`, and
/// chmod to `0600`.
///
/// Three-layer defense so the socket never exists at a looser mode:
///
/// 1. **Tightened umask around `bind(2)`** — the kernel creates the socket
///    file with mode `0666 & ~umask`. Default umasks (`0022`, `0002`) yield
///    `0644` / `0664`, which Linux honors for `connect(2)`. We temporarily
///    set umask `0177` so the file is `0600` from the instant it appears.
/// 2. **Explicit `chmod 0600`** — umask can't be trusted across weird init
///    configs (login shells, containers with overridden umask).
/// 3. **Peer-cred check** in [`handle_connection`] — kernel-authenticated
///    UID match, closes the tiny TOCTOU window between bind and chmod.
///
/// `libc::umask` is process-wide, so another thread creating a file in
/// the same microseconds would also inherit `0177`. Bind is called once
/// from `main` before the poll loop starts and before any notify work
/// fires, so in practice only the main thread is active here.
fn bind(path: &Path) -> Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to unlink stale socket at {}", path.display()));
        }
    }

    // SAFETY: `umask(2)` is async-signal-safe and has no preconditions.
    // We restore the prior mask unconditionally (including on the error
    // path) so a bind failure doesn't leak the tightened umask into the
    // rest of the process.
    let prior_umask = unsafe { libc::umask(0o177) };
    let listener_result = UnixListener::bind(path);
    unsafe { libc::umask(prior_umask) };

    let listener = listener_result
        .with_context(|| format!("failed to bind notify socket at {}", path.display()))?;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod socket to 0600 at {}", path.display()))?;

    Ok(listener)
}

async fn accept_loop<F: Forwarder>(
    listener: UnixListener,
    socket_path: PathBuf,
    forwarder: Arc<F>,
    tracker: TaskTracker,
    shutdown: CancellationToken,
) {
    loop {
        let accept = tokio::select! {
            a = listener.accept() => a,
            () = shutdown.cancelled() => {
                tracing::info!("Notify listener shutting down");
                let _ = std::fs::remove_file(&socket_path);
                return;
            }
        };

        match accept {
            Ok((stream, _)) => {
                let f = forwarder.clone();
                // Track per-connection tasks so `tracker.wait()` drains
                // in-flight deliveries on shutdown. Short-lived by design.
                tracker.spawn(async move {
                    handle_connection(stream, f).await;
                });
            }
            Err(e) => {
                tracing::warn!(err = %e, "Notify accept failed");
            }
        }
    }
}

/// Read one line of JSON, forward it, write a status line, close.
///
/// Peer authentication: every accepted connection is checked against the
/// bridge's own effective UID. `chmod 0600` is the primary gate (only the
/// owner can `connect(2)`), but it has a TOCTOU window between `bind` and
/// the explicit `chmod`. Peer-cred closes that — if the kernel-reported
/// peer UID doesn't match ours, we refuse without touching the forwarder.
/// On shared-user systems this is the real defense.
async fn handle_connection<F: Forwarder>(mut stream: UnixStream, forwarder: Arc<F>) {
    if !peer_is_self(&stream) {
        // Deliberately minimal reply — don't leak whether the check
        // triggered on uid mismatch vs. getsockopt failure.
        let _ = stream
            .write_all(b"{\"ok\":false,\"error\":\"forbidden\"}\n")
            .await;
        return;
    }

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

    // Log metadata only — `text` may contain secrets or sensitive
    // conversation content, same reason inbound message text isn't logged.
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

/// Read the stream up to the first `\n` or EOF, whichever comes first, with
/// a global `CONNECT_TIMEOUT`. Reject empty payloads and payloads that
/// would exceed `MAX_PAYLOAD_BYTES`.
async fn read_payload(stream: &mut UnixStream) -> Result<Payload> {
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

/// `AsyncBufReadExt::read_until` with a hard cap. Prevents a client that
/// never sends `\n` from growing the buffer without bound.
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

/// Kernel-authenticated peer check — the peer's reported UID must equal
/// the bridge's own effective UID. The pid reported by `peer_cred` is
/// racy (the peer can fork/exec between report and any action we'd take
/// on it), so we only trust `uid`.
///
/// `geteuid` is an async-signal-safe syscall with no failure modes.
/// `peer_cred` can fail on exotic kernels / socket states; treat failure
/// as reject so a misbehaving stack doesn't silently bypass the check.
fn peer_is_self(stream: &UnixStream) -> bool {
    let our_euid = unsafe { libc::geteuid() };
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == our_euid => true,
        Ok(cred) => {
            tracing::warn!(
                peer_uid = cred.uid(),
                our_euid,
                "Notify: rejecting connection from different uid"
            );
            false
        }
        Err(e) => {
            tracing::warn!(err = %e, "Notify: peer_cred failed, rejecting connection");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    //! Protocol tests using `UnixStream::pair()` and a recording `Forwarder`.
    //! This validates that the listener's read / parse / dispatch / write
    //! path is correct without touching a real socket or the Telegram API.

    use super::super::{ForwardError, Forwarder, Payload};
    use super::*;

    use std::sync::Mutex;
    use tokio::io::AsyncReadExt;

    /// Records every payload it's handed so tests can assert on them.
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

    async fn drive(forwarder: Arc<impl Forwarder>, write: &[u8]) -> Vec<u8> {
        let (server, mut client) = UnixStream::pair().expect("UnixStream::pair");
        let handle = tokio::spawn(handle_connection(server, forwarder));

        client.write_all(write).await.unwrap();
        // Half-close write side so the server hits EOF when a newline is
        // absent — otherwise `read_until_bounded` blocks until the 5 s
        // CONNECT_TIMEOUT and we'd measure timeout behavior, not parsing.
        client.shutdown().await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        handle.await.unwrap();
        response
    }

    #[tokio::test]
    async fn peer_is_self_accepts_same_process_socketpair() {
        // `UnixStream::pair` creates a connected pair inside the current
        // process, so the peer uid is the same as our euid — the "allowed"
        // path in `peer_is_self`. The reject path (different uid) can't be
        // exercised in a single-process test; it's defensively coded
        // (log-and-return).
        let (a, _b) = UnixStream::pair().expect("UnixStream::pair");
        assert!(peer_is_self(&a));
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
        // read_until_bounded returns at EOF with whatever's buffered.
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
