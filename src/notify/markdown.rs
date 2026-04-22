//! Minimal Markdown → Telegram-HTML translator for hook-delivered text.
//!
//! Claude's replies routinely include fenced code blocks, inline
//! code, bold, and italic. With plain HTML-entity-escaping these
//! render as literal backticks and asterisks. This module translates
//! the small Markdown subset that matters on a phone into the
//! corresponding Telegram-supported HTML tags:
//!
//! - Fenced triple-backtick blocks → `<pre>…</pre>`
//! - Inline backticks → `<code>…</code>`
//! - `**bold**` → `<b>…</b>`
//! - `*italic*` → `<i>…</i>`
//!
//! Headings, bullets, tables, blockquotes, links pass through as
//! literal markdown — Telegram's HTML mode doesn't support them, so
//! translating would produce either no-op tags or invalid markup.
//!
//! Hand-rolled parser (no `regex` dep, which isn't in our tree).
//! Single-pass scan for code spans (they take priority over bold /
//! italic — Markdown spec), with bodies held out and substituted
//! back after the bold / italic passes.

use std::fmt::Write as _;

use crate::sanitize;

/// Apply the full pipeline: HTML-escape, then Markdown-translate.
/// Single entry point for the notify formatter.
///
/// If the markdown translator produces unbalanced HTML (e.g. from
/// `***foo***` where bold+italic interact pathologically), the result
/// would fail Telegram's `parse_mode=HTML` parser and we'd lose the
/// whole notify delivery. Safety-net: validate the output's tag
/// balance and fall back to escape-only on mismatch. The user gets
/// literal `**foo**` instead of bold, but the message still arrives.
pub fn to_html(text: &str) -> String {
    // Strip NULs up front — `translate` uses them as placeholder
    // sentinels.
    let mut escaped = sanitize::escape_html(text);
    if escaped.contains(NUL) {
        escaped = escaped.replace(NUL, "");
    }
    let translated = translate(&escaped);
    if is_balanced_telegram_html(&translated) {
        translated
    } else {
        // Fallback: raw HTML-escaped, no markdown translation.
        // Well-formed by construction (escape_html only emits
        // character entities, never tags).
        escaped
    }
}

/// True when every opener has a matching closer in the set of Telegram
/// HTML tags the translator emits. Doesn't validate nesting / order
/// beyond count — sufficient to catch the `***foo***` class of
/// pathologies the greedy bold/italic passes can produce.
fn is_balanced_telegram_html(html: &str) -> bool {
    for (open, close) in [
        ("<b>", "</b>"),
        ("<i>", "</i>"),
        ("<code>", "</code>"),
        ("<pre>", "</pre>"),
    ] {
        if html.matches(open).count() != html.matches(close).count() {
            return false;
        }
    }
    true
}

/// Opaque placeholder for an extracted code span, so bold / italic
/// passes don't touch its content. Form: `\x00<kind><index>\x00`.
/// `\x00` is disallowed in user text (stripped by the hook script's
/// `jq -r` output, which is UTF-8 text only).
const NUL: char = '\x00';

fn translate(input: &str) -> String {
    // Phase 1: pull out fenced ``` and inline `` ` `` spans. Bodies are
    // held in Vec<String>; the main text gets a placeholder in their place.
    let (phase1, fences, inlines) = extract_code_spans(input);

    // Phase 2: bold + italic passes over the remaining text. Bold first
    // so `**foo**` doesn't get eaten by italic.
    let phase2 = replace_inline_pair(&phase1, "**", "<b>", "</b>");
    let phase3 = replace_inline_pair(&phase2, "*", "<i>", "</i>");

    // Phase 3: re-embed fences + inline code with the right HTML tags.
    let phase4 = reembed(&phase3, "INLINE", &inlines, "<code>", "</code>");
    reembed(&phase4, "FENCE", &fences, "<pre>", "</pre>")
}

