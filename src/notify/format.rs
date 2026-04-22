//! `Payload` → Telegram HTML body. Markdown subset translated; no `<pre>` wrap
//! (hook text is clean Markdown, not terminal output).

use super::{Payload, markdown};
use crate::sanitize;

pub fn body(p: &Payload) -> String {
    let tag_line = p.kind.as_deref().and_then(kind_tag).map(|t| {
        format!("<i>[{t}]</i>\n")
    });

    let html = markdown::to_html(&p.text);
    let truncated = sanitize::wrap_and_truncate(&html, "", "");

    match tag_line {
        Some(t) => format!("{t}{truncated}"),
        None => truncated,
    }
}

/// Tag non-obvious kinds. Plain stop gets no prefix. Claude ships both
/// `permission_prompt` and `permission_request` across versions.
fn kind_tag(raw: &str) -> Option<&'static str> {
    match raw {
        "permission_prompt" | "permission_request" => Some("ask"),
        "idle_prompt" | "idle" => Some("idle"),
        "subagent_stop" => Some("agent"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(text: &str, kind: Option<&str>) -> Payload {
        Payload {
            text: text.into(),
            cwd: Some("/tmp/test-project".into()),
            session: Some("de54af3d-b13e-4b73-b929-190001455ee1".into()),
            kind: kind.map(Into::into),
        }
    }

    #[test]
    fn plain_stop_is_just_text() {
        assert_eq!(
            body(&payload("Hey, still here. What do you need?", Some("stop"))),
            "Hey, still here. What do you need?"
        );
    }

    #[test]
    fn unknown_kind_gets_no_tag() {
        assert_eq!(body(&payload("done", Some("something_new"))), "done",);
    }

    #[test]
    fn idle_gets_italic_tag_prefix() {
        assert_eq!(
            body(&payload("Claude is waiting for input", Some("idle_prompt"))),
            "<i>[idle]</i>\nClaude is waiting for input"
        );
    }

    #[test]
    fn permission_prompt_gets_ask_tag_and_markdown_translated() {
        assert_eq!(
            body(&payload(
                "Run `rm -rf /tmp/foo`?",
                Some("permission_prompt")
            )),
            "<i>[ask]</i>\nRun <code>rm -rf /tmp/foo</code>?"
        );
    }

    #[test]
    fn permission_request_synonym_maps_to_ask_tag() {
        assert_eq!(
            body(&payload("OK?", Some("permission_request"))),
            "<i>[ask]</i>\nOK?"
        );
    }

    #[test]
    fn subagent_stop_gets_agent_tag() {
        assert_eq!(
            body(&payload("done", Some("subagent_stop"))),
            "<i>[agent]</i>\ndone"
        );
    }

    #[test]
    fn html_entities_escaped_but_text_not_pre_wrapped() {
        let out = body(&payload("Look at <foo> and bar & baz", Some("stop")));
        assert!(!out.contains("<pre>"));
        assert_eq!(out, "Look at &lt;foo&gt; and bar &amp; baz");
    }

    #[test]
    fn newlines_preserved_in_output() {
        let out = body(&payload("line 1\nline 2\nline 3", Some("stop")));
        assert_eq!(out, "line 1\nline 2\nline 3");
    }

    #[test]
    fn no_cwd_or_session_in_output() {
        let out = body(&payload("hi", Some("stop")));
        assert!(!out.contains("test-project"));
        assert!(!out.contains("de54af3d"));
    }
}
