//! HTML + JSON assembly for the inspect dashboard. Helpers live in sibling modules.

mod css;
mod rows;
mod sessions_table;
mod settings;
mod timefmt;

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use super::{LiveContext, Snapshot};
use crate::sanitize;

#[expect(
    clippy::too_many_lines,
    reason = "one HTML template; splitting fragments the layout"
)]
pub(super) async fn html(snapshot: &Snapshot, live: &LiveContext) -> String {
    let live_sessions = live.cached_live_sessions().await;
    let allowlist: HashSet<_> = snapshot.allowed_sessions.iter().cloned().collect();
    let default_target = live.session.target();
    let permissive = snapshot.allowed_sessions.is_empty();

    let meta = build_meta(snapshot, live);
    let sessions_table = sessions_table::build_sessions_table(
        &live_sessions,
        &allowlist,
        default_target.as_deref(),
        &snapshot.allowed_sessions,
        permissive,
    );
    let bot_rows = rows::build_bot_rows(snapshot.bot.as_ref());
    let autostart_rows = rows::build_autostart_rows(snapshot.autostart.as_ref());
    let notify_rows = rows::build_notify_rows(snapshot.notify.as_ref());
    let hooks_rows = rows::build_hooks_rows(&snapshot.hooks);
    let voice_rows = rows::build_voice_rows(snapshot.voice.as_ref(), &live.metrics);
    let settings_section = settings::build_settings_section(snapshot);

    let default_target_display = default_target.as_deref().map_or_else(
        || r#"<span class="muted">none</span>"#.to_string(),
        |s| format!("<code>{}</code>", sanitize::escape_html(s)),
    );

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="refresh" content="5">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>tebis · control</title>
<style>{css}</style>
</head>
<body>

<header class="page-head">
  <div class="{dot_class}" aria-hidden="true"></div>
  <div>
    <h1>tebis</h1>
    <p class="page-meta">
      {status_label}<span class="sep">·</span>v{version}<span class="sep">·</span>pid <code>{pid}</code><span class="sep">·</span>{hostname}<span class="sep">·</span>uptime <strong>{uptime}</strong>
    </p>
  </div>
</header>

<section aria-labelledby="activity-h">
  <h2 id="activity-h">Activity</h2>
  <div class="stats">
    <div class="stat">
      <div class="stat-label">Messages</div>
      <div class="stat-value">{updates_received}</div>
      <div class="stat-sub">{updates_processed} processed · {rate_limited} rate-limited</div>
    </div>
    <div class="stat">
      <div class="stat-label">Last message</div>
      <div class="stat-value">{last_update_ago}</div>
    </div>
    <div class="stat">
      <div class="stat-label">Last response</div>
      <div class="stat-value">{last_response_primary}</div>
      <div class="stat-sub">{last_response_secondary}</div>
    </div>
    <div class="stat">
      <div class="stat-label">Handlers in-flight</div>
      <div class="stat-value">{in_flight} / {max_handlers}</div>
      <div class="stat-sub">{poll_success} polls ok · {poll_errors} err</div>
    </div>
  </div>
</section>

<section aria-labelledby="sessions-h">
  <h2 id="sessions-h">Tmux sessions</h2>
  <p class="section-lede">Default target: {default_target_display}. Plain-text messages route here.</p>
  <div class="panel">{sessions_table}</div>
</section>

<section aria-labelledby="identity-h">
  <h2 id="identity-h">Identity &amp; health</h2>
  <div class="panel"><dl>
    {bot_rows}
    <dt>Authorized Telegram user</dt><dd><code>{allowed_user_id}</code></dd>
    <dt>Tmux version</dt><dd><code>{tmux_version}</code></dd>
    <dt>Last successful poll</dt><dd>{last_poll_success_ago}</dd>
    <dt>Handler errors</dt><dd>{handler_errors}</dd>
    <dt>Live session slots</dt><dd><code>{dynamic_slots}</code></dd>
  </dl></div>
</section>

<section aria-labelledby="config-h">
  <h2 id="config-h">Configuration</h2>
  <p class="section-lede">Read-only snapshot. Edit the env file (or use <em>Settings</em> below) and restart to reload.</p>
  <div class="panel"><dl>
    <dt>Poll timeout</dt><dd><code>{poll_timeout}s</code></dd>
    <dt>Max output chars</dt><dd><code>{max_output_chars}</code></dd>
    <dt>Max handlers</dt><dd><code>{max_handlers}</code></dd>
    <dt>Session allowlist</dt><dd>{allowlist_display}</dd>
    {autostart_rows}
    {notify_rows}
    {hooks_rows}
    {voice_rows}
  </dl></div>
</section>

<section aria-labelledby="settings-h">
  <h2 id="settings-h">Settings</h2>
  {settings_section}
</section>

<section aria-labelledby="danger-h">
  <h2 id="danger-h">Danger zone</h2>
  <div class="panel">
    <div class="danger-row">
      <div class="label">
        <div class="title">Kill all allowlisted sessions</div>
        <div class="desc">Idempotent. Drops the default target so the next plain-text re-provisions.</div>
      </div>
      <form method="POST" action="/actions/kill-all-sessions">
        <button type="submit" class="btn btn-danger">Kill all</button>
      </form>
    </div>
    <div class="danger-row">
      <div class="label">
        <div class="title">Restart bridge</div>
        <div class="desc">Graceful shutdown; launchd / systemd respawns with the current env file.</div>
      </div>
      <form method="POST" action="/actions/restart">
        <button type="submit" class="btn btn-danger">Restart</button>
      </form>
    </div>
  </div>
</section>

<footer>auto-refresh 5 s · <a href="/status">/status.json</a> · <a href="/">refresh now</a></footer>

</body>
</html>
"#,
        css = css::CSS,
        version = snapshot.bridge.version,
        pid = snapshot.bridge.pid,
        hostname = sanitize::escape_html(&snapshot.bridge.hostname),
        tmux_version = sanitize::escape_html(&snapshot.bridge.tmux_version),
        allowed_user_id = snapshot.allowed_user_id,
        allowlist_display = if permissive {
            r#"<span class="muted">any (permissive)</span>"#.to_string()
        } else {
            format!(
                "<code>{}</code>",
                sanitize::escape_html(&snapshot.allowed_sessions.join(", "))
            )
        },
        poll_timeout = snapshot.poll_timeout,
        max_output_chars = snapshot.max_output_chars,
        max_handlers = snapshot.max_concurrent_handlers,
        dot_class = meta.dot_class,
        status_label = meta.status_label,
        uptime = meta.uptime,
        updates_received = meta.updates_received,
        updates_processed = meta.updates_processed,
        rate_limited = meta.rate_limited,
        last_update_ago = meta.last_update_ago,
        last_response_primary = meta.last_response_primary,
        last_response_secondary = meta.last_response_secondary,
        last_poll_success_ago = meta.last_poll_success_ago,
        handler_errors = meta.handler_errors,
        in_flight = meta.in_flight,
        poll_success = meta.poll_success,
        poll_errors = meta.poll_errors,
        dynamic_slots = meta.dynamic_slots,
    )
}

