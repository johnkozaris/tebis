//! TUI-agnostic reply detection: poll the tmux pane, and when its
//! normalized content hash is stable for `stable_duration`, send the
//! **delta since baseline** (or a tail if no baseline) back to Telegram.
//!
//! Why this exists: the hook-based path (Claude Code's `Stop` hook) gives
//! a pinpoint "reply finished" signal but requires per-agent integration.
//! This path needs nothing — if the agent's a TUI in a tmux pane, the
//! pane's rendered buffer is the only input.
//!
//! UX details:
//!
//! - A sub-task sends `sendChatAction=typing` on a 4-second loop so the
//!   chat shows "typing…" until the final message lands.
//! - We diff the settled capture against a baseline captured *before* the
//!   send, so the user sees only what's new — not the agent's whole
//!   scrollback plus banner.
//! - Normalization strategy for settle-detection: strip Braille Pattern
//!   codepoints (U+2800..U+28FF — Ink/React TUI spinners) and C0/C1
//!   control chars, collapse whitespace runs, trim. Two rapid snapshots
//!   of an idle Claude UI hash equal even while the cursor blinks; a
//!   Claude UI streaming tokens hashes differently because alphanumeric
//!   content is actually changing.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::task::TaskTracker;

use super::typing::TypingGuard;
use crate::telegram::TelegramClient;
use crate::tmux::Tmux;

/// Tunables for the pane-settle auto-reply. No env knobs for
/// individual fields in v1 — a single `TELEGRAM_AUTOREPLY=off` switch
/// disables the whole feature. Lives next to `watch_and_forward`
/// (the sole consumer) rather than in `config.rs`, same pattern as
/// `AutostartConfig` / `NotifyConfig`.
#[derive(Clone)]
pub struct AutoreplyConfig {
    /// Minimum wait before the first capture (let `send_keys` land).
    pub min_wait: Duration,
    /// Give-up deadline if the pane never stabilizes.
    pub max_wait: Duration,
    /// How often to capture + hash.
    pub poll_interval: Duration,
    /// How long the normalized hash must be unchanged to declare "settled".
    pub stable_duration: Duration,
    /// Lines of scrollback to capture (same order as `/read`).
    pub capture_lines: usize,
    /// Max chars of the tail we'll actually send.
    pub tail_chars: usize,
}

impl Default for AutoreplyConfig {
    fn default() -> Self {
        Self {
            min_wait: Duration::from_secs(1),
            max_wait: Duration::from_mins(1),
            poll_interval: Duration::from_millis(500),
            stable_duration: Duration::from_secs(3),
            capture_lines: 200,
            tail_chars: 3000,
        }
    }
}

/// Poll the pane until content stabilizes (or the max deadline passes),
/// then send the new content back. Intended for `tracker.spawn` — no
/// return value, all errors are logged + swallowed.
///
/// `tracker` is threaded through so the typing-refresh subtask also
/// runs on the shared `TaskTracker` (CLAUDE.md invariant 12):
/// shutdown drains every outstanding typing loop alongside in-flight
/// handlers.
pub async fn watch_and_forward(
    tracker: TaskTracker,
    tg: Arc<TelegramClient>,
    tmux: Arc<Tmux>,
    session: String,
    chat_id: i64,
    baseline: Option<String>,
    cfg: Arc<AutoreplyConfig>,
) {
    // Kick off the typing indicator immediately — before min_wait — so
    // the user sees feedback within a second of sending. The guard
    // auto-cancels on Drop, which happens at every exit below
    // (early return on capture failure, normal completion after
    // send_message, or max_wait timeout).
    let typing = TypingGuard::start(&tracker, tg.clone(), chat_id);

    tokio::time::sleep(cfg.min_wait).await;

    let start = Instant::now();
    let mut last_hash: Option<u64> = None;
    let mut stable_for = Duration::ZERO;
    let mut latest_pane = String::new();

    loop {
        if start.elapsed() > cfg.max_wait {
            tracing::debug!(session = %session, "autoreply: hit max_wait without settle");
            break;
        }
        match tmux.capture_pane(&session, cfg.capture_lines).await {
            Ok(pane) => {
                let hash = normalized_hash(&pane);
                latest_pane = pane;
                if Some(hash) == last_hash {
                    stable_for += cfg.poll_interval;
                    if stable_for >= cfg.stable_duration {
                        break;
                    }
                } else {
                    stable_for = Duration::ZERO;
                    last_hash = Some(hash);
                }
            }
            Err(e) => {
                tracing::debug!(session = %session, err = %e, "autoreply: capture failed, aborting");
                return;
            }
        }
        tokio::time::sleep(cfg.poll_interval).await;
    }

    // Stop typing once we're ready to send. (Sending a message
    // client-side also clears the indicator, but explicit cancel
    // ends the refresh task immediately.)
    typing.cancel();

    let new_content = extract_new(baseline.as_deref(), &latest_pane);
    let tail = tail_chars(new_content.trim(), cfg.tail_chars);
    if tail.trim().is_empty() {
        tracing::debug!(session = %session, "autoreply: nothing new to send");
        return;
    }
    let body = format_pane_reply(&tail);
    if let Err(e) = tg.send_message(chat_id, &body).await {
        tracing::warn!(err = %e, session = %session, "autoreply: send_message failed");
    }
}

/// Shared HTML-wrap for pane-captured replies. Kept private to this
/// module since only the pane-settle path uses it.
fn format_pane_reply(pane: &str) -> String {
    let escaped = crate::sanitize::escape_html(pane);
    crate::sanitize::wrap_and_truncate(&escaped, "<pre>", "</pre>")
}

