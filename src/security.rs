//! Auth + per-chat rate limiting.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::telegram::types::Update;

/// Per-sender unauthorized-log cooldown so DM-spam can't fill the journal.
const UNAUTHORIZED_LOG_COOLDOWN: Duration = Duration::from_secs(300);

/// Invariant 1: authorize by numeric `user_id` only, never username
/// (usernames are mutable — CVE-2026-28480).
pub fn is_authorized(update: &Update, allowed_user_id: i64) -> bool {
    let Some(message) = &update.message else {
        return false;
    };
    let Some(user) = &message.from else {
        return false;
    };
    if user.id != allowed_user_id {
        if should_log_unauthorized(user.id) {
            tracing::warn!(
                unauthorized_user_id = user.id,
                username = ?user.username,
                "Unauthorized message — silently dropping (further rejections from this id suppressed for 5 min)"
            );
        }
        return false;
    }
    true
}

/// Soft cap so an id-rotating attacker can't grow the map unbounded.
const SEEN_SOFT_CAP: usize = 1024;

fn should_log_unauthorized(user_id: i64) -> bool {
    static SEEN: OnceLock<Mutex<HashMap<i64, Instant>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut guard) = seen.lock() else {
        return true;
    };
    let now = Instant::now();
    if guard.len() >= SEEN_SOFT_CAP {
        guard.retain(|_, last| now.duration_since(*last) < UNAUTHORIZED_LOG_COOLDOWN);
    }
    match guard.get(&user_id) {
        Some(last) if now.duration_since(*last) < UNAUTHORIZED_LOG_COOLDOWN => false,
        _ => {
            guard.insert(user_id, now);
            true
        }
    }
}

/// GCRA rate limiter. `per_minute` is the refill rate; `burst` the
/// back-to-back capacity.
pub struct RateLimiter {
    tats: Mutex<HashMap<i64, Instant>>,
    emission_interval: Duration,
    tau: Duration,
}

impl RateLimiter {
    pub fn new(per_minute: u32, burst: u32) -> Self {
        assert!(per_minute > 0, "rate must be positive");
        assert!(burst > 0, "burst must be positive");
        let emission = Duration::from_mins(1) / per_minute;
        let tau = emission
            .checked_mul(burst)
            .expect("rate-limiter tau overflowed — per_minute/burst combination too large");
        Self {
            tats: Mutex::new(HashMap::new()),
            emission_interval: emission,
            tau,
        }
    }

    /// `Ok(())` if admitted; `Err(retry_after)` with the wait until the next call would pass.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "guard protects the read-modify-write of TAT; releasing earlier races concurrent calls"
    )]
    pub fn check(&self, chat_id: i64) -> std::result::Result<(), Duration> {
        let now = Instant::now();
        let mut tats = self.tats.lock().expect("rate limiter poisoned");
        let tat = tats.entry(chat_id).or_insert(now);
        let new_tat = (*tat).max(now) + self.emission_interval;
        let delay = new_tat.saturating_duration_since(now);
        if delay > self.tau {
            // Clip retry-after to 30s so a drifted TAT can't report "wait hours".
            let retry_after = delay.saturating_sub(self.tau).min(Duration::from_secs(30));
            return Err(retry_after);
        }
        *tat = new_tat;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_allows_burst_then_throttles() {
        let limiter = RateLimiter::new(60, 5);
        for _ in 0..5 {
            assert!(limiter.check(1).is_ok());
        }
        assert!(limiter.check(1).is_err());
    }

    #[test]
    fn rate_limiter_separates_chats() {
        let limiter = RateLimiter::new(60, 2);
        assert!(limiter.check(1).is_ok());
        assert!(limiter.check(1).is_ok());
        assert!(limiter.check(1).is_err());
        assert!(limiter.check(2).is_ok());
        assert!(limiter.check(2).is_ok());
    }

    #[test]
    fn rate_limiter_refills_over_time() {
        let limiter = RateLimiter::new(6000, 1);
        assert!(limiter.check(1).is_ok());
        std::thread::sleep(Duration::from_millis(15));
        assert!(limiter.check(1).is_ok());
    }

    #[test]
    fn rate_limiter_returns_retry_after_when_throttled() {
        let limiter = RateLimiter::new(60, 2);
        assert!(limiter.check(7).is_ok());
        assert!(limiter.check(7).is_ok());
        let err = limiter.check(7).unwrap_err();
        assert!(err.as_millis() > 0);
        assert!(err <= Duration::from_secs(30));
    }

    #[test]
    fn unauthorized_log_cooldown_suppresses_repeats() {
        // Distinct ids avoid races with other tests sharing the module-global SEEN map.
        let attacker = 999_999_001;
        let other = 999_999_002;
        assert!(should_log_unauthorized(attacker), "first must log");
        assert!(
            !should_log_unauthorized(attacker),
            "second within cooldown must suppress"
        );
        assert!(
            !should_log_unauthorized(attacker),
            "third within cooldown must suppress"
        );
        assert!(
            should_log_unauthorized(other),
            "different sender is independent"
        );
    }
}
