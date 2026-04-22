//! Duration + unix-timestamp formatting for the dashboard. `never` for
//! zero, ordinary `Ns` / `Nm Ns` / `Nh Nm Ns` / `Nd Nh Nm Ns` for
//! anything else. Used both by the HTML page and the JSON endpoint.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) fn format_duration(d: Duration) -> String {
    let mut s = d.as_secs();
    let days = s / 86_400;
    s %= 86_400;
    let hours = s / 3_600;
    s %= 3_600;
    let mins = s / 60;
    let secs = s % 60;
    let mut out = String::new();
    if days > 0 {
        let _ = write!(out, "{days}d ");
    }
    if hours > 0 || days > 0 {
        let _ = write!(out, "{hours}h ");
    }
    if mins > 0 || hours > 0 || days > 0 {
        let _ = write!(out, "{mins}m ");
    }
    let _ = write!(out, "{secs}s");
    out
}

pub(super) fn format_ago(unix_secs: i64) -> String {
    if unix_secs == 0 {
        return "never".to_string();
    }
    let now = now_unix_secs();
    let delta = now.saturating_sub(unix_secs);
    if delta <= 0 {
        return "0s ago".to_string();
    }
    format!(
        "{} ago",
        format_duration(Duration::from_secs(u64::try_from(delta).unwrap_or(0)))
    )
}

pub(super) fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_shapes() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 5s");
        assert_eq!(format_duration(Duration::from_secs(3_661)), "1h 1m 1s");
        assert_eq!(format_duration(Duration::from_secs(86_461)), "1d 0h 1m 1s");
    }

    #[test]
    fn format_ago_never_for_zero() {
        assert_eq!(format_ago(0), "never");
    }
}
