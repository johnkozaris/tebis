//! Process-level orchestration: config → wiring → poll loop → shutdown.
//!
//! The split:
//!
//! - `main` — tokio runtime, tracing, panic hook, config load, Telegram
//!   client bring-up, shared-state construction, signal handling, and the
//!   `getUpdates` poll loop with its 409/5xx/Conflict backoff policy.
//! - [`bridge::handle_update`] — per-message behavior: rate limit, parse,
//!   execute, reply. Lives in `bridge.rs` so `main` stays focused on the
//!   lifecycle and the behavior side is testable without spinning up the
//!   whole poll loop.
//!
//! That boundary is what "separate the bridge from the behavior" means in
//! practice: the plumbing here (`main.rs`, `telegram.rs`, `tmux.rs`) knows
//! nothing about commands or autostart; the policy (`bridge.rs`,
//! `handler.rs`, `session.rs`) knows nothing about how bytes arrive.

use anyhow::{Context, Result};
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing_subscriber::EnvFilter;

// Modules live in `lib.rs` so `examples/` can share them.
use tebis::{bridge, config, inspect, metrics, notify, security, session, setup, telegram, tmux};

const HELP: &str = "\
tebis — Telegram-tmux bridge

Usage:
  tebis                 Start the bridge (reads config from env)
  tebis setup           Interactive first-run setup wizard
  tebis --help          Show this message
  tebis --version       Print version

Required env (set by `tebis setup` or your own scripts):
  TELEGRAM_BOT_TOKEN            Bot token from @BotFather
  TELEGRAM_ALLOWED_USER         Your numeric Telegram user id
  TELEGRAM_ALLOWED_SESSIONS     Comma-separated tmux session allowlist

Optional env:
  TELEGRAM_POLL_TIMEOUT         Long-poll seconds (default 30, 1..=900)
  TELEGRAM_MAX_OUTPUT_CHARS     /read truncation cap (default 4000)
  TELEGRAM_AUTOSTART_SESSION    Autostart session name (must be in allowlist)
  TELEGRAM_AUTOSTART_DIR        Autostart cwd
  TELEGRAM_AUTOSTART_COMMAND    Autostart command (e.g. `claude`)
  NOTIFY_CHAT_ID                Enable hook-forward UDS listener
  NOTIFY_SOCKET_PATH            UDS path (default $XDG_RUNTIME_DIR/tebis.sock)
  INSPECT_PORT                  Enable local control dashboard on 127.0.0.1
  BRIDGE_ENV_FILE               Env file path (enables dashboard Settings edits)

Docs: see README.md and CLAUDE.md.
";

fn main() -> Result<()> {
    if let Some(arg) = env::args().nth(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("tebis {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "setup" | "init" => return setup::run(),
            other => {
                eprintln!("unknown argument: {other}\n\n{HELP}");
                std::process::exit(2);
            }
        }
    }

    // No args: start the bridge. If the user hasn't run setup yet and no
    // env is present, point them at the wizard rather than dumping a
    // stack of "env var not set" errors.
    if first_run_check_needed() {
        eprintln!(
            "tebis: no config found — run `tebis setup` first.\n\
             (Expected env vars like TELEGRAM_BOT_TOKEN; see `tebis --help`.)"
        );
        std::process::exit(2);
    }

    run_bridge()
}

/// True when both the primary required env var is missing AND the
/// canonical env file doesn't exist. Either on its own means "not a
/// fresh user", so we only nudge toward `setup` when nothing at all is
/// configured.
fn first_run_check_needed() -> bool {
    if env::var("TELEGRAM_BOT_TOKEN").is_ok_and(|v| !v.is_empty()) {
        return false;
    }
    setup::env_file_path().map_or(true, |p| !p.exists())
}

/// Global cap on concurrent handlers doing tmux work. Single-user bot —
/// realistic need is 1-2 at a time; 8 is generous. Bounds subprocess
/// fan-out when Telegram delivers a burst of queued updates (e.g., after
/// the phone reconnects from offline).
const MAX_CONCURRENT_HANDLERS: usize = 8;

