//! Pane-settle reply detection. Normalization strips Braille spinners
//! (U+2800..U+28FF) + C0/C1 + collapses whitespace so idle frames hash equal.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::typing::TypingGuard;
use crate::telegram::TelegramClient;
use crate::tmux::Tmux;

#[derive(Clone)]
pub struct AutoreplyConfig {
    pub min_wait: Duration,
    pub max_wait: Duration,
    pub poll_interval: Duration,
    pub stable_duration: Duration,
    pub capture_lines: usize,
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

/// Poll → settle → send delta. Spawn on `tracker` (invariant 12).
#[allow(
    clippy::too_many_arguments,
    reason = "a context struct adds more code than it saves"
)]
pub async fn watch_and_forward(
    tracker: TaskTracker,
    tg: Arc<TelegramClient>,
    tmux: Arc<Tmux>,
    session: String,
    chat_id: i64,
    message_id: i64,
    baseline: Option<String>,
    cfg: Arc<AutoreplyConfig>,
    shutdown: CancellationToken,
) {
    // Typing indicator kicks in before min_wait; RAII cancel on every exit.
    let typing = TypingGuard::start(&tracker, tg.clone(), chat_id, &shutdown);

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
                // Keystroke landed; owe the user an ack or they see only fading dots.
                tracing::debug!(session = %session, err = %e, "autoreply: capture failed, falling back to 👍");
                typing.cancel();
                if let Err(re) = tg.set_message_reaction(chat_id, message_id, "👍").await {
                    tracing::warn!(
                        err = %re, session = %session,
                        "autoreply: fallback reaction after capture failure also failed"
                    );
                }
                return;
            }
        }
        tokio::time::sleep(cfg.poll_interval).await;
    }

    typing.cancel();

    let new_content = extract_new(baseline.as_deref(), &latest_pane);
    let tail = tail_chars(new_content.trim(), cfg.tail_chars);
    if tail.trim().is_empty() {
        tracing::debug!(session = %session, "autoreply: nothing new; reacting 👍");
        if let Err(e) = tg.set_message_reaction(chat_id, message_id, "👍").await {
            tracing::warn!(err = %e, session = %session, "autoreply: fallback reaction failed");
        }
        return;
    }
    let body = format_pane_reply(&tail);
    if let Err(e) = tg.send_message(chat_id, &body).await {
        tracing::warn!(err = %e, session = %session, "autoreply: send_message failed, trying 👍 fallback");
        if let Err(re) = tg.set_message_reaction(chat_id, message_id, "👍").await {
            tracing::warn!(
                err = %re, session = %session,
                "autoreply: send_message + reaction both failed — user gets no ack"
            );
        }
    }
}

fn format_pane_reply(pane: &str) -> String {
    let escaped = crate::sanitize::escape_html(pane);
    crate::sanitize::wrap_and_truncate(&escaped, "<pre>", "</pre>")
}

/// Anchor on last 3 non-empty baseline lines; everything after that point in `after` is new.
/// `find` not `rfind` so an echoed/quoted prompt doesn't strip real content before it.
fn extract_new(baseline: Option<&str>, after: &str) -> String {
    let Some(baseline) = baseline else {
        return after.to_string();
    };
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
    after.find(&anchor).map_or_else(
        || after.to_string(),
        |pos| after[pos + anchor.len()..].to_string(),
    )
}

fn normalized_hash(s: &str) -> u64 {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        let cp = c as u32;
        if (0x2800..=0x28FF).contains(&cp) {
            continue;
        }
        if cp < 0x20 && c != '\n' && c != '\t' {
            continue;
        }
        if (0x80..=0x9F).contains(&cp) {
            continue;
        }
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

/// Last `max` chars, prepending `…` when cut.
fn tail_chars(s: &str, max: usize) -> String {
    debug_assert!(max > 0, "tail_chars called with max=0");
    if max == 0 {
        return String::new();
    }
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
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
        let baseline = "old content we wont find";
        let after = "completely different new content";
        assert_eq!(extract_new(Some(baseline), after), after);
    }

    #[test]
    fn extract_new_ignores_empty_lines_in_anchor() {
        let baseline = "line 1\nline 2\nlast real line\n\n\n";
        let after = "line 1\nline 2\nlast real line\n\nthe reply";
        let new = extract_new(Some(baseline), after);
        assert!(new.contains("the reply"));
        assert!(!new.contains("last real line"));
    }

    /// Regression: reply echoes the anchor back; `find` keeps early content.
    #[test]
    fn extract_new_survives_anchor_echoed_in_reply() {
        let baseline = "prompt> run the thing\n> ";
        let after = "prompt> run the thing\n> \
                     Sure, running the thing.\n\
                     prompt> run the thing\n\
                     Done!\n> ";
        let new = extract_new(Some(baseline), after);
        assert!(
            new.contains("Sure, running the thing"),
            "early content must not be stripped; got: {new:?}"
        );
        assert!(new.contains("Done!"), "tail must be included; got: {new:?}");
    }
}
