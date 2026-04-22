//! Live tmux session table with kill buttons.

use std::collections::HashSet;
use std::fmt::Write as _;

use crate::sanitize;

pub(super) fn build_sessions_table(
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
    // Not-running rows exist only in strict mode — permissive has no fixed allowlist.
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
