//! Preview the inspect dashboard with **mostly real** data — no Telegram
//! credentials needed.
//!
//! Usage:
//!     cargo run --release --example inspect-demo
//!     open <http://127.0.0.1:9090>
//!
//! What's real:
//! - hostname, tmux version (queried at startup)
//! - live tmux sessions on your machine (`tmux list-sessions`)
//! - allowlist = whichever of your live sessions have valid names
//! - process id, bridge version
//!
//! What's honest-null:
//! - bot identity (no `getMe` without a token)
//! - autostart / notify (no env config)
//!
//! What's placeholder:
//! - `allowed_user_id = 0` (there is no real authorized user in demo mode)
//! - activity metrics are seeded with a handful of synthetic events so
//!   the "messages / last response" cards aren't empty

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use tebis::{inspect, metrics, session, tmux};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hyper=warn")),
        )
        .with_target(false)
        .init();

    // Real tmux state: probe with an empty-allowlist Tmux first, then
    // rebuild with whatever valid-named sessions we actually found.
    let probe = tmux::Tmux::new(vec![], 4000);
    let live = probe.list_sessions().await.unwrap_or_default();
    let real_allowlist: Vec<String> = live
        .iter()
        .filter(|s| tmux::is_valid_session_name(s))
        .cloned()
        .collect();

    // If the machine has no tmux sessions, seed the allowlist with a
    // placeholder so the dashboard shows an "allowlisted, not running"
    // row — better than an empty table.
    let allowlist = if real_allowlist.is_empty() {
        vec!["demo-session".into()]
    } else {
        real_allowlist.clone()
    };
    let tmux = Arc::new(tmux::Tmux::new(allowlist.clone(), 4000));
    let default_target = allowlist.first().cloned();

    // SessionState with no autostart — demo doesn't pretend to have a
    // Claude or similar agent running.
    let sessions = Arc::new(session::SessionState::new(None));
    if let Some(t) = default_target.as_ref() {
        sessions.set_target(t.clone());
    }

    let handler_sem = Arc::new(Semaphore::new(8));
    let m = Arc::new(metrics::Metrics::new());

    // Seed a few synthetic events so the activity cards show plausible
    // values instead of all zeros.
    for _ in 0..5 {
        m.record_update_received();
    }
    m.record_handler_completed(342);
    m.record_handler_completed(218);
    m.record_handler_completed(1_173);
    m.record_rate_limited();
    for _ in 0..120 {
        m.record_poll_success();
    }
    m.record_poll_error();

    // Started a few hours ago so the uptime card is interesting.
    let started_at = Instant::now()
        .checked_sub(Duration::from_secs(3 * 3600 + 12 * 60 + 41))
        .unwrap_or_else(Instant::now);

    let snapshot = Arc::new(inspect::Snapshot {
        bridge: inspect::BridgeInfo {
            version: env!("CARGO_PKG_VERSION"),
            pid: std::process::id(),
            hostname: inspect::hostname(),
            tmux_version: inspect::tmux_version().await,
        },
        // Honest-null: no bot identity without a real getMe.
        bot: None,
        allowed_user_id: 0,
        allowed_sessions: allowlist,
        poll_timeout: 30,
        max_output_chars: 4000,
        max_concurrent_handlers: 8,
        autostart: None,
        notify: None,
        // Demo passes through BRIDGE_ENV_FILE if you set it, so you can
        // test the Settings edit flow locally against a throwaway file.
        env_file: std::env::var("BRIDGE_ENV_FILE").ok(),
    });
    let shutdown = CancellationToken::new();
    let tracker = TaskTracker::new();

    let live_ctx =
        inspect::LiveContext::new(tmux, sessions, handler_sem, m, started_at, shutdown.clone());

    // 51624: dynamic-range port (49152-65535) unlikely to collide with
    // anything — Prometheus (9090), common dev servers (3000/5173/8080),
    // and most container tooling all land lower.
    let port: u16 = std::env::var("INSPECT_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(51_624);

    inspect::spawn(&tracker, shutdown.clone(), port, snapshot, live_ctx)?;

    println!();
    println!("  \u{1f50e} inspect demo: http://127.0.0.1:{port}/");
    println!(
        "  live tmux: {}",
        if live.is_empty() {
            "(none — run `tmux new-session -s test` to populate)".to_string()
        } else {
            live.join(", ")
        }
    );
    println!("  press Ctrl-C to exit");
    println!();

    tokio::signal::ctrl_c().await?;
    shutdown.cancel();
    tracker.close();
    let _ = tokio::time::timeout(Duration::from_secs(5), tracker.wait()).await;
    Ok(())
}
