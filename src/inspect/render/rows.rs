//! `<dl>` row builders for the dashboard.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use crate::sanitize;

pub(super) fn build_bot_rows(bot: Option<&crate::inspect::BotInfo>) -> String {
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

pub(super) fn build_autostart_rows(
    autostart: Option<&crate::inspect::AutostartInfo>,
) -> String {
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

pub(super) fn build_hooks_rows(hooks: &crate::inspect::HooksInfo) -> String {
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

pub(super) fn build_voice_rows(
    voice: Option<&crate::inspect::VoiceInfo>,
    metrics: &crate::metrics::Metrics,
) -> String {
    let Some(v) = voice else {
        return r#"<dt>Voice</dt><dd class="muted">disabled</dd>"#.to_string();
    };

    let mut out = String::new();

    if let Some(model) = &v.stt_model {
        let status = if v.stt_ready {
            "<span>ready</span>"
        } else {
            r#"<span class="muted">unavailable (see startup logs)</span>"#
        };
        let received = metrics.voice_received.load(Ordering::Relaxed);
        let success = metrics.stt_success.load(Ordering::Relaxed);
        let failures = metrics.stt_failures.load(Ordering::Relaxed);
        let last_ms = metrics.last_stt_duration_ms.load(Ordering::Relaxed);
        let activity = if received == 0 {
            "<span class=\"muted\">no voice messages yet</span>".to_string()
        } else {
            format!(
                "<code>{received}</code> received · <code>{success}</code> transcribed \
                 · <code>{failures}</code> failed · last took <code>{last_ms} ms</code>",
            )
        };
        let _ = write!(
            out,
            "<dt>Voice STT model</dt><dd><code>{model}</code></dd>\
             <dt>Voice STT status</dt><dd>{status}</dd>\
             <dt>Voice STT activity</dt><dd>{activity}</dd>",
            model = sanitize::escape_html(model),
        );
    } else {
        out.push_str(r#"<dt>Voice input</dt><dd class="muted">disabled</dd>"#);
    }

    if let Some(voice_name) = &v.tts_voice {
        let tts_success = metrics.tts_success.load(Ordering::Relaxed);
        let tts_failures = metrics.tts_failures.load(Ordering::Relaxed);
        let last_ms = metrics.last_tts_duration_ms.load(Ordering::Relaxed);
        let activity = if tts_success + tts_failures == 0 {
            "<span class=\"muted\">no replies synthesized yet</span>".to_string()
        } else if last_ms == 0 {
            format!("<code>{tts_success}</code> sent · <code>{tts_failures}</code> failed")
        } else {
            format!(
                "<code>{tts_success}</code> sent · <code>{tts_failures}</code> failed · last <code>{last_ms} ms</code>",
            )
        };
        let backend_display = match v.tts_backend {
            "say" => "macOS <code>say</code>".to_string(),
            "kokoro-local" => v.tts_detail.as_deref().map_or_else(
                || "Kokoro local".to_string(),
                |d| {
                    format!(
                        "Kokoro local <span class=\"muted\">({})</span>",
                        sanitize::escape_html(d)
                    )
                },
            ),
            "kokoro-remote" => v.tts_detail.as_deref().map_or_else(
                || "Kokoro remote".to_string(),
                |d| {
                    format!(
                        "Kokoro remote <span class=\"muted\">({})</span>",
                        sanitize::escape_html(d)
                    )
                },
            ),
            _ => sanitize::escape_html(v.tts_backend),
        };
        let _ = write!(
            out,
            "<dt>Voice TTS backend</dt><dd>{backend_display}</dd>\
             <dt>Voice TTS voice</dt><dd><code>{voice}</code> <span class=\"muted\">({scope})</span></dd>\
             <dt>Voice TTS activity</dt><dd>{activity}</dd>",
            voice = sanitize::escape_html(voice_name),
            scope = sanitize::escape_html(v.tts_scope),
        );
    } else {
        out.push_str(r#"<dt>Voice replies</dt><dd class="muted">disabled</dd>"#);
    }

    out
}

pub(super) fn build_notify_rows(notify: Option<&crate::inspect::NotifyInfo>) -> String {
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