#[tokio::main]
#[expect(
    clippy::too_many_lines,
    reason = "top-level wiring; factoring just shuffles it"
)]
async fn run_bridge() -> Result<()> {
    print_startup_banner();

    // 1. Tracing — low-level HTTP/TLS crates at warn to keep the bot token
    //    (embedded in the request URL) out of debug logs.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,hyper_util=warn,rustls=warn")),
        )
        .with_target(false)
        .init();

    // 2. Redacted panic hook — body of panic is suppressed.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(
            "PANIC at {}: panicked",
            info.location()
                .map_or_else(|| "unknown".to_string(), ToString::to_string)
        );
    }));

    // 3. rustls crypto provider — install once, up front. Must happen
    //    before any TLS handshake. Panics if already installed, which
    //    would indicate a dep silently picked a different backend.
    telegram::install_crypto_provider();

    // 4. Config
    let config = config::Config::from_env()?;
    tracing::info!(
        allowed_user = config.allowed_user_id,
        sessions = ?config.allowed_sessions,
        "Config loaded"
    );

    // 4. Telegram client + webhook teardown + getMe verification
    let tg = Arc::new(telegram::TelegramClient::new(&config.bot_token));
    tg.delete_webhook().await?;
    let me = tg.get_me().await?;
    tracing::info!(
        bot_id = me.id,
        bot_username = ?me.username,
        "Bot connected: {}",
        me.first_name
    );

    // 5. Snapshot non-secret config + identity for the inspect dashboard
    //    BEFORE any field is moved into a subsystem below. `config.bot_token`
    //    is deliberately not copied — no path from env → dashboard.
    let inspect_port: Option<u16> = match env::var("INSPECT_PORT") {
        Ok(s) => Some(
            s.parse()
                .context("INSPECT_PORT must be a valid port number (1..=65535)")?,
        ),
        Err(_) => None,
    };
    let inspect_snapshot = if inspect_port.is_some() {
        let tmux_ver = inspect::tmux_version().await;
        Some(Arc::new(inspect::Snapshot {
            bridge: inspect::BridgeInfo {
                version: env!("CARGO_PKG_VERSION"),
                pid: std::process::id(),
                hostname: inspect::hostname(),
                tmux_version: tmux_ver,
            },
            bot: Some(inspect::BotInfo {
                id: me.id,
                first_name: me.first_name.clone(),
                username: me.username.clone(),
            }),
            allowed_user_id: config.allowed_user_id,
            allowed_sessions: config.allowed_sessions.clone(),
            poll_timeout: config.poll_timeout,
            max_output_chars: config.max_output_chars,
            max_concurrent_handlers: MAX_CONCURRENT_HANDLERS,
            autostart: config.autostart.as_ref().map(|a| inspect::AutostartInfo {
                session: a.session.clone(),
                dir: a.dir.clone(),
                command: a.command.clone(),
            }),
            notify: config.notify.as_ref().map(|n| inspect::NotifyInfo {
                socket_path: n.socket_path.to_string_lossy().into_owned(),
                chat_id: n.chat_id,
            }),
            // Enables the Settings panel edit form when set. systemd
            // unit should export this alongside `EnvironmentFile=`.
            env_file: env::var("BRIDGE_ENV_FILE").ok(),
        }))
    } else {
        None
    };

    // 6. Shared state. `SessionState` owns the mutable session bookkeeping
    //    (default target + autostart + its serialization lock); `Tmux` is
    //    the pure API wrapper.
    let tmux = Arc::new(tmux::Tmux::new(
        config.allowed_sessions,
        config.max_output_chars,
    ));
    if let Some(a) = config.autostart.as_ref() {
        tracing::info!(
            session = %a.session,
            dir = %a.dir,
            command = %a.command,
            "Autostart configured — first plain-text message will provision this session"
        );
    }
    let session_state = Arc::new(session::SessionState::new(config.autostart));
    let rate_limiter = Arc::new(security::RateLimiter::new(30, 10));
    let handler_sem = Arc::new(Semaphore::new(MAX_CONCURRENT_HANDLERS));
    let metrics = Arc::new(metrics::Metrics::new());
    let started_at = Instant::now();

    // 6. Shutdown plumbing — cancel token fans out, task tracker drains.
    let shutdown = CancellationToken::new();
    let tracker = TaskTracker::new();
    {
        let shutdown = shutdown.clone();
        // Tracked so `wait()` on shutdown has a consistent view — the
        // signal task is short-lived (fires `cancel()` and returns) but
        // keeping it under the same TaskTracker is consistent with every
        // other spawned task in the process.
        tracker.spawn(async move {
            shutdown_signal().await;
            tracing::info!("Shutdown signal received");
            shutdown.cancel();
        });
    }

    // 7. Outbound-notify listener (UDS, owner-only). Opt-in via config.
    if let Some(n) = config.notify {
        let forwarder = Arc::new(notify::TelegramForwarder::new(tg.clone(), n.chat_id));
        notify::spawn(&tracker, shutdown.clone(), n.socket_path, forwarder)?;
    }

    // 7b. Inspect dashboard (local HTTP). Opt-in via INSPECT_PORT.
    if let (Some(port), Some(snapshot)) = (inspect_port, inspect_snapshot) {
        let live = inspect::LiveContext::new(
            tmux.clone(),
            session_state.clone(),
            handler_sem.clone(),
            metrics.clone(),
            started_at,
            // Same CancellationToken the poll loop watches — the
            // /actions/restart endpoint cancels it for graceful exit.
            shutdown.clone(),
        );
        inspect::spawn(&tracker, shutdown.clone(), port, snapshot, live)?;
    }

    // 8. Poll loop — pure plumbing. Per-message behavior is in `bridge.rs`.
    let mut offset: Option<i64> = None;
    let mut backoff = Duration::from_secs(1);
    // 409 Conflict means another poller is holding the long-poll. Sleep at
    // least `poll_timeout + 5s` so the prior poller's long-poll expires
    // before we retry, instead of storming the server with 1s-backoff.
    let conflict_backoff = Duration::from_secs(u64::from(config.poll_timeout) + 5);

    tracing::info!(
        max_concurrent_handlers = MAX_CONCURRENT_HANDLERS,
        poll_timeout_secs = config.poll_timeout,
        "Bridge ready"
    );

    loop {
        let poll_result = tokio::select! {
            r = tg.get_updates(offset, config.poll_timeout) => r,
            () = shutdown.cancelled() => break,
        };

        match poll_result {
            Ok(updates) => {
                backoff = Duration::from_secs(1);
                metrics.record_poll_success();
                if updates.len() >= 100 {
                    tracing::info!(
                        count = updates.len(),
                        "getUpdates returned full batch — more may be pending"
                    );
                }

                for update in updates {
                    offset = Some(update.update_id + 1);

                    if !security::is_authorized(&update, config.allowed_user_id) {
                        continue;
                    }

                    let Some(message) = update.message else {
                        continue;
                    };
                    let Some(text) = message.text else { continue };
                    let chat_id = message.chat.id;
                    let message_id = message.message_id;

                    // Never log message content — pasted secrets would leak
                    // to the journal. Observability is bounded to metadata.
                    tracing::debug!(chat_id, bytes = text.len(), "Received message");

                    let ctx = bridge::HandlerContext {
                        tg: tg.clone(),
                        tmux: tmux.clone(),
                        session: session_state.clone(),
                        rate_limiter: rate_limiter.clone(),
                        handler_sem: handler_sem.clone(),
                        started_at,
                        metrics: metrics.clone(),
                    };

                    // NOTE: no per-handler cancel select. `tmux send_keys` is
                    // text-send + submit-gap + Enter-send; cancelling at the
                    // gap would leave uncommitted text in the pane that the
                    // next message would prepend to — dangerous for AI-agent
                    // targets. Shutdown drains via `tracker.wait()` with a
                    // bounded timeout below.
                    tracker.spawn(bridge::handle_update(ctx, chat_id, message_id, text));
                }
            }
            Err(e) if e.is_conflict() => {
                metrics.record_poll_error();
                tracing::warn!(
                    err = %e,
                    backoff_secs = conflict_backoff.as_secs(),
                    "409 Conflict — waiting for server to release prior poller"
                );
                tokio::select! {
                    () = tokio::time::sleep(conflict_backoff) => {}
                    () = shutdown.cancelled() => break,
                }
            }
            Err(e) => {
                metrics.record_poll_error();
                tracing::error!(
                    err = %e,
                    backoff_secs = backoff.as_secs(),
                    "getUpdates failed, backing off"
                );
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    () = shutdown.cancelled() => break,
                }
                backoff = backoff.saturating_mul(2).min(Duration::from_mins(1));
            }
        }
    }

    tracing::info!("Draining in-flight handlers (15s budget)…");
    tracker.close();
    if tokio::time::timeout(Duration::from_secs(15), tracker.wait())
        .await
        .is_ok()
    {
        tracing::info!("Shutdown complete");
    } else {
        tracing::warn!("Shutdown drain timed out; exiting anyway");
    }
    Ok(())
}

/// One-line colored identity banner at startup. Suppressed when stdout
/// isn't a terminal (systemd/launchd logs) so service logs stay clean.
fn print_startup_banner() {
    let term = console::Term::stdout();
    if !term.is_term() {
        return;
    }
    let version = env!("CARGO_PKG_VERSION");
    println!(
        "  {}  {}  {}",
        console::style("tebis").bold().cyan(),
        console::style(format!("v{version}")).dim(),
        console::style("· Telegram → tmux bridge").dim(),
    );
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }
}
