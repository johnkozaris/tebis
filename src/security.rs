//! Access-control primitives: Telegram-side authorization and per-chat
//! rate limiting. Both guard the inbound-message path before any tmux
//! work happens — the rate limit is cheap, the auth check is free.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::telegram::types::Update;

/// Per-sender cooldown so a determined attacker DM'ing the bot can't
/// fill the journal with "Unauthorized message — …" warns. First
/// rejected message from each sender logs; subsequent rejections
/// within this window are counted silently.
const UNAUTHORIZED_LOG_COOLDOWN: Duration = Duration::from_secs(300);

/// Authorize by Telegram numeric `user_id`. Never authenticate by username —
/// usernames are recyclable and mutable, making them unsafe for access control.
/// CVE-2026-28480 is the canonical example of this flaw.
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

/// Returns true if we haven't logged a rejection from `user_id` within
/// [`UNAUTHORIZED_LOG_COOLDOWN`]. Map is bounded by real-world
/// attacker count (one entry per probing user id); no eviction needed
/// because entries stay small and the table self-prunes on restart.
fn should_log_unauthorized(user_id: i64) -> bool {
    static SEEN: OnceLock<Mutex<HashMap<i64, Instant>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut guard) = seen.lock() else {
        // Poisoned → log anyway; poisoning is rare and silence here
        // would hide real issues.
        return true;
    };
    let now = Instant::now();
    match guard.get(&user_id) {
        Some(last) if now.duration_since(*last) < UNAUTHORIZED_LOG_COOLDOWN => false,
        _ => {
            guard.insert(user_id, now);
            true
        }
    }
}

/// Per-chat rate limiter using GCRA (Generic Cell Rate Algorithm) — the
/// same algorithm governor uses. O(1) check, fixed state per key.
///
/// `rate` is requests per minute, `burst` is how many can fire back-to-back
/// before throttling kicks in.
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
        // `emission * burst` is tau. Overflow is a configuration bug
        // (e.g. per_minute=1, burst=u32::MAX) — fail loudly at
        // construction rather than at the first `.check()` call.
        let tau = emission
            .checked_mul(burst)
            .expect("rate-limiter tau overflowed — per_minute/burst combination too large");
        Self {
            tats: Mutex::new(HashMap::new()),
            emission_interval: emission,
            tau,
        }
    }

    /// GCRA admission check.
    ///
    /// Returns `Ok(())` if the request is within the burst+rate budget, or
    /// `Err(retry_after)` with the exact duration until the next call would
    /// be accepted. Clients surface this wait to the user so "try again
    /// later" isn't a blind guess.
    #[allow(clippy::significant_drop_tightening)]
    pub fn check(&self, chat_id: i64) -> std::result::Result<(), Duration> {
        let now = Instant::now();
        let mut tats = self.tats.lock().expect("rate limiter poisoned");
        let tat = tats.entry(chat_id).or_insert(now);
        let new_tat = (*tat).max(now) + self.emission_interval;
        let delay = new_tat.saturating_duration_since(now);
        if delay > self.tau {
            // Time until we'd re-enter the allowed window. Clipped to a
            // reasonable upper bound (30 s) so a drifted TAT can't hand
            // the user a "wait 12 hours" message. `saturating_sub` is
            // paranoia — the `delay > self.tau` branch guarantees the
            // subtraction is positive.
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
        // Different chat_id has its own bucket
        assert!(limiter.check(2).is_ok());
        assert!(limiter.check(2).is_ok());
    }

    #[test]
    fn rate_limiter_refills_over_time() {
        let limiter = RateLimiter::new(6000, 1); // 100/sec, emission 10ms
        assert!(limiter.check(1).is_ok());
        std::thread::sleep(Duration::from_millis(15));
        assert!(limiter.check(1).is_ok());
    }

    #[test]
    fn rate_limiter_returns_retry_after_when_throttled() {
        // 60/min, burst 2 → emission = 1s, tau = 2s. Burst, then throttle,
        // retry_after should be about one emission interval.
        let limiter = RateLimiter::new(60, 2);
        assert!(limiter.check(7).is_ok());
        assert!(limiter.check(7).is_ok());
        let err = limiter.check(7).unwrap_err();
        // Sanity bounds — should be under the 30s cap and > 0.
        assert!(err.as_millis() > 0);
        assert!(err <= Duration::from_secs(30));
    }

    #[test]
    fn unauthorized_log_cooldown_suppresses_repeats() {
        // Distinct user ids so we don't race with other tests sharing the
        // module-global SEEN map. First hit logs; back-to-back repeats
        // within the cooldown window stay silent. Different id is
        // independent.
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
