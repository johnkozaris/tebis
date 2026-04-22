//! UDS binding, accept loop, per-connection protocol.

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

/// 16 KiB is ~10× the advertised 1500-char body limit.
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Three-layer defense so the socket never exists at a looser mode:
/// (1) tightened `umask(0177)` around `bind(2)`, (2) explicit `chmod 0600`,
/// (3) `peer_cred` check in `handle_connection`. Layers (1)+(2) close the
/// window between bind and the first accept; (3) is the only
/// authenticated gate. Don't remove any in isolation.
///
/// Pre-bind unlink is safe on the `/tmp/tebis-$USER.sock` fallback even
/// though `/tmp` is world-writable:
/// - `remove_file` → `unlink(2)`, which does NOT follow symlinks. An
///   attacker who pre-creates the path as a symlink to `/etc/passwd`
///   only has its own symlink unlinked; the target is untouched.
/// - The sticky bit on `/tmp` prevents attacker-owned files/symlinks
///   from being unlinked by us (EPERM). We surface that as a non-
///   NotFound `Err` and bail — no clobber path.
/// - If the path is clean or ours to unlink, `bind(2)` then creates a
///   fresh inode. A race where an attacker re-creates the path between
///   our unlink and our bind results in `bind` failing with
///   `EADDRINUSE` (bind refuses existing paths); still no clobber.
fn bind(path: &Path) -> Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to unlink stale socket at {}", path.display()));
        }
    }

    // SAFETY: `umask(2)` is async-signal-safe; prior mask restored
    // unconditionally below so a bind failure doesn't leak the tightening.
    let prior_umask = unsafe { libc::umask(0o177) };
    let listener_result = UnixListener::bind(path);
    unsafe { libc::umask(prior_umask) };

    let listener = listener_result
        .with_context(|| format!("failed to bind notify socket at {}", path.display()))?;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod socket to 0600 at {}", path.display()))?;

    Ok(listener)
}

/// RAII guard: removes the socket file on drop so the listener can't
/// exit any path (shutdown, panic, task cancel) without cleaning up.
/// Before this, a panic in the accept loop left a stale socket that
/// subsequent hook firings happily `connect(2)`-ed to, only to see
/// `ECONNREFUSED` since no one was accepting.
struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Minimum sleep between consecutive failed `accept(2)` calls.
/// Without this, a sticky error like `EMFILE` (process fd exhaustion)
/// burns CPU and fills the journal with warnings at whatever rate the
/// kernel hands them back.
const ACCEPT_ERROR_COOLDOWN: Duration = Duration::from_millis(100);

async fn accept_loop<F: Forwarder>(
    listener: UnixListener,
    socket_path: PathBuf,
    forwarder: Arc<F>,
    tracker: TaskTracker,
    shutdown: CancellationToken,
) {
    let _cleanup = SocketCleanup(socket_path);
    loop {
        let accept = tokio::select! {
            a = listener.accept() => a,
            () = shutdown.cancelled() => {
                tracing::info!("Notify listener shutting down");
                return;
            }
        };

        match accept {
            Ok((stream, _)) => {
                let f = forwarder.clone();
                // Per-connection tasks tracked so `tracker.wait()` drains
                // in-flight deliveries on shutdown.
                tracker.spawn(async move {
                    handle_connection(stream, f).await;
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

/// Per-accept peer-cred check against our euid closes the TOCTOU window
/// between `bind` and `chmod 0600`. On shared-user systems this is the
/// authenticated gate.
async fn handle_connection<F: Forwarder>(mut stream: UnixStream, forwarder: Arc<F>) {
    if !peer_is_self(&stream) {
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

    // Metadata only — `text` may contain secrets.
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

/// Read up to `\n` or EOF, bounded by `CONNECT_TIMEOUT` + `MAX_PAYLOAD_BYTES`.
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

/// `read_until` with a hard cap — a client that never sends `\n` can't
/// grow the buffer unbounded.
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

/// Kernel-authenticated peer check. `peer_cred`'s pid is racy (the peer
/// can fork/exec), so we only trust `uid`. `peer_cred` failure → reject,
/// so a misbehaving stack can't bypass the check.
fn peer_is_self(stream: &UnixStream) -> bool {
    // SAFETY: `geteuid` is async-signal-safe with no failure modes.
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
    //! Protocol tests via `UnixStream::pair()` + a recording `Forwarder`.

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