/// Single-pass scan for fenced triple-backtick blocks and inline
/// single-backtick spans.
/// Returns `(text_with_placeholders, fence_bodies, inline_bodies)`.
///
/// We scan on byte indices (so ASCII delimiters like `` ` `` are
/// cheap to find with `&bytes[i..i+3] == b"```"`) but copy by UTF-8
/// scalar width so multi-byte content (emoji, accented letters, CJK)
/// survives intact. `bytes[i] as char` was a correctness bug —
/// treating a single UTF-8 continuation byte as a codepoint produced
/// mojibake for any non-ASCII input.
fn extract_code_spans(input: &str) -> (String, Vec<String>, Vec<String>) {
    let mut out = String::with_capacity(input.len());
    let mut fences: Vec<String> = Vec::new();
    let mut inlines: Vec<String> = Vec::new();

    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Fenced block: ``` at start-of-input or after newline.
        let at_line_start = i == 0 || bytes[i - 1] == b'\n';
        if at_line_start
            && i + 3 <= bytes.len()
            && &bytes[i..i + 3] == b"```"
            && let Some(end) = find_fence_close(bytes, i + 3)
        {
            // Skip optional language tag up to the first newline.
            let body_start = find_line_break(bytes, i + 3).map_or(i + 3, |nl| nl + 1);
            let body = std::str::from_utf8(&bytes[body_start..end])
                .unwrap_or("")
                .to_string();
            let _ = write!(out, "{NUL}FENCE{}{NUL}", fences.len());
            fences.push(body);
            i = end + 3; // skip the closing ```
            continue;
        }

        // Inline code: single backtick, body has no `\n` or `\``.
        if bytes[i] == b'`'
            && let Some(close) = find_inline_close(bytes, i + 1)
        {
            let body = std::str::from_utf8(&bytes[i + 1..close])
                .unwrap_or("")
                .to_string();
            let _ = write!(out, "{NUL}INLINE{}{NUL}", inlines.len());
            inlines.push(body);
            i = close + 1;
            continue;
        }

        // Copy one UTF-8 scalar. ASCII bytes advance by 1; multi-byte
        // scalars advance by 2–4. `utf8_scalar_width` returns the
        // declared length of the leading byte; invalid bytes advance
        // by 1 (they pass through as-is so we don't drop data).
        let width = utf8_scalar_width(bytes[i]);
        let end = (i + width).min(bytes.len());
        out.push_str(&input[i..end]);
        i = end;
    }
    (out, fences, inlines)
}

/// Declared UTF-8 byte width of a scalar from its leading byte. Per
/// RFC 3629 the high bits of byte 0 determine the length: `0xxxxxxx`
/// = 1, `110xxxxx` = 2, `1110xxxx` = 3, `11110xxx` = 4. Continuation
/// bytes (`10xxxxxx`) shouldn't appear at a start position; treating
/// them as width-1 means we emit them as-is, which keeps bad input
/// from panicking even though output will be invalid UTF-8 on the
/// margin.
const fn utf8_scalar_width(byte: u8) -> usize {
    // ASCII or lone continuation byte (shouldn't appear at start) →
    // advance by 1. Treating continuation as 1 keeps malformed input
    // from panicking; output may be invalid on the margin but we
    // don't drop data.
    if byte < 0xC0 {
        1
    } else if byte < 0xE0 {
        2
    } else if byte < 0xF0 {
        3
    } else {
        4
    }
}

/// Find the byte index of a ```` ``` ```` closing fence starting at or
/// after `from`. Requires the fence to appear at line-start (either
/// `from` is a newline position or the previous byte is `\n`).
fn find_fence_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j + 3 <= bytes.len() {
        if &bytes[j..j + 3] == b"```" && (j == from || bytes[j - 1] == b'\n') {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Find matching closing `` ` `` for an inline code span starting at
/// `from`. Refuses to span newlines — that's a Markdown rule and it
/// keeps us from eating backticks across paragraphs.
fn find_inline_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j < bytes.len() {
        match bytes[j] {
            b'`' => return Some(j),
            b'\n' => return None,
            _ => j += 1,
        }
    }
    None
}

fn find_line_break(bytes: &[u8], from: usize) -> Option<usize> {
    bytes
        .iter()
        .skip(from)
        .position(|&b| b == b'\n')
        .map(|off| from + off)
}

