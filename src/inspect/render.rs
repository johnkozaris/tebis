//! HTML + JSON rendering. Single-buffer `format!` calls; inline CSS with
//! a `rem`-based type scale, light/dark via `prefers-color-scheme`.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    let sessions_table = build_sessions_table(
        &live_sessions,
        &allowlist,
        default_target.as_deref(),
        &snapshot.allowed_sessions,
        permissive,
    );
    let bot_rows = build_bot_rows(snapshot.bot.as_ref());
    let autostart_rows = build_autostart_rows(snapshot.autostart.as_ref());
    let notify_rows = build_notify_rows(snapshot.notify.as_ref());
    let hooks_rows = build_hooks_rows(&snapshot.hooks);
    let voice_rows = build_voice_rows(snapshot.voice.as_ref());
    let settings_section = build_settings_section(snapshot);

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
        css = CSS,
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
    )
}

/// All the numbers + formatted-string pieces the template needs, gathered
/// once so `format!` doesn't have to resolve each from `snapshot`/`live`
/// under its own named arg.
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
            format_ago(last_response_raw),
            format!("took {last_response_ms} ms"),
        )
    };

    let last_poll_secs = m.last_poll_success_at.load(Ordering::Relaxed);
    let healthy = last_poll_secs != 0
        && now_unix_secs().saturating_sub(last_poll_secs) < i64::from(2 * snapshot.poll_timeout);
    let (dot_class, status_label) = if healthy {
        ("dot ok", "online")
    } else {
        ("dot warn", "waiting")
    };

    RenderMeta {
        uptime: format_duration(live.started_at.elapsed()),
        dot_class,
        status_label,
        updates_received: m.updates_received.load(Ordering::Relaxed),
        updates_processed: m.updates_processed.load(Ordering::Relaxed),
        rate_limited: m.rate_limited.load(Ordering::Relaxed),
        handler_errors: m.handler_errors.load(Ordering::Relaxed),
        poll_success: m.poll_success.load(Ordering::Relaxed),
        poll_errors: m.poll_errors.load(Ordering::Relaxed),
        last_update_ago: format_ago(m.last_update_at.load(Ordering::Relaxed)),
        last_response_primary,
        last_response_secondary,
        last_poll_success_ago: format_ago(m.last_poll_success_at.load(Ordering::Relaxed)),
        in_flight: snapshot.max_concurrent_handlers.saturating_sub(available),
    }
}

fn build_bot_rows(bot: Option<&super::BotInfo>) -> String {
    bot.map_or_else(
        || r#"<dt>Bot</dt><dd class="muted">unavailable (demo mode)</dd>"#.to_string(),
        |b| {
            let username = b.username.as_deref().map_or_else(
                || r#"<span class="muted">—</span>"#.to_string(),
                |u| format!("<code>@{}</code>", sanitize::escape_html(u)),
            );
            format!(
                "<dt>Bot</dt><dd>{name} · <code>{id}</code> · {username}</dd>",
                name = sanitize::escape_html(&b.first_name),
                id = b.id,
            )
        },
    )
}

fn build_autostart_rows(autostart: Option<&super::AutostartInfo>) -> String {
    autostart.map_or_else(
        || r#"<dt>Autostart</dt><dd class="muted">not configured</dd>"#.to_string(),
        |a| {
            format!(
                "<dt>Autostart session</dt><dd><code>{session}</code></dd>\
                 <dt>Autostart directory</dt><dd><code>{dir}</code></dd>\
                 <dt>Autostart command</dt><dd><code>{cmd}</code></dd>",
                session = sanitize::escape_html(&a.session),
                dir = sanitize::escape_html(&a.dir),
                cmd = sanitize::escape_html(&a.command),
            )
        },
    )
}

