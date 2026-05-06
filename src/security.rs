//! Auth gate. Drops messages from anyone other than the configured user_id.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::telegram::types::Update;

/// Per-sender unauthorized-log cooldown so DM-spam can't fill the journal.
const UNAUTHORIZED_LOG_COOLDOWN: Duration = Duration::from_secs(300);

/// Authorize by numeric `user_id` only — usernames are mutable
/// (CVE-2026-28480).
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

#[cfg(test)]
mod tests {
    use super::*;

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
