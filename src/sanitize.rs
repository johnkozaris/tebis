//! Sanitizers + HTML escaper. Defends against terminal injection (dgl.cx
//! 2023) and bidi-spoofing.

const fn is_bidi_or_zero_width(c: char) -> bool {
    matches!(c as u32,
        0x200B..=0x200F   // ZWSP, ZWNJ, ZWJ, LRM, RLM
        | 0x2028 | 0x2029 // line/paragraph separators
        | 0x202A..=0x202E // LRE, RLE, PDF, LRO, RLO
        | 0x2066..=0x2069 // LRI, RLI, FSI, PDI
        | 0xFEFF          // BOM / ZWNBSP
    )
}

/// Strips C0/C1/bidi/zero-width. Trailing `;` removed (tmux #1849). 4 KiB cap.
pub fn sanitize_tmux_input(input: &str) -> String {
    let max_len = 4096;
    let truncated = if input.len() > max_len {
        &input[..input.floor_char_boundary(max_len)]
    } else {
        input
    };

    let sanitized: String = truncated
        .chars()
        .filter(|&c| {
            let cp = c as u32;
            cp >= 0x20 && cp != 0x7F && !(0x80..=0x9F).contains(&cp) && !is_bidi_or_zero_width(c)
        })
        .collect();

    sanitized.trim_end_matches(';').to_string()
}

/// ANSI strip + manual pass for stray control/bidi codepoints.
pub fn sanitize_tmux_output(output: &str, max_chars: usize) -> String {
    let stripped = strip_ansi_escapes::strip_str(output);

    let clean: String = stripped
        .chars()
        .filter(|&c| {
            let cp = c as u32;
            if is_bidi_or_zero_width(c) {
                return false;
            }
            c == '\n' || c == '\t' || (cp >= 0x20 && cp != 0x7F && !(0x80..=0x9F).contains(&cp))
        })
        .collect();

    if clean.len() <= max_chars {
        return clean;
    }

    let truncated = &clean[..clean.floor_char_boundary(max_chars)];
    truncated.rfind('\n').map_or_else(
        || format!("{truncated}\n... (truncated)"),
        |pos| format!("{}\n... (truncated)", &truncated[..pos]),
    )
}

/// Tag-content escaper for `parse_mode=HTML`. Does NOT escape quotes —
/// we never emit attribute values.
pub fn escape_html(text: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(text.len() + 16);
    for &byte in text.as_bytes() {
        match byte {
            b'&' => out.extend_from_slice(b"&amp;"),
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            b => out.push(b),
        }
    }
    String::from_utf8(out).expect("escape_html only substitutes ASCII — output is valid UTF-8")
}

// ─── Invariant 6: network-error redaction ────────────────────────────
//
// Shared primitives used by every hyper-error logger. Triggers differ by
// endpoint so each caller passes its own `should_redact` predicate; the
// scaffold (root-cause walk + "kind: <cause>" format + redacted replacement)
// is here to prevent drift.

/// Invariant 6: true when `s` contains `/bot` + ASCII digit (Telegram Bot API URL shape
/// `/bot<TOKEN>/method`). Callers pair with host-level `api.telegram.org` check.
pub(crate) fn contains_bot_token_shape(s: &str) -> bool {
    let bytes = s.as_bytes();
    let needle = b"/bot";
    bytes
        .windows(needle.len())
        .enumerate()
        .any(|(i, w)| w == needle && bytes.get(i + needle.len()).is_some_and(u8::is_ascii_digit))
}

