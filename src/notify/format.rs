//! Pure `Payload` → Telegram HTML body.
//!
//! The hook script hands us text that's already clean — a tail of
//! Claude's final assistant message (Markdown, not terminal output).
//! So unlike the pane-settle path, we do NOT wrap in `<pre>`. We
//! escape HTML entities so Telegram's `parse_mode=HTML` sees
//! plain-looking text, preserving line breaks and intentional
//! inline formatting (bold, italics, code spans) without painting
//! every reply as a fixed-width code block.
//!
//! Header rules: single-user bot, same cwd and same session on every
//! message — so adding a header to every Stop reply is noise. We only
//! prepend a tiny italic tag when the kind is non-obvious (idle, ask,
//! session-up, session-end). Plain `stop` / `subagent_stop` replies
//! get no prefix at all.

use super::{Payload, markdown};
use crate::sanitize;

/// Convert a payload into the final Telegram HTML body.
///
/// Content rules:
/// - Markdown subset translated to Telegram HTML tags (inline + fenced
///   code, bold, italic) so Claude's `**bold**` / `` `code` `` / fenced
///   blocks render correctly instead of as literal punctuation.
/// - HTML entities escaped up front (so `<foo>` and `&` in the reply
///   render literally, not as tags / entities).
/// - Soft-truncate to the Telegram 4096-char ceiling with the shared
///   `wrap_and_truncate` helper.
pub fn body(p: &Payload) -> String {
    let tag_line = p.kind.as_deref().and_then(kind_tag).map(|t| {
        // Italic bracket tag. Kept short so the real content leads.
        format!("<i>[{t}]</i>\n")
    });

    let html = markdown::to_html(&p.text);
    let truncated = sanitize::wrap_and_truncate(&html, "", "");

    match tag_line {
        Some(t) => format!("{t}{truncated}"),
        None => truncated,
    }
}

/// Returns a short human tag for kinds where the user needs to know
/// the flavor. Normal `Stop` / `SubagentStop` replies get no tag
/// because they're the overwhelming majority — labelling them
/// `[reply]` on every message would be chat noise.
fn kind_tag(raw: &str) -> Option<&'static str> {
    match raw {
        // Permission asks ("Claude wants to do X — approve?") and
        // idle prompts are read very differently from normal replies.
        // Claude's notification_type has varied across versions
        // (`permission_prompt` vs `permission_request`); accept both.
        "permission_prompt" | "permission_request" => Some("ask"),
        "idle_prompt" | "idle" => Some("idle"),
        // Subagent results are distinct from the main agent's Stop.
        "subagent_stop" => Some("agent"),
        // `stop` (plain) or any unknown → no tag. Ordinary replies
        // should look like ordinary messages, not tagged system events.
        // session_start / session_end are intentionally dropped from
        // the installed event set; we never receive them at runtime.
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
        // No tag, no header, no <pre>. Just the message.
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
        // Confirms the markdown pipeline runs: backticks become
        // <code> tags so the command is rendered as inline code on
        // Telegram, not literal backticks.
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
        // Claude Code emits `permission_request` in some versions.
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
        // The reply might contain angle brackets — escape them so Telegram
        // doesn't parse them as tags, but do NOT wrap in <pre>.
        let out = body(&payload("Look at <foo> and bar & baz", Some("stop")));
        assert!(!out.contains("<pre>"));
        assert_eq!(out, "Look at &lt;foo&gt; and bar &amp; baz");
    }

    #[test]
    fn newlines_preserved_in_output() {
        // Without <pre>, line breaks come from \n in the source text.
        // Telegram's HTML mode renders \n as line breaks.
        let out = body(&payload("line 1\nline 2\nline 3", Some("stop")));
        assert_eq!(out, "line 1\nline 2\nline 3");
    }

    #[test]
    fn no_cwd_or_session_in_output() {
        // Belt-and-suspenders: even though we moved the cwd/session
        // out of the format, confirm they don't leak into the body.
        let out = body(&payload("hi", Some("stop")));
        assert!(!out.contains("test-project"));
        assert!(!out.contains("de54af3d"));
    }
}
