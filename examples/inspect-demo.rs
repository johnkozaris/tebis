//! Preview the inspect dashboard with **mostly real** data — no Telegram
//! credentials needed.
//!
//! Usage:
//!     cargo run --release --example inspect-demo
//!     open <http://127.0.0.1:51624>
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
//!
//! ## Dummy-data mode (for screenshots)
//!
//! Set `INSPECT_DEMO_DUMMY=1` to swap every real field for a fake one —
//! hostname becomes "demo-host", pid becomes a made-up integer, tmux
//! sessions are hardcoded, bot identity + autostart + notify all populate
//! with sample values. Useful for capturing dashboard screenshots without
//! leaking your actual machine setup.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use tebis::agent_hooks::HooksMode;
use tebis::bridge::session;
use tebis::{inspect, metrics, tmux};

#[tokio::main]
#[expect(
    clippy::too_many_lines,
    reason = "linear demo wiring; factoring just shuffles it"
)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hyper=warn")),
        )
        .with_target(false)
        .init();

    // `INSPECT_DEMO_DUMMY=1` forces hardcoded sample data so the dashboard
    // can be screenshotted without leaking the real hostname, pid, or
    // whatever tmux sessions are running on the operator's machine.
    let dummy = std::env::var("INSPECT_DEMO_DUMMY").is_ok();

    // Real tmux state: probe with an empty-allowlist Tmux first, then
    // rebuild with whatever valid-named sessions we actually found.
    let live: Vec<String> = if dummy {
        vec!["claude-code".into(), "shell".into(), "notes".into()]
    } else {
        let probe = tmux::Tmux::new(vec![], 4000);
        probe.list_sessions().await.unwrap_or_default()
    };
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
    let sessions = Arc::new(session::SessionState::new(None, HooksMode::Off));
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

    let (hostname, tmux_ver, pid, allowed_user, bot_info, autostart_info, notify_info) = if dummy {
        (
            "demo-host".to_string(),
            "3.4".to_string(),
            12_345,
            1_234_567_890_i64,
            Some(inspect::BotInfo {
                id: 9_876_543_210,
                first_name: "Demo Bridge".into(),
                username: Some("demo_bridge_bot".into()),
            }),
            Some(inspect::AutostartInfo {
                session: "claude-code".into(),
                dir: "/home/demo/Repos/sample-project".into(),
                command: "claude".into(),
            }),
            Some(inspect::NotifyInfo {
                socket_path: "/run/user/1000/tebis.sock".into(),
                chat_id: 1_234_567_890,
            }),
        )
    } else {
        (
            inspect::hostname(),
            inspect::tmux_version().await,
            std::process::id(),
            0,
            None,
            None,
            None,
        )
    };

    let snapshot = Arc::new(inspect::Snapshot {
        bridge: inspect::BridgeInfo {
            version: env!("CARGO_PKG_VERSION"),
            pid,
            hostname,
            tmux_version: tmux_ver,
        },
        bot: bot_info,
        allowed_user_id: allowed_user,
        allowed_sessions: allowlist,
        poll_timeout: 30,
        max_output_chars: 4000,
        max_concurrent_handlers: 8,
        autostart: autostart_info,
        notify: notify_info,
        hooks: inspect::HooksInfo {
            mode: "off",
            entries: Vec::new(),
        },
        voice: Some(inspect::VoiceInfo {
            stt_provider: "local",
            stt_model: "base.en".to_string(),
            stt_ready: true,
        }),
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
