/// Codepoints that render as zero-width, line-break, or bidi overrides.
/// These are dangerous to pass through tmux (terminal confusion) and to
/// forward back to Telegram (can RTL-spoof the client display).
/// See dgl.cx, 2023: <https://dgl.cx/2023/09/ansi-terminal-security>
const fn is_bidi_or_zero_width(c: char) -> bool {
    matches!(c as u32,
        0x200B..=0x200F   // ZWSP, ZWNJ, ZWJ, LRM, RLM
        | 0x2028 | 0x2029 // line/paragraph separators
        | 0x202A..=0x202E // LRE, RLE, PDF, LRO, RLO
        | 0x2066..=0x2069 // LRI, RLI, FSI, PDI
        | 0xFEFF          // BOM / ZWNBSP
    )
}

/// Strip control characters that are dangerous for tmux send-keys.
///
/// Removes:
/// - C0 control chars (0x00–0x1F) — especially CR (0x0d) which executes commands
/// - DEL (0x7F)
/// - C1 control chars (0x80–0x9F) — especially ESC (0x1b) which injects terminal escapes
/// - Bidi / zero-width codepoints — display-confusion attacks
/// - Trailing semicolons (tmux parser bug, tmux issue #1849)
///
/// Allows: printable Unicode (>= 0x20, excluding the ranges above).
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

/// Sanitize tmux capture-pane output before sending to Telegram.
///
/// Defense-in-depth: strip-ansi-escapes handles ANSI sequences, then we
/// manually strip control characters and bidi codepoints. ANSI-stripping
/// alone is not sufficient — see dgl.cx 2023 for terminal-injection CVEs.
pub fn sanitize_tmux_output(output: &str, max_chars: usize) -> String {
    // Layer 1: strip ANSI escape sequences
    let stripped = strip_ansi_escapes::strip_str(output);

    // Layer 2: remove remaining control chars and bidi codepoints
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

/// Escape text for Telegram HTML parse mode.
///
/// Handles **tag content only** (e.g., text inside `<pre>…</pre>`,
/// `<code>…</code>`, `<b>…</b>`). Does NOT escape `"` or `'`, which would
/// be required if we ever emitted attribute values like
/// `<a href="…">`. We never emit attributes — if that changes, use a
/// context-aware escaper instead of reaching for this one.
///
/// Single-pass implementation: the naive three sequential `.replace()`
/// calls this replaced allocated three intermediate `String`s per call
/// (one per replacement). This version pre-sizes the output buffer and
/// walks `text` once, cutting the typical per-reply allocation from
/// 3× input-size bytes to input-size + slack.
pub fn escape_html(text: &str) -> String {
    // Worst case: every byte is `&` → `&amp;` (+4 per byte). For realistic
    // input the ratio is ~0, so +16 slack covers small inputs without
    // under-sizing. Larger inputs grow once at most if we misjudge.
    let mut out: Vec<u8> = Vec::with_capacity(text.len() + 16);
    for &byte in text.as_bytes() {
        match byte {
            b'&' => out.extend_from_slice(b"&amp;"),
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            // Everything else — including every byte of a multi-byte
            // UTF-8 codepoint — is copied verbatim.
            b => out.push(b),
        }
    }
    // SAFETY: `text` is a valid `&str` (guaranteed UTF-8). We only
    // replace three ASCII bytes (`&`, `<`, `>`) with pure-ASCII byte
    // sequences (`&amp;`, `&lt;`, `&gt;`), and preserve every other byte
    // unchanged. The resulting byte sequence is therefore valid UTF-8.
    unsafe { String::from_utf8_unchecked(out) }
}

/// Wrap an already-HTML-escaped body in `open`/`close` tags, guaranteeing the
/// final message fits Telegram's 4096-char limit (we cap at 4000 for slack).
/// If the body is too long, truncate at an HTML-safe boundary: prefer a
/// newline cut, otherwise step back to the nearest codepoint boundary that is
/// not inside an HTML entity like `&amp;`. The longest entity we emit is
/// `&amp;` (5 chars), so we back off to the last `&` within 6 chars of the cut
/// if the intervening slice doesn't contain `;`.
pub fn wrap_and_truncate(escaped_body: &str, open: &str, close: &str) -> String {
    const MAX_MSG: usize = 4000;
    const TRUNC_SUFFIX: &str = "\n... (truncated)";
    let overhead = open.len() + close.len();
    if escaped_body.len() + overhead <= MAX_MSG {
        return format!("{open}{escaped_body}{close}");
    }

    let target = MAX_MSG.saturating_sub(overhead + TRUNC_SUFFIX.len());
    let mut cut = escaped_body.floor_char_boundary(target);

    // Avoid cutting inside an HTML entity.
    if let Some(amp) = escaped_body[..cut].rfind('&') {
        let tail = &escaped_body[amp..cut];
        if !tail.contains(';') && tail.len() < 6 {
            cut = amp;
        }
    }

    // Prefer the last newline within the safe window.
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
        // U+202E = RLO
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
        // 1000 × "line_XXXX\n" ≈ 10_000 chars — clearly over the 4000 limit.
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
        // Build a body whose naive cut would land inside `&amp;`.
        let prefix = "a".repeat(3985);
        let body = format!("{prefix}&amp;tail");
        let wrapped = wrap_and_truncate(&body, "<pre>", "</pre>");
        assert!(wrapped.len() <= 4000);
        // Must not contain a half-entity like "&am" right before the trunc marker.
        let inside = &wrapped["<pre>".len()..wrapped.len() - "</pre>".len()];
        // Every '&' in the output must be followed by ';' within 6 chars.
        for (idx, _) in inside.match_indices('&') {
            let window_end = (idx + 6).min(inside.len());
            assert!(
                inside[idx..window_end].contains(';'),
                "entity truncated at byte {idx} in: {inside}"
            );
        }
    }
}
