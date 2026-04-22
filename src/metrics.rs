//! Lock-free counters + last-event timestamps. Timestamps are Unix seconds;
//! `0` means "never".

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct Metrics {
    pub updates_received: AtomicU64,
    pub updates_processed: AtomicU64,
    pub rate_limited: AtomicU64,
    pub handler_errors: AtomicU64,
    pub poll_success: AtomicU64,
    pub poll_errors: AtomicU64,

    pub last_update_at: AtomicI64,
    pub last_response_at: AtomicI64,
    pub last_response_duration_ms: AtomicU64,
    pub last_poll_success_at: AtomicI64,

    pub voice_received: AtomicU64,
    pub stt_success: AtomicU64,
    pub stt_failures: AtomicU64,
    pub last_stt_duration_ms: AtomicU64,

    pub tts_success: AtomicU64,
    pub tts_failures: AtomicU64,
    pub last_tts_duration_ms: AtomicU64,
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

    pub fn record_voice_received(&self) {
        self.voice_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_stt_success(&self, duration_ms: u32) {
        self.stt_success.fetch_add(1, Ordering::Relaxed);
        self.last_stt_duration_ms
            .store(u64::from(duration_ms), Ordering::Relaxed);
    }

    pub fn record_stt_failure(&self) {
        self.stt_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_tts_success(&self, duration_ms: u64) {
        self.tts_success.fetch_add(1, Ordering::Relaxed);
        self.last_tts_duration_ms
            .store(duration_ms, Ordering::Relaxed);
    }

    pub fn record_tts_failure(&self) {
        self.tts_failures.fetch_add(1, Ordering::Relaxed);
    }
}

/// Unix seconds; `0` on clock-before-epoch so readers treat `0` as "unknown".
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
        assert!(t > 1_700_000_000);
    }
}