fn build_hooks_rows(hooks: &super::HooksInfo) -> String {
    let mut out = format!(
        "<dt>Hooks mode</dt><dd><code>{}</code></dd>",
        sanitize::escape_html(hooks.mode),
    );
    if hooks.entries.is_empty() {
        out.push_str(r#"<dt>Installed hooks</dt><dd class="muted">none</dd>"#);
    } else {
        out.push_str("<dt>Installed hooks</dt><dd><ul class=\"hooks-list\">");
        for entry in &hooks.entries {
            let _ = write!(
                out,
                "<li><code>{agent}</code> · {dir} <span class=\"muted\">({ts})</span></li>",
                agent = sanitize::escape_html(&entry.agent),
                dir = sanitize::escape_html(&entry.dir),
                ts = sanitize::escape_html(&entry.installed_at),
            );
        }
        out.push_str("</ul></dd>");
    }
    out
}

fn build_voice_rows(voice: Option<&super::VoiceInfo>) -> String {
    voice.map_or_else(
        || r#"<dt>Voice input</dt><dd class="muted">disabled</dd>"#.to_string(),
        |v| {
            let status = if v.stt_ready {
                "<span>ready</span>"
            } else {
                r#"<span class="muted">unavailable (see startup logs)</span>"#
            };
            format!(
                "<dt>Voice STT model</dt><dd><code>{model}</code></dd>\
                 <dt>Voice STT status</dt><dd>{status}</dd>",
                model = sanitize::escape_html(&v.stt_model),
                status = status,
            )
        },
    )
}

fn build_notify_rows(notify: Option<&super::NotifyInfo>) -> String {
    notify.map_or_else(
        || r#"<dt>Notify listener</dt><dd class="muted">not configured</dd>"#.to_string(),
        |n| {
            format!(
                "<dt>Notify chat</dt><dd><code>{chat}</code></dd>\
                 <dt>Notify socket</dt><dd><code>{sock}</code></dd>",
                chat = n.chat_id,
                sock = sanitize::escape_html(&n.socket_path),
            )
        },
    )
}

fn build_settings_section(snapshot: &Snapshot) -> String {
    let Some(path) = snapshot.env_file.as_ref() else {
        return r#"<div class="panel"><div class="danger-row"><div class="label"><div class="title">Config editing disabled</div><div class="desc">Set <code>BRIDGE_ENV_FILE</code> to the env file path to enable in-place editing.</div></div></div></div>"#.to_string();
    };
    // Only show the autostart-dir field when autostart is already
    // configured — editing the path of a non-existent autostart makes
    // no sense.
    let autostart_dir_row = snapshot.autostart.as_ref().map_or_else(String::new, |a| {
        format!(
            r#"<div class="settings-row">
    <label>
      <div>Autostart working directory</div>
      <div class="hint">Where <code>{cmd}</code> runs for the autostart session. Must exist.</div>
    </label>
    <input type="text" name="autostart_dir" value="{dir}" size="40" required>
  </div>"#,
            cmd = sanitize::escape_html(&a.command),
            dir = sanitize::escape_html(&a.dir),
        )
    });

    format!(
        r#"<div class="panel"><form method="POST" action="/actions/config" class="settings-form">
  <div class="settings-row">
    <label>
      <div>Long-poll timeout</div>
      <div class="hint">Seconds tebis waits for Telegram updates per request. 1–900.</div>
    </label>
    <input type="number" name="poll_timeout" min="1" max="900" value="{poll}" required>
  </div>
  <div class="settings-row">
    <label>
      <div>Max capture output chars</div>
      <div class="hint">Largest <code>/read</code> response before truncation. 100–20000.</div>
    </label>
    <input type="number" name="max_output_chars" min="100" max="20000" value="{max_chars}" required>
  </div>
  {autostart_dir_row}
  <div class="settings-submit">
    <div class="desc">Writes to <code>{path_html}</code> and restarts the bridge.</div>
    <button type="submit" class="btn btn-primary">Save &amp; restart</button>
  </div>
</form></div>"#,
        poll = snapshot.poll_timeout,
        max_chars = snapshot.max_output_chars,
        path_html = sanitize::escape_html(path),
    )
}

/// Row states depend on mode:
///
/// **Strict**:
/// - live + allowlisted → kill button enabled
/// - live + not allowlisted → no action (bridge can't touch it)
/// - allowlisted + not running → kill disabled (nothing to kill)
///
/// **Permissive**:
/// - every live session → kill button enabled, "allowlisted" badge omitted
fn build_sessions_table(
    live_sessions: &[String],
    allowlist: &HashSet<String>,
    default_target: Option<&str>,
    allowed_sessions: &[String],
    permissive: bool,
) -> String {
    let mut rows = String::new();
    for name in live_sessions {
        let is_allowed = permissive || allowlist.contains(name);
        let is_default = default_target == Some(name.as_str());
        let escaped = sanitize::escape_html(name);
        let mut badges = String::from(r#"<span class="badge badge-ok">running</span>"#);
        if !permissive {
            badges.push_str(if is_allowed {
                r#"<span class="badge badge-ok">allowlisted</span>"#
            } else {
                r#"<span class="badge badge-miss">not allowlisted</span>"#
            });
        }
        if is_default {
            badges.push_str(r#"<span class="badge badge-def">default</span>"#);
        }
        let action = if is_allowed {
            format!(
                r#"<form class="inline" method="POST" action="/actions/kill/{escaped}"><button type="submit">kill</button></form>"#
            )
        } else {
            r#"<span class="muted">—</span>"#.to_string()
        };
        let _ = write!(
            rows,
            r#"<tr><td class="col-name">{escaped}</td><td>{badges}</td><td class="col-actions">{action}</td></tr>"#
        );
    }
    // Pre-declared-but-not-running rows only make sense in strict mode —
    // there is no fixed allowlist in permissive mode.
    if !permissive {
        let mut not_running: Vec<_> = allowed_sessions
            .iter()
            .filter(|s| !live_sessions.contains(s))
            .cloned()
            .collect();
        not_running.sort();
        for name in &not_running {
            let escaped = sanitize::escape_html(name);
            let _ = write!(
                rows,
                r#"<tr><td class="col-name muted">{escaped}</td><td><span class="badge badge-down">not running</span><span class="badge badge-ok">allowlisted</span></td><td class="col-actions"><button disabled>kill</button></td></tr>"#
            );
        }
    }
    if rows.is_empty() {
        rows.push_str(
            r#"<tr><td class="col-empty" colspan="3">no tmux sessions — run <code>tmux new-session -s something</code></td></tr>"#,
        );
    }
    format!(
        r#"<table><thead><tr><th>name</th><th>state</th><th class="col-actions">action</th></tr></thead><tbody>{rows}</tbody></table>"#
    )
}

// ---------- JSON ----------

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

// ---------- formatting ----------

fn format_duration(d: Duration) -> String {
    let mut s = d.as_secs();
    let days = s / 86_400;
    s %= 86_400;
    let hours = s / 3_600;
    s %= 3_600;
    let mins = s / 60;
    let secs = s % 60;
    let mut out = String::new();
    if days > 0 {
        let _ = write!(out, "{days}d ");
    }
    if hours > 0 || days > 0 {
        let _ = write!(out, "{hours}h ");
    }
    if mins > 0 || hours > 0 || days > 0 {
        let _ = write!(out, "{mins}m ");
    }
    let _ = write!(out, "{secs}s");
    out
}

fn format_ago(unix_secs: i64) -> String {
    if unix_secs == 0 {
        return "never".to_string();
    }
    let now = now_unix_secs();
    let delta = now.saturating_sub(unix_secs);
    if delta <= 0 {
        return "0s ago".to_string();
    }
    format!(
        "{} ago",
        format_duration(Duration::from_secs(u64::try_from(delta).unwrap_or(0)))
    )
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ---------- CSS ----------

/// Inline stylesheet for the dashboard. Tokenized with CSS custom
/// properties so the dark-mode override is a single `:root {}` block.
/// Fixed `rem`-based type scale — predictable for a dense app UI.
/// Tabular-nums everywhere a number should align.
const CSS: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #f7f8fa; --surface: #fff; --surface-2: #f2f4f7;
  --border: #e1e4e8; --border-strong: #cfd6de;
  --text: #1f2328; --text-2: #59636e; --text-3: #848f99;
  --accent: #0969da;
  --danger: #cf222e; --danger-bg: #ffeef0;
  --ok: #1a7f37; --ok-bg: #dafbe1;
  --warn: #9a6700; --warn-bg: #fff8c5;
  --def: #0969da; --def-bg: #ddf4ff;
  --ring: color-mix(in srgb, var(--accent) 30%, transparent);
  /* Typography tokens — `--text-*` sizes name roles, not values,
     so a scale change doesn't require a repo-wide find/replace. */
  --text-display: 1.75rem;
  --text-heading: 1.0625rem;
  --text-body: 0.9375rem;
  --text-small: 0.8125rem;
  --text-micro: 0.72rem;
  --font-sans: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
  --font-mono: ui-monospace, "SF Mono", "Cascadia Mono", Menlo, monospace;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0d1117; --surface: #161b22; --surface-2: #1c2128;
    --border: #30363d; --border-strong: #484f58;
    --text: #e6edf3; --text-2: #9198a1; --text-3: #6e7681;
    --accent: #58a6ff;
    --danger: #f85149; --danger-bg: #3b1316;
    --ok: #3fb950; --ok-bg: #033a16;
    --warn: #d29922; --warn-bg: #3b2e01;
    --def: #58a6ff; --def-bg: #0b2a4a;
  }
}
*, *::before, *::after { box-sizing: border-box; }
html { font-size: 16px; }
body {
  font-family: var(--font-sans);
  font-size: var(--text-body); line-height: 1.55;
  color: var(--text); background: var(--bg);
  max-width: 60rem; margin: 0 auto;
  padding: 2.5rem 1.25rem 3rem;
  -webkit-font-smoothing: antialiased;
  -moz-osx-font-smoothing: grayscale;
  font-kerning: normal;
  font-feature-settings: "kern", "liga", "calt";
}
code {
  font-family: var(--font-mono);
  font-size: 0.875em; background: var(--surface-2);
  padding: 0.1em 0.4em; border-radius: 3px;
  word-break: break-word;
}
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
:focus-visible { outline: 2px solid var(--ring); outline-offset: 2px; border-radius: 3px; }

/* HEAD — H1 gets typographic weight (the one H1 on the page).
   Name stays in --accent for a visual anchor; everything else is
   metadata in the smaller, dimmer secondary line. */
.page-head {
  display: flex; align-items: center; gap: 0.875rem;
  padding-bottom: 1.5rem; border-bottom: 1px solid var(--border);
  margin-bottom: 2rem;
}
.dot { width: 10px; height: 10px; border-radius: 50%; flex-shrink: 0; }
.dot.ok   { background: var(--ok);   box-shadow: 0 0 0 4px color-mix(in srgb, var(--ok) 22%, transparent); }
.dot.warn { background: var(--warn); box-shadow: 0 0 0 4px color-mix(in srgb, var(--warn) 22%, transparent); }
h1 {
  font-size: var(--text-display); font-weight: 700; line-height: 1.1;
  margin: 0; letter-spacing: -0.02em; color: var(--accent);
  font-feature-settings: "kern", "liga", "ss01";
}
.page-meta { margin: 0.125rem 0 0; color: var(--text-2); font-size: var(--text-small); font-variant-numeric: tabular-nums; }
.page-meta strong { color: var(--text); font-weight: 600; }
.page-meta .sep { margin: 0 0.4em; color: var(--text-3); }

/* SECTIONS — H2 labels identify groups; kept small-caps-style for a
   "field label" feel without shouting. Generous margin below so the
   label and the panel feel like one unit, not two. */
section { margin-bottom: 2.25rem; }
section:last-of-type { margin-bottom: 0; }
h2 {
  font-size: var(--text-micro); font-weight: 600; letter-spacing: 0.1em;
  text-transform: uppercase; color: var(--text-2); margin: 0 0 0.625rem;
}
.section-lede { margin: -0.25rem 0 0.625rem; color: var(--text-2); font-size: var(--text-small); }
.panel {
  background: var(--surface); border: 1px solid var(--border);
  border-radius: 8px; overflow: hidden;
}

/* STATS — big mono value, tiny label above. Matches the read order
   a user expects ("what am I looking at" → "what's the number"). */
.stats { display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 0.625rem; }
.stat { background: var(--surface); border: 1px solid var(--border); border-radius: 8px; padding: 0.875rem 1rem 0.9rem; }
.stat-label { font-size: var(--text-micro); font-weight: 600; letter-spacing: 0.05em; text-transform: uppercase; color: var(--text-2); }
.stat-value {
  margin-top: 0.375rem;
  font-family: var(--font-mono);
  font-size: 1.3125rem; font-weight: 600;
  font-variant-numeric: tabular-nums; line-height: 1.15;
  color: var(--text); overflow-wrap: anywhere;
  letter-spacing: -0.01em;
}
.stat-sub { margin-top: 0.125rem; color: var(--text-3); font-size: var(--text-small); font-variant-numeric: tabular-nums; }

/* DL — field labels and values, flat (no tinted background / border
   on the dt). Relies on color + weight for hierarchy, which reads as
   "labelled field" instead of "data table cell". */
dl { display: grid; grid-template-columns: minmax(11rem, max-content) 1fr; margin: 0; }
dt, dd { padding: 0.5rem 1rem; margin: 0; border-top: 1px solid var(--border); }
dt {
  color: var(--text-2); font-size: var(--text-small); font-weight: 500;
  white-space: nowrap;
}
dd { color: var(--text); overflow-wrap: anywhere; font-variant-numeric: tabular-nums; }
dd code { overflow-wrap: anywhere; word-break: break-all; }
dt:first-of-type, dt:first-of-type + dd { border-top: 0; }
dd.muted, .muted { color: var(--text-3); }
dd.muted { font-style: italic; }
@media (max-width: 560px) {
  dl { grid-template-columns: 1fr; }
  dt { padding-bottom: 0.15rem; }
  dt:first-of-type { border-top: 1px solid var(--border); }
  dd { padding-top: 0; border-top: 0; }
  dd + dt { border-top: 1px solid var(--border); }
}

/* TABLE — session listing. Header row in small caps, tight body, mono
   for session names so eye-scanning down the column is stable. */
table { width: 100%; border-collapse: collapse; }
thead th {
  padding: 0.45rem 1rem; text-align: left;
  font-size: var(--text-micro); font-weight: 600; letter-spacing: 0.06em;
  text-transform: uppercase; color: var(--text-2);
  background: var(--surface-2); border-bottom: 1px solid var(--border);
}
thead th.col-actions { text-align: right; }
tbody td { padding: 0.5rem 1rem; border-bottom: 1px solid var(--border); vertical-align: middle; }
tbody tr:last-child td { border-bottom: 0; }
td.col-name {
  font-family: var(--font-mono); font-weight: 500; color: var(--text);
  overflow-wrap: anywhere; word-break: break-all;
  font-feature-settings: "ss02";
}
td.col-name.muted { color: var(--text-3); font-weight: 400; }
td.col-actions { text-align: right; white-space: nowrap; width: 0; }
td.col-empty { padding: 1.5rem 1rem; text-align: center; color: var(--text-3); font-style: italic; }

/* BADGES — pill tags, slightly tighter so "running / allowlisted / default"
   can all fit on one row without wrapping in a narrow viewport. */
.badge {
  display: inline-block; padding: 2px 8px; border-radius: 10px;
  font-size: var(--text-micro); font-weight: 600; margin-right: 0.25rem;
  letter-spacing: 0.02em;
}
.badge:last-child { margin-right: 0; }
.badge-ok   { background: var(--ok-bg);     color: var(--ok); }
.badge-miss { background: var(--danger-bg); color: var(--danger); }
.badge-def  { background: var(--def-bg);    color: var(--def); }
.badge-down { background: var(--surface-2); color: var(--text-3); }

/* BUTTONS */
.btn {
  display: inline-block; padding: 0.4rem 0.9rem; border-radius: 5px;
  font: inherit; font-size: var(--text-small); font-weight: 500;
  border: 1px solid var(--border-strong); background: var(--surface);
  color: var(--text); cursor: pointer; text-decoration: none; line-height: 1.3;
  transition: background 60ms linear, border-color 60ms linear;
}
.btn:hover:not(:disabled) { background: var(--surface-2); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-danger { color: var(--danger); border-color: color-mix(in srgb, var(--danger) 35%, var(--border)); }
.btn-danger:hover:not(:disabled) { background: var(--danger-bg); border-color: var(--danger); }
.btn-primary { color: var(--accent); border-color: color-mix(in srgb, var(--accent) 35%, var(--border)); }
.btn-primary:hover:not(:disabled) { background: color-mix(in srgb, var(--accent) 10%, var(--surface)); border-color: var(--accent); }
form.inline { display: inline; margin: 0; }

/* DANGER + SETTINGS — two-column row: label on the left (title + desc),
   control on the right. Title in body weight, desc in small muted copy. */
.danger-row, .settings-row, .settings-submit {
  display: flex; align-items: center; justify-content: space-between;
  gap: 1rem; padding: 0.875rem 1.125rem; border-top: 1px solid var(--border);
}
.danger-row:first-child, .settings-row:first-child { border-top: 0; }
.danger-row .label, .settings-row > label { flex: 1; min-width: 0; }
.danger-row .title, .settings-row > label > :first-child {
  font-weight: 600; color: var(--text); font-size: var(--text-body);
}
.danger-row .desc, .settings-row .hint {
  margin-top: 0.15rem; font-size: var(--text-small); color: var(--text-2);
  line-height: 1.45;
}
.settings-row .hint { color: var(--text-3); }
.settings-row input[type="number"],
.settings-row input[type="text"] {
  padding: 0.35rem 0.65rem;
  font: inherit; font-family: var(--font-mono);
  font-size: var(--text-small); background: var(--surface); color: var(--text);
  border: 1px solid var(--border-strong); border-radius: 4px;
  font-variant-numeric: tabular-nums;
}
.settings-row input[type="number"] { width: 8rem; }
.settings-row input[type="text"]   { width: 22rem; max-width: 100%; }
.settings-row input:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3px var(--ring); }
.settings-submit { background: var(--surface-2); padding: 0.875rem 1.125rem; }
.settings-submit .desc { font-size: var(--text-small); color: var(--text-2); }

/* FOOTER */
footer {
  margin-top: 2.5rem; padding-top: 1rem; border-top: 1px solid var(--border);
  text-align: center; color: var(--text-3); font-size: var(--text-small);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_shapes() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 5s");
        assert_eq!(format_duration(Duration::from_secs(3_661)), "1h 1m 1s");
        assert_eq!(format_duration(Duration::from_secs(86_461)), "1d 0h 1m 1s");
    }

    #[test]
    fn format_ago_never_for_zero() {
        assert_eq!(format_ago(0), "never");
    }
}
