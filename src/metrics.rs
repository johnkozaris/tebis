//! Lock-free counters + last-event timestamps. Shared across the poll
//! loop, every handler, and the inspect dashboard via `Arc<Metrics>`.
//!
//! All fields are atomics — no mutex, no lock contention on the fast
//! path. Timestamps are stored as seconds since the Unix epoch (i64)
//! with 0 meaning "never recorded yet" so readers can distinguish the
//! fresh-process case.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct Metrics {
    /// Total Telegram updates handed to a handler task (post-auth filter).
    pub updates_received: AtomicU64,
    /// Handlers that ran to completion (success or handled error). A
    /// divergence between received and processed is in-flight or dropped
    /// by rate limit.
    pub updates_processed: AtomicU64,
    /// Rate-limited replies sent. Subset of `updates_received`.
    pub rate_limited: AtomicU64,
    /// `send_message` / `set_message_reaction` calls that failed.
    pub handler_errors: AtomicU64,
    /// Successful `getUpdates` poll returns.
    pub poll_success: AtomicU64,
    /// Failed `getUpdates` attempts (incl. 409 conflict + network).
    pub poll_errors: AtomicU64,

    /// Unix seconds of the last received update. `0` if none yet.
    pub last_update_at: AtomicI64,
    /// Unix seconds of the last handler completion.
    pub last_response_at: AtomicI64,
    /// Wall-clock milliseconds of the last handler's full duration
    /// (rate-limit check → reply sent).
    pub last_response_duration_ms: AtomicU64,
    /// Unix seconds of the last successful getUpdates.
    pub last_poll_success_at: AtomicI64,
}

impl Metrics {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_update_received(&self) {
        self.updates_received.fetch_add(1, Ordering::Relaxed);
        self.last_update_at.store(now_secs(), Ordering::Relaxed);
    }

    pub fn record_rate_limited(&self) {
        self.rate_limited.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_handler_completed(&self, duration_ms: u64) {
        self.updates_processed.fetch_add(1, Ordering::Relaxed);
        self.last_response_at.store(now_secs(), Ordering::Relaxed);
        self.last_response_duration_ms
            .store(duration_ms, Ordering::Relaxed);
    }

    pub fn record_handler_error(&self) {
        self.handler_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_poll_success(&self) {
        self.poll_success.fetch_add(1, Ordering::Relaxed);
        self.last_poll_success_at
            .store(now_secs(), Ordering::Relaxed);
    }

    pub fn record_poll_error(&self) {
        self.poll_errors.fetch_add(1, Ordering::Relaxed);
    }
}

/// Current time as seconds since the Unix epoch. Clamps to `0` on the
/// impossible case of `SystemTime::now() < UNIX_EPOCH` (clock set before
/// 1970) so readers can treat `0` uniformly as "unknown".
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_flow_is_additive() {
        let m = Metrics::new();
        m.record_update_received();
        m.record_update_received();
        assert_eq!(m.updates_received.load(Ordering::Relaxed), 2);

        m.record_handler_completed(42);
        assert_eq!(m.updates_processed.load(Ordering::Relaxed), 1);
        assert_eq!(m.last_response_duration_ms.load(Ordering::Relaxed), 42);

        m.record_rate_limited();
        m.record_handler_error();
        m.record_poll_success();
        m.record_poll_error();
        assert_eq!(m.rate_limited.load(Ordering::Relaxed), 1);
        assert_eq!(m.handler_errors.load(Ordering::Relaxed), 1);
        assert_eq!(m.poll_success.load(Ordering::Relaxed), 1);
        assert_eq!(m.poll_errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn timestamps_default_to_zero() {
        let m = Metrics::new();
        assert_eq!(m.last_update_at.load(Ordering::Relaxed), 0);
        assert_eq!(m.last_response_at.load(Ordering::Relaxed), 0);
        assert_eq!(m.last_poll_success_at.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn now_secs_is_positive_and_recent() {
        let t = now_secs();
        // After Jan 1 2024 (any sane build environment)
        assert!(t > 1_700_000_000);
    }
}
