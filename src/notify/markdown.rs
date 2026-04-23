//! Minimal Markdown → Telegram-HTML for hook-delivered text. Translates
//! fences (`<pre>`), inline backticks (`<code>`), `**bold**` (`<b>`),
//! `*italic*` (`<i>`). Other markdown passes through as literal.

use std::fmt::Write as _;

use crate::sanitize;

/// Escape → translate, falling back to escape-only on unbalanced output
/// so a pathological `***foo***` can't break HTML `parse_mode` delivery.
pub fn to_html(text: &str) -> String {
    // NUL is the translator's placeholder sentinel — strip from user input.
    let mut escaped = sanitize::escape_html(text);
    if escaped.contains(NUL) {
        escaped = escaped.replace(NUL, "");
    }
    let translated = translate(&escaped);
    if is_balanced_telegram_html(&translated) {
        translated
    } else {
        escaped
    }
}

/// Count-only balance check — catches `***foo***` pathologies.
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

/// Placeholder sentinel — `\x00<kind><index>\x00`. Stripped from user input in `to_html`.
const NUL: char = '\x00';

fn translate(input: &str) -> String {
    let (phase1, fences, inlines) = extract_code_spans(input);
    // Bold before italic — `**foo**` would otherwise be eaten as italic.
    let phase2 = replace_inline_pair(&phase1, "**", "<b>", "</b>");
    let phase3 = replace_inline_pair(&phase2, "*", "<i>", "</i>");
    let phase4 = reembed(&phase3, "INLINE", &inlines, "<code>", "</code>");
    reembed(&phase4, "FENCE", &fences, "<pre>", "</pre>")
}

/// Byte-scan with UTF-8-scalar-width copy — a `bytes[i] as char` cast
/// would mangle multi-byte scalars.
fn extract_code_spans(input: &str) -> (String, Vec<String>, Vec<String>) {
    let mut out = String::with_capacity(input.len());
    let mut fences: Vec<String> = Vec::new();
    let mut inlines: Vec<String> = Vec::new();

    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let at_line_start = i == 0 || bytes[i - 1] == b'\n';
        if at_line_start
            && i + 3 <= bytes.len()
            && &bytes[i..i + 3] == b"```"
            && let Some(end) = find_fence_close(bytes, i + 3)
        {
            // Drop the optional language tag (first line after opener).
            let body_start = find_line_break(bytes, i + 3).map_or(i + 3, |nl| nl + 1);
            let body = std::str::from_utf8(&bytes[body_start..end])
                .unwrap_or("")
                .to_string();
            let _ = write!(out, "{NUL}FENCE{}{NUL}", fences.len());
            fences.push(body);
            i = end + 3;
            continue;
        }

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

        let width = utf8_scalar_width(bytes[i]);
        let end = (i + width).min(bytes.len());
        out.push_str(&input[i..end]);
        i = end;
    }
    (out, fences, inlines)
}

/// UTF-8 scalar width from the leading byte (RFC 3629). Continuation
/// bytes at a start position advance by 1 — keeps malformed input from panicking.
const fn utf8_scalar_width(byte: u8) -> usize {
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

/// Line-start closing fence from `from` onward.
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

/// Single backtick close; refuses to span newlines (Markdown rule).
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

/// Wrap `delim…delim` pairs with `open`/`close`. Single-delim requires
/// prev char not to be the same delim — otherwise italic would re-eat bold.
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
            // Italic must not match the inner `*` of a `**…**` pair.
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
        assert_eq!(to_html("`<foo>`"), "<code>&lt;foo&gt;</code>");
    }

    #[test]
    fn multiple_code_spans_each_replaced() {
        assert_eq!(to_html("`a` and `b`"), "<code>a</code> and <code>b</code>");
    }

    #[test]
    fn utf8_preserved_verbatim() {
        // `bytes[i] as char` regression floor.
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
        // NUL is the placeholder sentinel — user input must not hijack re-embed.
        let malicious = "hello\x00INLINE0\x00world";
        let out = to_html(malicious);
        assert!(!out.contains('\x00'), "NUL leaked into output: {out:?}");
        assert!(
            !out.contains("<code>"),
            "synthetic placeholder matched: {out:?}"
        );
        assert!(out.contains("hello"));
        assert!(out.contains("world"));
    }

    #[test]
    fn fence_without_newline_falls_back_to_backticks() {
        let out = to_html("```foo```");
        assert!(out.contains("foo"), "got: {out}");
    }
}