/// Render a hyper-util error into a token-safe string. Walks to root cause,
/// redacts when `should_redact(&raw)` returns true. Invariant 6.
pub(crate) fn redact_hyper_error(
    err: &hyper_util::client::legacy::Error,
    should_redact: impl Fn(&str) -> bool,
) -> String {
    const MAX_SOURCE_DEPTH: usize = 16;
    let mut cur: &dyn std::error::Error = err;
    for _ in 0..MAX_SOURCE_DEPTH {
        let Some(next) = cur.source() else { break };
        cur = next;
    }
    let kind = if err.is_connect() { "connect" } else { "request" };
    let raw = format!("{kind}: {cur}");
    if should_redact(&raw) {
        tracing::warn!("Network error contained sensitive data; replaced with redacted placeholder");
        return format!("{kind}: <redacted network error>");
    }
    raw
}

/// String-input variant of [`redact_hyper_error`] for callers whose errors flatten
/// to `String` before reaching the redaction layer (e.g. `audio::fetch`).
pub(crate) fn redact_hyper_error_string(s: &str, should_redact: impl Fn(&str) -> bool) -> String {
    if should_redact(s) {
        return "<redacted network error>".to_string();
    }
    s.to_string()
}

/// Wrap an already-escaped body, truncating to fit Telegram's 4096 cap.
/// Cut avoids landing inside `&amp;`-style entities and prefers the last newline.
pub fn wrap_and_truncate(escaped_body: &str, open: &str, close: &str) -> String {
    const MAX_MSG: usize = 4000;
    const TRUNC_SUFFIX: &str = "\n... (truncated)";
    let overhead = open.len() + close.len();
    if escaped_body.len() + overhead <= MAX_MSG {
        return format!("{open}{escaped_body}{close}");
    }

    let target = MAX_MSG.saturating_sub(overhead + TRUNC_SUFFIX.len());
    let mut cut = escaped_body.floor_char_boundary(target);

    // Longest entity `escape_html` emits is `&quot;` (6). Bump if you add a longer one.
    const MAX_ENTITY_LEN: usize = 6;
    if let Some(amp) = escaped_body[..cut].rfind('&') {
        let tail = &escaped_body[amp..cut];
        if !tail.contains(';') && tail.len() < MAX_ENTITY_LEN {
            cut = amp;
        }
    }

    if let Some(nl) = escaped_body[..cut].rfind('\n') {
        cut = nl;
    }

    format!("{open}{}{TRUNC_SUFFIX}{close}", &escaped_body[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_carriage_return() {
        assert_eq!(sanitize_tmux_input("hello\rworld"), "helloworld");
    }

    #[test]
    fn strips_escape_sequences() {
        assert_eq!(sanitize_tmux_input("hello\x1b[31mworld"), "hello[31mworld");
    }

    #[test]
    fn strips_null_bytes() {
        assert_eq!(sanitize_tmux_input("hello\0world"), "helloworld");
    }

    #[test]
    fn strips_trailing_semicolons() {
        assert_eq!(sanitize_tmux_input("hello;"), "hello");
        assert_eq!(sanitize_tmux_input("hello;;;"), "hello");
    }

    #[test]
    fn strips_rtl_override() {
        assert_eq!(sanitize_tmux_input("foo\u{202E}bar"), "foobar");
    }

    #[test]
    fn strips_zero_width_joiner() {
        assert_eq!(sanitize_tmux_input("a\u{200B}b\u{200D}c"), "abc");
    }

    #[test]
    fn preserves_normal_text() {
        assert_eq!(sanitize_tmux_input("hello world 123!"), "hello world 123!");
    }

    #[test]
    fn preserves_unicode() {
        assert_eq!(sanitize_tmux_input("hello 世界 🌍"), "hello 世界 🌍");
    }

    #[test]
    fn truncates_long_input() {
        let long = "a".repeat(5000);
        assert!(sanitize_tmux_input(&long).len() <= 4096);
    }

    #[test]
    fn output_strips_ansi_colors() {
        let input = "\x1b[31mred text\x1b[0m";
        let result = sanitize_tmux_output(input, 4000);
        assert_eq!(result, "red text");
    }

    #[test]
    fn output_strips_bidi() {
        let input = "hello\u{202E}evil";
        let result = sanitize_tmux_output(input, 4000);
        assert_eq!(result, "helloevil");
    }

    #[test]
    fn output_truncates_at_newline() {
        let input = "line1\nline2\nline3\nline4";
        let result = sanitize_tmux_output(input, 15);
        assert!(result.contains("truncated"));
        assert!(!result.contains("line4"));
    }

    #[test]
    fn html_escaping() {
        assert_eq!(
            escape_html("<script>alert('xss')</script>"),
            "&lt;script&gt;alert('xss')&lt;/script&gt;"
        );
        assert_eq!(escape_html("a & b"), "a &amp; b");
    }

    #[test]
    fn wrap_and_truncate_fits_under_limit() {
        let body = escape_html("hello");
        let wrapped = wrap_and_truncate(&body, "<pre>", "</pre>");
        assert_eq!(wrapped, "<pre>hello</pre>");
    }

    #[test]
    fn wrap_and_truncate_chops_at_newline() {
        use std::fmt::Write as _;
        let mut body = String::new();
        for i in 0..1000 {
            writeln!(body, "line_{i:04}").unwrap();
        }
        let wrapped = wrap_and_truncate(&body, "<pre>", "</pre>");
        assert!(wrapped.len() <= 4000, "len was {}", wrapped.len());
        assert!(wrapped.starts_with("<pre>"));
        assert!(wrapped.ends_with("</pre>"));
        assert!(wrapped.contains("... (truncated)"));
    }

    #[test]
    fn wrap_and_truncate_respects_entity_boundary() {
        let prefix = "a".repeat(3985);
        let body = format!("{prefix}&amp;tail");
        let wrapped = wrap_and_truncate(&body, "<pre>", "</pre>");
        assert!(wrapped.len() <= 4000);
        let inside = &wrapped["<pre>".len()..wrapped.len() - "</pre>".len()];
        for (idx, _) in inside.match_indices('&') {
            let window_end = (idx + 6).min(inside.len());
            assert!(
                inside[idx..window_end].contains(';'),
                "entity truncated at byte {idx} in: {inside}"
            );
        }
    }

    #[test]
    fn bot_token_shape_detects_digit_after_slash_bot() {
        assert!(contains_bot_token_shape("/bot12345:ABC/getMe"));
        assert!(contains_bot_token_shape("https://api.telegram.org/bot9/getUpdates"));
    }

    #[test]
    fn bot_token_shape_ignores_benign_slash_bot() {
        assert!(!contains_bot_token_shape("/bot"));
        assert!(!contains_bot_token_shape("/bot/dir/"));
        assert!(!contains_bot_token_shape("/robots.txt"));
        for benign in [
            "connect: tcp stream closed to /bot/health",
            "request: /botanical-garden sensor error",
            "request: unrelated string ending with /bot",
            "connect: proxy at 10.0.0.1/bot-proxy refused",
        ] {
            assert!(
                !contains_bot_token_shape(benign),
                "tightened shape should not match benign input: {benign:?}"
            );
        }
    }

    #[test]
    fn bot_token_shape_requires_digit_after_bot() {
        assert!(contains_bot_token_shape("/bot0"));
        assert!(contains_bot_token_shape("/bot9123456789:XYZ"));
        assert!(!contains_bot_token_shape("/bot:"));
        assert!(!contains_bot_token_shape("/botX"));
    }

    #[test]
    fn redact_hyper_error_string_redacts_when_predicate_true() {
        let raw = "connect: https://api.telegram.org/bot123:X/getUpdates refused";
        let out = redact_hyper_error_string(raw, |s| {
            contains_bot_token_shape(s) || s.contains("api.telegram.org")
        });
        assert_eq!(out, "<redacted network error>");
    }

    #[test]
    fn redact_hyper_error_string_passes_through_when_predicate_false() {
        let raw = "connect: dns error: no A record";
        let out = redact_hyper_error_string(raw, |s| s.contains("Bearer "));
        assert_eq!(out, raw);
    }
}
