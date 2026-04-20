//! Opt-in local-only control dashboard. `INSPECT_PORT=<n>` binds
//! `127.0.0.1:n`. Loopback-only, CSRF-checked, zero JS.
//!
//! - `mod.rs` — public types + `spawn` + system helpers
//! - `server.rs` — HTTP accept + routing + actions + env-file write
//! - `render.rs` — HTML + JSON + inline CSS

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

use crate::bridge::session::SessionState;
use crate::metrics::Metrics;
use crate::tmux::Tmux;

/// Immutable snapshot of non-secret config + identity. `bot_token` is
/// deliberately absent — no path from env → dashboard.
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
    pub hooks: HooksInfo,
    /// `Some` → Settings panel becomes editable and writes here.
    pub env_file: Option<String>,
}

/// Hooks-mode policy + snapshot of installed project dirs. `entries`
/// is sampled from the manifest at render time so the dashboard
/// reflects the real state, not a startup freeze.
pub struct HooksInfo {
    pub mode: &'static str,
    pub entries: Vec<HooksEntryInfo>,
}

pub struct HooksEntryInfo {
    pub agent: String,
    pub dir: String,
    pub installed_at: String,
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

/// Live state sampled per-request. `shutdown` is the process-wide
/// cancellation token the Restart action fires.
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

    /// ~2 s cache in front of `tmux list-sessions`. Dashboard auto-refreshes
    /// every 5 s; without the cache that's one subprocess per viewer-refresh.
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
        // Fetch without the mutex so a slow tmux doesn't block concurrent loads.
        let fresh = Arc::new(self.tmux.list_sessions().await.unwrap_or_default());
        let mut guard = self.live_sessions_cache.lock().await;
        *guard = Some((Instant::now(), fresh.clone()));
        fresh
    }
}

/// Binds `127.0.0.1:port` only — non-loopback would leak the dashboard.
///
/// If the port is already in use **by another tebis process** (typically
/// a previous run that didn't exit cleanly), the stale process is killed
/// and the bind is retried. Any other holder surfaces as a normal
/// `AddrInUse` error so we don't clobber unrelated services.
pub fn spawn(
    tracker: &TaskTracker,
    shutdown: CancellationToken,
    port: u16,
    snapshot: Arc<Snapshot>,
    live: LiveContext,
) -> Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let std_listener = bind_with_takeover(addr)?;
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

/// Bind, and on `AddrInUse` take over from a stale tebis process.
fn bind_with_takeover(addr: SocketAddr) -> Result<std::net::TcpListener> {
    match std::net::TcpListener::bind(addr) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            match find_port_holder(addr.port()) {
                Some(holder) if is_tebis_process(holder.pid) => {
                    tracing::warn!(
                        pid = holder.pid,
                        cmd = %holder.cmd,
                        "Port {} held by a stale tebis process — killing and reclaiming",
                        addr.port()
                    );
                    kill_and_wait(holder.pid);
                    std::net::TcpListener::bind(addr)
                        .with_context(|| format!("inspect: rebind {addr} after takeover"))
                }
                Some(holder) => Err(anyhow::anyhow!(
                    "inspect: port {} already in use by pid {} ({}). \
                     Stop that process or pick a different INSPECT_PORT.",
                    addr.port(),
                    holder.pid,
                    holder.cmd,
                )),
                None => Err(anyhow::Error::new(e))
                    .with_context(|| format!("inspect: bind {addr} (holder unknown)")),
            }
        }
        Err(e) => Err(anyhow::Error::new(e)).with_context(|| format!("inspect: bind {addr}")),
    }
}

#[derive(Debug)]
struct PortHolder {
    pid: u32,
    cmd: String,
}

/// Discover which process is holding a local TCP port via `lsof`. Returns
/// `None` if `lsof` isn't installed or no holder is found — callers should
/// fall back to the generic `AddrInUse` error in that case.
fn find_port_holder(port: u16) -> Option<PortHolder> {
    use std::process::Command;
    // `-F pc` prints one field per line: `p<pid>` and `c<command>`.
    let out = Command::new("lsof")
        .args(["-nP", "-sTCP:LISTEN", "-F", "pc"])
        .arg(format!("-iTCP:{port}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut pid = None;
    let mut cmd = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(rest) = line.strip_prefix('p') {
            pid = rest.parse().ok();
        } else if let Some(rest) = line.strip_prefix('c') {
            cmd = Some(rest.to_string());
        }
        if pid.is_some() && cmd.is_some() {
            break;
        }
    }
    Some(PortHolder {
        pid: pid?,
        cmd: cmd.unwrap_or_else(|| "(unknown)".to_string()),
    })
}

/// True when the process at `pid` looks like one of ours — a `tebis` or
/// `inspect-demo` binary. We're deliberately lenient here (substring
/// match on the command) rather than hashing the executable: the purpose
/// is "don't kill your IDE by mistake", not cryptographic identity.
///
/// Caveat: `ps -o comm=` truncates at `TASK_COMM_LEN-1 = 15` chars on
/// macOS / Linux. If the binary is ever renamed to something longer than
/// `inspect-demo` (12 chars), update the match here.
fn is_tebis_process(pid: u32) -> bool {
    use std::process::Command;
    let Ok(out) = Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let comm = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let base = comm.rsplit('/').next().unwrap_or(&comm);
    base == "tebis" || base == "inspect-demo"
}

/// Tunables for `kill_and_wait`. Named so we don't have magic numbers
/// threading through the body.
const TERM_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const TERM_POLL_ATTEMPTS: usize = 30;
const POST_SIGKILL_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

/// SIGTERM + wait + SIGKILL fallback. Synchronous — uses
/// `std::thread::sleep`, which would block a tokio worker if called from
/// async code. Safe here because `bind_with_takeover` runs at startup
/// before the poll loop begins; if a future caller invokes this from
/// async, convert to `tokio::time::sleep`.
fn kill_and_wait(pid: u32) {
    // SAFETY: `kill(2)` with a valid pid is sound. `pid` as i32 can only
    // misbehave for pids ≥ 2^31, which no real system emits.
    unsafe {
        libc::kill(pid.cast_signed(), libc::SIGTERM);
    }
    for _ in 0..TERM_POLL_ATTEMPTS {
        std::thread::sleep(TERM_POLL_INTERVAL);
        // SAFETY: `kill(pid, 0)` is the standard "does this pid exist?"
        // probe. Returns 0 if alive, -1 with ESRCH if gone.
        let alive = unsafe { libc::kill(pid.cast_signed(), 0) } == 0;
        if !alive {
            return;
        }
    }
    tracing::warn!(pid, "stale process didn't exit on SIGTERM; sending SIGKILL");
    unsafe {
        libc::kill(pid.cast_signed(), libc::SIGKILL);
    }
    std::thread::sleep(POST_SIGKILL_WAIT);
}

#[must_use]
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `gethostname` writes at most `buf.len()` bytes into our
    // buffer; no preconditions.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "(unknown)".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

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