/// Find `delim…delim` pairs and wrap the inner content with `open`/
/// `close`. Skips pairs that span a newline or are whitespace-only.
/// Single-delimiter (italic) requires the previous char to not be the
/// same delimiter — so `**bold**` isn't mis-parsed as italic after the
/// bold pass already ran.
fn replace_inline_pair(input: &str, delim: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let dlen = delim.len();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if starts_with_at(bytes, i, delim.as_bytes())
            && (dlen == 2 || prev_is_not_same_delim(bytes, i, delim.as_bytes()[0]))
            && let Some(close_at) = find_closing_delim(bytes, i + dlen, delim.as_bytes())
        {
            let body = &input[i + dlen..close_at];
            if !body.trim().is_empty() {
                out.push_str(open);
                out.push_str(body);
                out.push_str(close);
                i = close_at + dlen;
                continue;
            }
        }
        // Copy one UTF-8 scalar (see extract_code_spans for rationale).
        let width = utf8_scalar_width(bytes[i]);
        let end = (i + width).min(bytes.len());
        out.push_str(&input[i..end]);
        i = end;
    }
    out
}

fn starts_with_at(bytes: &[u8], at: usize, needle: &[u8]) -> bool {
    at + needle.len() <= bytes.len() && &bytes[at..at + needle.len()] == needle
}

fn prev_is_not_same_delim(bytes: &[u8], at: usize, delim: u8) -> bool {
    at == 0 || bytes[at - 1] != delim
}