struct RenderMeta {
    uptime: String,
    dot_class: &'static str,
    status_label: &'static str,
    updates_received: u64,
    updates_processed: u64,
    rate_limited: u64,
    handler_errors: u64,
    poll_success: u64,
    poll_errors: u64,
    last_update_ago: String,
    last_response_primary: String,
    last_response_secondary: String,
    last_poll_success_ago: String,
    in_flight: usize,
    dynamic_slots: usize,
}

fn build_meta(snapshot: &Snapshot, live: &LiveContext) -> RenderMeta {
    let m = &live.metrics;
    let available = live.handler_sem.available_permits();
    let last_response_raw = m.last_response_at.load(Ordering::Relaxed);
    let last_response_ms = m.last_response_duration_ms.load(Ordering::Relaxed);
    let (last_response_primary, last_response_secondary) = if last_response_raw == 0 {
        ("never".to_string(), String::new())
    } else {
        (
            timefmt::format_ago(last_response_raw),
            format!("took {last_response_ms} ms"),
        )
    };

    let last_poll_secs = m.last_poll_success_at.load(Ordering::Relaxed);
    let healthy = last_poll_secs != 0
        && timefmt::now_unix_secs().saturating_sub(last_poll_secs) < i64::from(2 * snapshot.poll_timeout);
    let (dot_class, status_label) = if healthy {
        ("dot ok", "online")
    } else {
        ("dot warn", "waiting")
    };

    RenderMeta {
        uptime: timefmt::format_duration(live.started_at.elapsed()),
        dot_class,
        status_label,
        updates_received: m.updates_received.load(Ordering::Relaxed),
        updates_processed: m.updates_processed.load(Ordering::Relaxed),
        rate_limited: m.rate_limited.load(Ordering::Relaxed),
        handler_errors: m.handler_errors.load(Ordering::Relaxed),
        poll_success: m.poll_success.load(Ordering::Relaxed),
        poll_errors: m.poll_errors.load(Ordering::Relaxed),
        last_update_ago: timefmt::format_ago(m.last_update_at.load(Ordering::Relaxed)),
        last_response_primary,
        last_response_secondary,
        last_poll_success_ago: timefmt::format_ago(m.last_poll_success_at.load(Ordering::Relaxed)),
        in_flight: snapshot.max_concurrent_handlers.saturating_sub(available),
        dynamic_slots: live.tmux.dynamic_slot_count(),
    }
}