/// Return only what's new in `after` relative to `baseline`. If no
/// baseline (first send after autostart) or the anchor can't be located,
/// returns `after` as-is — the later `tail_chars` truncation will handle
/// any excess.
///
/// Anchor strategy: take the last few non-empty lines of the baseline
/// (joined) and look for them in `after`. When found, everything after
/// that point is new. This handles the Claude case well: the baseline's
/// last line is usually the `❯` prompt or the previous response's tail,
/// and the new content begins right after.
fn extract_new(baseline: Option<&str>, after: &str) -> String {
    let Some(baseline) = baseline else {
        return after.to_string();
    };
    // Use the last 3 non-empty baseline lines as the anchor. Fewer →
    // higher chance of false matches (e.g. `❯` appears often); more →
    // might not find a match if the pane scrolled.
    let anchor_lines: Vec<&str> = baseline
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(3)
        .collect();
    if anchor_lines.is_empty() {
        return after.to_string();
    }
    let anchor: String = anchor_lines
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    after.rfind(&anchor).map_or_else(
        || after.to_string(),
        |pos| after[pos + anchor.len()..].to_string(),
    )
}

/// Hash a pane snapshot with animation noise filtered out. See module
/// docs for the normalization rules.
fn normalized_hash(s: &str) -> u64 {
    // Collect normalized output, then trim leading/trailing whitespace
    // and hash the result. Building the string (instead of streaming into
    // the hasher) lets us handle the trim cleanly without lookahead.
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        let cp = c as u32;
        // Skip Braille Pattern spinners.
        if (0x2800..=0x28FF).contains(&cp) {
            continue;
        }
        // Skip C0 (except \n and \t) and C1 controls.
        if cp < 0x20 && c != '\n' && c != '\t' {
            continue;
        }
        if (0x80..=0x9F).contains(&cp) {
            continue;
        }
        // Collapse whitespace runs to a single space.
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
            continue;
        }
        prev_ws = false;
        out.push(c);
    }
    let mut h = DefaultHasher::new();
    out.trim().hash(&mut h);
    h.finish()
}

/// Keep the last `max` characters. Prepends `…` when we actually cut so
/// the recipient can tell the view is a tail.
fn tail_chars(s: &str, max: usize) -> String {
    // `max == 0` would collapse to just "…" which is useless content
    // in a Telegram message. Every configured call passes `tail_chars
    // = 3000`; a zero comes only from a buggy override. Fail loud in
    // debug, best-effort empty in release.
    debug_assert!(max > 0, "tail_chars called with max=0");
    if max == 0 {
        return String::new();
    }
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    // Walk forward until `total - i == max` chars remain.
    let skip = total - max;
    let start = s
        .char_indices()
        .nth(skip)
        .map_or_else(|| s.len(), |(i, _)| i);
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_hash_stable_across_spinner_frames() {
        let a = "Thinking ⠋ about your question";
        let b = "Thinking ⠙ about your question";
        let c = "Thinking ⠹ about your question";
        assert_eq!(normalized_hash(a), normalized_hash(b));
        assert_eq!(normalized_hash(b), normalized_hash(c));
    }

    #[test]
    fn normalized_hash_stable_across_whitespace_wiggle() {
        let a = "claude> ready";
        let b = "claude>  ready";
        let c = "claude> ready   ";
        assert_eq!(normalized_hash(a), normalized_hash(b));
        assert_eq!(normalized_hash(b), normalized_hash(c));
    }

    #[test]
    fn normalized_hash_changes_on_real_content() {
        let a = "claude> working on it";
        let b = "claude> here's your answer";
        assert_ne!(normalized_hash(a), normalized_hash(b));
    }

    #[test]
    fn tail_chars_passes_through_when_under_cap() {
        assert_eq!(tail_chars("hello", 10), "hello");
    }

    #[test]
    fn tail_chars_keeps_last_n_with_ellipsis() {
        let s = "abcdefghij";
        assert_eq!(tail_chars(s, 3), "…hij");
    }

    #[test]
    fn tail_chars_is_codepoint_safe() {
        assert_eq!(tail_chars("世界🌍", 2), "…界🌍");
    }

    #[test]
    fn extract_new_returns_full_when_no_baseline() {
        assert_eq!(extract_new(None, "hello world"), "hello world");
    }

    #[test]
    fn extract_new_strips_baseline_prefix() {
        let baseline = "line 1\nline 2\nline 3";
        let after = "line 1\nline 2\nline 3\nnew reply here\nmore new";
        let new = extract_new(Some(baseline), after);
        assert_eq!(new.trim(), "new reply here\nmore new");
    }

    #[test]
    fn extract_new_falls_back_to_full_on_missing_anchor() {
        // Baseline scrolled off — anchor can't be found in after.
        let baseline = "old content we wont find";
        let after = "completely different new content";
        assert_eq!(extract_new(Some(baseline), after), after);
    }

    #[test]
    fn extract_new_ignores_empty_lines_in_anchor() {
        // Baseline ends with trailing empty lines (common in tmux capture).
        let baseline = "line 1\nline 2\nlast real line\n\n\n";
        let after = "line 1\nline 2\nlast real line\n\nthe reply";
        let new = extract_new(Some(baseline), after);
        assert!(new.contains("the reply"));
        assert!(!new.contains("last real line"));
    }
}