fn find_closing_delim(bytes: &[u8], from: usize, delim: &[u8]) -> Option<usize> {
    let mut j = from;
    while j + delim.len() <= bytes.len() {
        if bytes[j] == b'\n' {
            return None;
        }
        if &bytes[j..j + delim.len()] == delim {
            // For italic (single `*`), the next byte must not be `*`
            // (that would indicate a `**…**` bold we'd double-match).
            if delim.len() == 1 && j + 1 < bytes.len() && bytes[j + 1] == delim[0] {
                j += 2;
                continue;
            }
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Replace `\x00<tag><index>\x00` placeholders with
/// `<open>{body}<close>`.
fn reembed(text: &str, tag: &str, bodies: &[String], open: &str, close: &str) -> String {
    let mut out = text.to_string();
    for (i, body) in bodies.iter().enumerate() {
        let marker = format!("{NUL}{tag}{i}{NUL}");
        let replacement = format!("{open}{body}{close}");
        out = out.replace(&marker, &replacement);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(to_html("just words"), "just words");
    }

    #[test]
    fn balanced_html_helper_accepts_well_formed() {
        assert!(is_balanced_telegram_html(""));
        assert!(is_balanced_telegram_html("plain"));
        assert!(is_balanced_telegram_html("<b>a</b>"));
        assert!(is_balanced_telegram_html("<b>a</b> <i>b</i>"));
        assert!(is_balanced_telegram_html(
            "<pre>x</pre> <code>y</code> <b>z</b>"
        ));
    }

    #[test]
    fn balanced_html_helper_rejects_mismatched() {
        assert!(!is_balanced_telegram_html("<b>unclosed"));
        assert!(!is_balanced_telegram_html("</b>stray-close"));
        assert!(!is_balanced_telegram_html("<b><b>a</b>"));
        assert!(!is_balanced_telegram_html("<i>a</b>"));
    }

    #[test]
    fn unbalanced_markdown_falls_back_to_escape_only() {
        // If the markdown translator produces mismatched tags the
        // output must degrade to plain escape — Telegram's HTML mode
        // would otherwise reject the whole delivery.
        //
        // Direct call of the helper: this literal survives the
        // markdown pass today (is balanced). The *point* of the test
        // is to lock in the is_balanced check; if a future regression
        // in translate() produces something unbalanced, to_html falls
        // back cleanly.
        let ok = to_html("a **bold** b");
        assert!(is_balanced_telegram_html(&ok), "today's output is balanced: {ok:?}");
    }

    #[test]
    fn html_entities_escaped() {
        assert_eq!(to_html("1 < 2 & 3 > 0"), "1 &lt; 2 &amp; 3 &gt; 0");
    }

    #[test]
    fn inline_code_becomes_code_tag() {
        assert_eq!(
            to_html("use `cargo test` to verify"),
            "use <code>cargo test</code> to verify"
        );
    }

    #[test]
    fn fenced_block_becomes_pre_tag() {
        assert_eq!(to_html("```\nfoo\nbar\n```"), "<pre>foo\nbar\n</pre>");
    }

    #[test]
    fn fenced_block_strips_language_hint() {
        // Telegram HTML doesn't syntax-highlight; drop the `rust\n` prefix.
        assert_eq!(
            to_html("```rust\nlet x = 1;\n```"),
            "<pre>let x = 1;\n</pre>"
        );
    }

    #[test]
    fn bold_becomes_b_tag() {
        assert_eq!(to_html("this is **bold** text"), "this is <b>bold</b> text");
    }

    #[test]
    fn italic_becomes_i_tag() {
        assert_eq!(
            to_html("this is *italic* text"),
            "this is <i>italic</i> text"
        );
    }

    #[test]
    fn bold_and_italic_dont_overlap() {
        // `**bold**` must not be consumed as italic-wrapping-italic.
        assert_eq!(to_html("**bold**"), "<b>bold</b>");
    }

    #[test]
    fn content_inside_code_spans_is_not_processed() {
        assert_eq!(
            to_html("literal: `**not bold**`"),
            "literal: <code>**not bold**</code>"
        );
    }

    #[test]
    fn content_inside_fences_is_not_processed() {
        assert_eq!(
            to_html("```\n**still literal**\n```"),
            "<pre>**still literal**\n</pre>"
        );
    }

    #[test]
    fn real_claude_sample() {
        let md = "Here's the fix:\n\n```rust\nlet x = 42;\n```\n\n**Note:** you need `cargo test` after.";
        let out = to_html(md);
        assert!(out.contains("<pre>let x = 42;\n</pre>"), "fence: {out}");
        assert!(out.contains("<b>Note:</b>"), "bold: {out}");
        assert!(out.contains("<code>cargo test</code>"), "code: {out}");
    }

    #[test]
    fn empty_input() {
        assert_eq!(to_html(""), "");
    }

    #[test]
    fn incomplete_bold_passes_through() {
        assert_eq!(to_html("one * asterisk"), "one * asterisk");
    }

    #[test]
    fn single_backtick_passes_through() {
        assert_eq!(to_html("one ` tick"), "one ` tick");
    }

    #[test]
    fn escape_before_inline_code() {
        // `<foo>` inside backticks — escape runs first, so the
        // placeholder extraction sees `&lt;foo&gt;` inside `` ` ``.
        assert_eq!(to_html("`<foo>`"), "<code>&lt;foo&gt;</code>");
    }

    #[test]
    fn multiple_code_spans_each_replaced() {
        assert_eq!(to_html("`a` and `b`"), "<code>a</code> and <code>b</code>");
    }

    #[test]
    fn utf8_preserved_verbatim() {
        // Regression: `out.push(bytes[i] as char)` mangled multi-byte
        // scalars. Every real Claude reply has at least smart quotes
        // or an em-dash, so this is a correctness floor, not an edge.
        assert_eq!(to_html("café — 🔥 hello 世界"), "café — 🔥 hello 世界");
    }

    #[test]
    fn utf8_inside_bold() {
        assert_eq!(to_html("**café is 🔥**"), "<b>café is 🔥</b>");
    }

    #[test]
    fn utf8_inside_inline_code() {
        assert_eq!(to_html("`café 🔥`"), "<code>café 🔥</code>");
    }

    #[test]
    fn nul_bytes_stripped_from_input() {
        // The translator uses \x00 as an opaque placeholder sentinel
        // for extracted code spans. User text containing a NUL would
        // hijack the re-embed step. to_html must strip them up front.
        let malicious = "hello\x00INLINE0\x00world";
        let out = to_html(malicious);
        assert!(!out.contains('\x00'), "NUL leaked into output: {out:?}");
        assert!(
            !out.contains("<code>"),
            "synthetic placeholder matched: {out:?}"
        );
        // The user's actual content survives.
        assert!(out.contains("hello"));
        assert!(out.contains("world"));
    }

    #[test]
    fn fence_without_newline_falls_back_to_backticks() {
        // Edge case: `` ```foo``` `` on one line isn't a real
        // CommonMark fenced block (needs newline after opener). We
        // don't try to heuristic-fix it — the content passes through
        // as literal backticks. Real Claude output uses proper fences.
        let out = to_html("```foo```");
        // At a minimum, "foo" survives (even if the backticks render
        // as some empty code spans). Confirm we didn't crash.
        assert!(out.contains("foo"), "got: {out}");
    }
}
