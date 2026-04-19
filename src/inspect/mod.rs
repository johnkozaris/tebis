//! Opt-in local-only control dashboard. `INSPECT_PORT=<n>` binds
//! `127.0.0.1:n` and serves an HTML control panel + JSON status endpoint.
//!
//! Loopback-only (no auth), CSRF-protected (`Origin` header check), zero
//! JS, server-rendered via `format!`. Split across three files:
//!
//! - `mod.rs` — public types, `spawn` entrypoint, and system-info helpers.
//! - `server.rs` — HTTP accept loop, routing, action endpoints, CSRF,
//!   env-file I/O.
//! - `render.rs` — HTML + JSON renderers, inline CSS.

mod render;
mod server;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::metrics::Metrics;
use crate::session::SessionState;
use crate::tmux::Tmux;

/// Immutable snapshot of non-secret config + identity. The bot token is
/// deliberately NOT a field here — can't leak through the dashboard if
/// it isn't reachable.
pub struct Snapshot {
    pub bridge: BridgeInfo,
    pub bot: Option<BotInfo>,
    pub allowed_user_id: i64,
    pub allowed_sessions: Vec<String>,
    pub poll_timeout: u32,
    pub max_output_chars: usize,
    pub max_concurrent_handlers: usize,
    pub autostart: Option<AutostartInfo>,
    pub notify: Option<NotifyInfo>,
    /// Path the bridge reads its env from. `Some` → Settings panel
    /// becomes editable and writes here. `None` → panel renders
    /// read-only.
    pub env_file: Option<String>,
}

pub struct BridgeInfo {
    pub version: &'static str,
    pub pid: u32,
    pub hostname: String,
    pub tmux_version: String,
}

pub struct BotInfo {
    pub id: i64,
    pub first_name: String,
    pub username: Option<String>,
}

pub struct AutostartInfo {
    pub session: String,
    pub dir: String,
    pub command: String,
}

pub struct NotifyInfo {
    pub socket_path: String,
    pub chat_id: i64,
}

/// Live state sampled per-request. `shutdown` is the process-wide token
/// that the Restart action cancels to trigger a graceful exit + respawn.
///
/// `live_sessions_cache` fronts `tmux list-sessions` with a short TTL.
/// The dashboard auto-refreshes every 5 s; without the cache that's
/// one subprocess fork per refresh per viewer.
pub struct LiveContext {
    pub tmux: Arc<Tmux>,
    pub session: Arc<SessionState>,
    pub handler_sem: Arc<Semaphore>,
    pub metrics: Arc<Metrics>,
    pub started_at: Instant,
    pub shutdown: CancellationToken,
    live_sessions_cache: tokio::sync::Mutex<Option<(Instant, Arc<Vec<String>>)>>,
}

impl LiveContext {
    #[must_use]
    pub fn new(
        tmux: Arc<Tmux>,
        session: Arc<SessionState>,
        handler_sem: Arc<Semaphore>,
        metrics: Arc<Metrics>,
        started_at: Instant,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            tmux,
            session,
            handler_sem,
            metrics,
            started_at,
            shutdown,
            live_sessions_cache: tokio::sync::Mutex::new(None),
        }
    }

    /// Return the live tmux session list, using a ~2 s cache so
    /// multiple renders in quick succession share one subprocess spawn.
    /// Errors from tmux collapse to an empty list (same policy as the
    /// uncached path).
    pub async fn cached_live_sessions(&self) -> Arc<Vec<String>> {
        const TTL: std::time::Duration = std::time::Duration::from_millis(1_800);
        {
            let guard = self.live_sessions_cache.lock().await;
            if let Some((at, cached)) = guard.as_ref()
                && at.elapsed() < TTL
            {
                return cached.clone();
            }
        }
        // Cache miss. Fetch WITHOUT holding the mutex so a slow tmux
        // doesn't block concurrent dashboard loads.
        let fresh = Arc::new(self.tmux.list_sessions().await.unwrap_or_default());
        let mut guard = self.live_sessions_cache.lock().await;
        *guard = Some((Instant::now(), fresh.clone()));
        fresh
    }
}

/// Start the accept loop on `tracker`. Binds `127.0.0.1:port` only — a
/// non-loopback bind would leak the dashboard to the network.
pub fn spawn(
    tracker: &TaskTracker,
    shutdown: CancellationToken,
    port: u16,
    snapshot: Arc<Snapshot>,
    live: LiveContext,
) -> Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let std_listener =
        std::net::TcpListener::bind(addr).with_context(|| format!("inspect: bind {addr}"))?;
    std_listener
        .set_nonblocking(true)
        .context("inspect: set_nonblocking")?;
    let listener = TcpListener::from_std(std_listener).context("inspect: TcpListener::from_std")?;
    tracing::info!(addr = %addr, "Inspect dashboard bound (loopback only)");

    let live = Arc::new(live);
    let expected_origins = Arc::new(server::expected_origins_for(port));
    let tracker_for_conns = tracker.clone();
    tracker.spawn(server::accept_loop(
        listener,
        shutdown,
        snapshot,
        live,
        expected_origins,
        tracker_for_conns,
    ));
    Ok(())
}

/// Host name via `gethostname(2)`. Startup-only; falls back to
/// `"(unknown)"`.
#[must_use]
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `gethostname` writes at most `buf.len()` bytes into the
    // provided buffer. No preconditions beyond a valid writable region.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "(unknown)".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// tmux version string via `tmux -V`. Startup-only. Returns the bare
/// version (no `tmux ` prefix) or `"(unknown)"` on failure.
pub async fn tmux_version() -> String {
    use tokio::process::Command;
    match Command::new("tmux").arg("-V").output().await {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .trim()
            .strip_prefix("tmux ")
            .map_or_else(
                || String::from_utf8_lossy(&out.stdout).trim().to_string(),
                ToString::to_string,
            ),
        _ => "(unknown)".to_string(),
    }
}