pub(super) async fn json(snapshot: &Snapshot, live: &LiveContext) -> String {
    use serde_json::json;
    let default_target = live.session.target();
    let available = live.handler_sem.available_permits();
    let in_flight = snapshot.max_concurrent_handlers.saturating_sub(available);
    let live_sessions: Vec<String> = live.cached_live_sessions().await.as_ref().clone();
    let m = &live.metrics;

    json!({
        "bridge": {
            "version": snapshot.bridge.version,
            "pid": snapshot.bridge.pid,
            "hostname": snapshot.bridge.hostname,
            "tmux_version": snapshot.bridge.tmux_version,
        },
        "bot": snapshot.bot.as_ref().map(|b| json!({
            "id": b.id,
            "first_name": b.first_name,
            "username": b.username,
        })),
        "config": {
            "allowed_user_id": snapshot.allowed_user_id,
            "allowed_sessions": snapshot.allowed_sessions,
            "poll_timeout": snapshot.poll_timeout,
            "max_output_chars": snapshot.max_output_chars,
            "max_handlers": snapshot.max_concurrent_handlers,
            "autostart": snapshot.autostart.as_ref().map(|a| json!({
                "session": a.session, "dir": a.dir, "command": a.command,
            })),
            "notify": snapshot.notify.as_ref().map(|n| json!({
                "chat_id": n.chat_id, "socket_path": n.socket_path,
            })),
        },
        "runtime": {
            "uptime_secs": live.started_at.elapsed().as_secs(),
            "default_target": default_target,
            "handlers_in_flight": in_flight,
            "handlers_available": available,
            "live_sessions": live_sessions,
        },
        "metrics": {
            "updates_received": m.updates_received.load(Ordering::Relaxed),
            "updates_processed": m.updates_processed.load(Ordering::Relaxed),
            "rate_limited": m.rate_limited.load(Ordering::Relaxed),
            "handler_errors": m.handler_errors.load(Ordering::Relaxed),
            "poll_success": m.poll_success.load(Ordering::Relaxed),
            "poll_errors": m.poll_errors.load(Ordering::Relaxed),
            "last_update_unix_secs": m.last_update_at.load(Ordering::Relaxed),
            "last_response_unix_secs": m.last_response_at.load(Ordering::Relaxed),
            "last_response_duration_ms": m.last_response_duration_ms.load(Ordering::Relaxed),
            "last_poll_success_unix_secs": m.last_poll_success_at.load(Ordering::Relaxed),
        }
    })
    .to_string()
}
