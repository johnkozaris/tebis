//! Pure formatting: [`Payload`] → Telegram HTML message body.
//!
//! No I/O, no tokio — every function here is a total function from inputs
//! to a `String`, which makes them trivial to unit-test.

use std::path::Path;

use super::Payload;
use crate::sanitize;

/// Header is capped so that `<b>{header}</b>\n` + `<pre>…</pre>` (≤ 4000)
/// still fits under Telegram's 4096-char message limit with slack.
const MAX_HEADER_CHARS: usize = 80;

/// Final HTML body. Layout:
///
/// ```text
/// <b>[kind] basename · session</b>
/// <pre>text (truncated if needed)</pre>
/// ```
///
/// If all three header fields are empty, the `<b>…</b>\n` prefix is omitted.
pub fn body(p: &Payload) -> String {
    let header = build_header(p.kind.as_deref(), p.cwd.as_deref(), p.session.as_deref());
    let escaped = sanitize::escape_html(&p.text);
    let wrapped = sanitize::wrap_and_truncate(&escaped, "<pre>", "</pre>");

    if header.is_empty() {
        wrapped
    } else {
        format!("<b>{header}</b>\n{wrapped}")
    }
}

/// Build the `[kind] basename · session` header. All parts are
/// HTML-escaped; the final string is codepoint-truncated to
/// [`MAX_HEADER_CHARS`] with a trailing `…` on overflow.
fn build_header(kind: Option<&str>, cwd: Option<&str>, session: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(tag) = kind.and_then(kind_tag) {
        parts.push(format!("[{tag}]"));
    }
    if let Some(cwd) = cwd {
        let name = Path::new(cwd)
            .file_name()
            .map_or_else(|| cwd.to_string(), |s| s.to_string_lossy().into_owned());
        if !name.is_empty() {
            parts.push(sanitize::escape_html(&name));
        }
    }
    if let Some(session) = session
        && !session.is_empty()
    {
        parts.push(sanitize::escape_html(session));
    }

    let joined = parts.join(" · ");
    truncate_chars(&joined, MAX_HEADER_CHARS)
}

/// Map raw hook event classification to a short header tag.
///
/// - `"stop"` → no tag (the common case; would be noise)
/// - `"subagent_stop"` → `agent`
/// - `"permission_prompt"` → `ask` (user attention needed)
/// - `"idle_prompt"` → `idle`
/// - unknown → no tag (render without, rather than leaking raw values)
fn kind_tag(raw: &str) -> Option<&'static str> {
    match raw {
        "subagent_stop" => Some("agent"),
        "permission_prompt" => Some("ask"),
        "idle_prompt" => Some("idle"),
        _ => None,
    }
}

/// Codepoint-aware truncation. Byte slicing with `&s[..N]` would panic on a
/// mid-codepoint cut, and `.chars().take(N).collect()` allocates twice.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut = s
        .char_indices()
        .nth(max.saturating_sub(1))
        .map_or(s.len(), |(i, _)| i);
    format!("{}…", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(
        text: &str,
        cwd: Option<&str>,
        session: Option<&str>,
        kind: Option<&str>,
    ) -> Payload {
        Payload {
            text: text.into(),
            cwd: cwd.map(Into::into),
            session: session.map(Into::into),
            kind: kind.map(Into::into),
        }
    }

    #[test]
    fn header_empty_when_no_fields() {
        assert_eq!(build_header(None, None, None), "");
    }

    #[test]
    fn header_uses_cwd_basename_not_full_path() {
        assert_eq!(
            build_header(None, Some("/tmp/myproject"), None),
            "myproject"
        );
    }

    #[test]
    fn header_combines_all_three_fields_with_middle_dot() {
        assert_eq!(
            build_header(Some("subagent_stop"), Some("/tmp/myrepo"), Some("s1")),
            "[agent] · myrepo · s1"
        );
    }

    #[test]
    fn header_omits_tag_for_default_stop() {
        assert_eq!(
            build_header(Some("stop"), Some("/tmp/r"), Some("s")),
            "r · s"
        );
    }

    #[test]
    fn header_omits_tag_for_unknown_kind() {
        assert_eq!(
            build_header(Some("weird_future_kind"), Some("/tmp/r"), None),
            "r"
        );
    }

    #[test]
    fn header_tag_for_permission_prompt() {
        assert_eq!(build_header(Some("permission_prompt"), None, None), "[ask]");
    }

    #[test]
    fn header_tag_for_idle_prompt() {
        assert_eq!(build_header(Some("idle_prompt"), None, None), "[idle]");
    }

    #[test]
    fn header_escapes_html_in_both_parts() {
        assert_eq!(
            build_header(None, Some("/tmp/<evil>"), Some("s&1")),
            "&lt;evil&gt; · s&amp;1"
        );
    }

    #[test]
    fn header_truncates_with_ellipsis() {
        let h = build_header(None, Some(&format!("/tmp/{}", "a".repeat(200))), None);
        assert!(h.chars().count() <= MAX_HEADER_CHARS + 1);
        assert!(h.ends_with('…'));
    }

    #[test]
    fn body_with_no_header_is_only_pre() {
        assert_eq!(body(&payload("hi", None, None, None)), "<pre>hi</pre>");
    }

    #[test]
    fn body_with_header_prepends_bold_tag_plus_newline() {
        assert_eq!(
            body(&payload("hi", Some("/tmp/r"), Some("s"), None)),
            "<b>r · s</b>\n<pre>hi</pre>"
        );
    }

    #[test]
    fn body_escapes_text_inside_pre() {
        assert_eq!(
            body(&payload("<script>", None, None, None)),
            "<pre>&lt;script&gt;</pre>"
        );
    }

    #[test]
    fn body_includes_kind_tag() {
        assert_eq!(
            body(&payload("done", None, None, Some("subagent_stop"))),
            "<b>[agent]</b>\n<pre>done</pre>"
        );
    }

    #[test]
    fn truncate_chars_passthrough_when_under_cap() {
        assert_eq!(truncate_chars("hello", 10), "hello");
    }

    #[test]
    fn truncate_chars_cuts_codepoint_aware() {
        // 3 codepoints, each multi-byte in UTF-8.
        assert_eq!(truncate_chars("世界🌍", 2), "世…");
    }
}
