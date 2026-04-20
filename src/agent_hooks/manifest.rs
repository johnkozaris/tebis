//! Host-wide manifest of project dirs where tebis has installed hooks.
//!
//! Without this, users who install hooks across many projects have no
//! way to enumerate them for cleanup on upgrade / uninstall. The
//! manifest lives at `$XDG_DATA_HOME/tebis/installed.json` — next to
//! the materialized hook scripts, same write-path pattern.
//!
//! Schema (v1):
//! ```jsonc
//! {
//!   "version": 1,
//!   "entries": [
//!     {
//!       "agent": "claude",
//!       "dir": "/abs/path/to/project",
//!       "installed_at": "2026-04-20T14:33:01Z"
//!     }
//!   ]
//! }
//! ```
//!
//! Fail-open: an unreadable manifest never blocks an install/uninstall
//! because it's auxiliary state. A read error logs warn and returns an
//! empty manifest; a write error logs warn and the install still
//! succeeds (the user loses the bookkeeping, not the hooks).

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{AgentKind, jsonfile};

const MANIFEST_FILE: &str = "installed.json";
const MANIFEST_LOCK_FILE: &str = "installed.json.lock";

/// One installed-hooks record. `dir` is an absolute, canonicalized
/// path; `installed_at` uses RFC 3339 in UTC so sorting is trivial.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub agent: String,
    pub dir: PathBuf,
    pub installed_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ManifestFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    entries: Vec<Entry>,
}

const fn default_version() -> u32 {
    1
}

fn manifest_path() -> Result<PathBuf> {
    Ok(super::data_dir()?.join(MANIFEST_FILE))
}

fn manifest_lock_path() -> Result<PathBuf> {
    Ok(super::data_dir()?.join(MANIFEST_LOCK_FILE))
}

/// Hold a blocking exclusive `flock(2)` on the manifest for the RAII
/// lifetime of this guard. Serializes concurrent `tebis hooks install`
/// / autostart installs so the read-modify-write in `record_install`
/// can't lose an entry.
///
/// Blocking (not `LOCK_NB`) — concurrent installs are brief and it's
/// better to wait a few ms than to silently drop bookkeeping.
struct ManifestLock {
    _file: std::fs::File,
}

impl ManifestLock {
    fn acquire() -> Result<Self> {
        let dir = super::data_dir()?;
        std::fs::create_dir_all(&dir)?;
        let path = manifest_lock_path()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;
        // SAFETY: flock(2) with a valid fd + flags is sound.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self { _file: file })
    }
}

// Kernel releases flock on fd close. We intentionally do NOT remove
// the lock file on drop — another installer may be waiting on it, and
// `remove_file` races their open.

/// Read the manifest. On any error (missing, malformed, unreadable)
/// returns an empty list and logs — this is auxiliary state.
pub fn load_entries() -> Vec<Entry> {
    let path = match manifest_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(err = %e, "manifest: data_dir unavailable; treating as empty");
            return Vec::new();
        }
    };
    let value = match jsonfile::load_or_empty(&path) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(err = %e, "manifest: read failed; treating as empty");
            return Vec::new();
        }
    };
    if value.as_object().is_some_and(serde_json::Map::is_empty) {
        return Vec::new();
    }
    match serde_json::from_value::<ManifestFile>(value) {
        Ok(m) => m.entries,
        Err(e) => {
            tracing::warn!(err = %e, "manifest: parse failed; treating as empty");
            Vec::new()
        }
    }
}

/// Record that `agent` hooks are now installed in `dir`. Canonicalizes
/// `dir` for a stable key (so `/tmp/foo/` and `/tmp/foo` don't create
/// two entries). Replaces any prior entry for the same (agent, dir).
///
/// Best-effort: write failures log warn and return `Ok(())` — the
/// install itself already succeeded; losing manifest bookkeeping is
/// preferable to rolling back a working hook.
pub fn record_install(agent: AgentKind, dir: &Path) -> Result<()> {
    let _lock = ManifestLock::acquire()?;
    let canon = canonicalize_or_keep(dir);
    let agent_name = agent_key(agent);
    let now = now_rfc3339();

    let mut entries = load_entries();
    entries.retain(|e| !(e.agent == agent_name && e.dir == canon));
    entries.push(Entry {
        agent: agent_name.to_string(),
        dir: canon,
        installed_at: now,
    });
    entries.sort_by(|a, b| a.dir.cmp(&b.dir).then(a.agent.cmp(&b.agent)));

    save_entries(&entries);
    Ok(())
}

/// Drop the (agent, dir) record, if present. Silent when absent —
/// `uninstall` is idempotent.
pub fn record_uninstall(agent: AgentKind, dir: &Path) -> Result<()> {
    let _lock = ManifestLock::acquire()?;
    let canon = canonicalize_or_keep(dir);
    let agent_name = agent_key(agent);

    let mut entries = load_entries();
    let before = entries.len();
    entries.retain(|e| !(e.agent == agent_name && e.dir == canon));
    if entries.len() == before {
        return Ok(());
    }
    save_entries(&entries);
    Ok(())
}

/// Remove entries whose `dir` no longer exists. Returns the list of
/// removed entries so callers can report what they pruned. Blocking
/// lock — same serialization story as install/uninstall.
pub fn prune_missing_dirs() -> Result<Vec<Entry>> {
    let _lock = ManifestLock::acquire()?;
    let entries = load_entries();
    let (keep, dropped): (Vec<_>, Vec<_>) = entries.into_iter().partition(|e| e.dir.exists());
    if dropped.is_empty() {
        return Ok(Vec::new());
    }
    save_entries(&keep);
    Ok(dropped)
}

fn save_entries(entries: &[Entry]) {
    let path = match manifest_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(err = %e, "manifest: data_dir unavailable; skipping write");
            return;
        }
    };
    let doc = ManifestFile {
        version: 1,
        entries: entries.to_vec(),
    };
    let value = match serde_json::to_value(&doc) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(err = %e, "manifest: serialize failed; skipping write");
            return;
        }
    };
    if let Err(e) = jsonfile::atomic_write_json(&path, &value) {
        tracing::warn!(err = %e, "manifest: write failed; install state unrecorded");
    }
}

fn canonicalize_or_keep(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

const fn agent_key(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => "claude",
        AgentKind::Copilot => "copilot",
    }
}

/// RFC 3339 timestamp in UTC without a third-party crate — just the
/// seconds-since-epoch formatted as `YYYY-MM-DDThh:mm:ssZ`. Good
/// enough for sort + display. No sub-second precision (we don't need
/// it to order installs that happen within the same second).
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_rfc3339_utc(secs)
}

/// Format seconds-since-UNIX-epoch as an RFC 3339 UTC string.
/// Extracted so tests can pin the input.
#[expect(
    clippy::cast_possible_wrap,
    reason = "days-since-epoch fits in i64 for any realistic install timestamp."
)]
fn format_rfc3339_utc(secs: u64) -> String {
    // Days since 1970-01-01 and seconds-of-day.
    let (days, sod) = (secs / 86_400, secs % 86_400);
    let (hh, mm, ss) = (sod / 3600, (sod / 60) % 60, sod % 60);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Days-since-1970 → (year, month, day) using Howard Hinnant's
/// `civil_from_days` algorithm. Adapted from
/// <https://howardhinnant.github.io/date_algorithms.html> — well-known
/// public-domain, avoids pulling `chrono` / `time` for one timestamp.
#[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    reason = "Hinnant's algorithm uses bounded year arithmetic that fits in the target types."
)]
const fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_hooks::test_support::with_scratch_data_home;

    #[test]
    fn format_rfc3339_epoch() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_rfc3339_known_point() {
        // 2026-04-20 00:00:00 UTC — verified in Python:
        // `calendar.timegm(datetime(2026,4,20).timetuple()) == 1_776_643_200`.
        assert_eq!(format_rfc3339_utc(1_776_643_200), "2026-04-20T00:00:00Z");
    }

    #[test]
    fn format_rfc3339_leap_year_boundary() {
        // 2024-02-29 23:59:59 UTC — leap-day boundary flexes the
        // civil_from_days path.
        assert_eq!(format_rfc3339_utc(1_709_251_199), "2024-02-29T23:59:59Z");
    }

    #[test]
    fn manifest_round_trips_install_then_uninstall() {
        with_scratch_data_home("manifest_install_uninstall", || {
            assert!(load_entries().is_empty());
            let dir = std::env::temp_dir();
            record_install(AgentKind::Claude, &dir).unwrap();
            let entries = load_entries();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].agent, "claude");

            record_uninstall(AgentKind::Claude, &dir).unwrap();
            assert!(load_entries().is_empty());
        });
    }

    #[test]
    fn manifest_replaces_duplicate_agent_dir() {
        with_scratch_data_home("manifest_dedup", || {
            let dir = std::env::temp_dir();
            record_install(AgentKind::Claude, &dir).unwrap();
            record_install(AgentKind::Claude, &dir).unwrap();
            assert_eq!(load_entries().len(), 1);
        });
    }

    #[test]
    fn manifest_keeps_distinct_agents_per_dir() {
        with_scratch_data_home("manifest_multi", || {
            let dir = std::env::temp_dir();
            record_install(AgentKind::Claude, &dir).unwrap();
            record_install(AgentKind::Copilot, &dir).unwrap();
            assert_eq!(load_entries().len(), 2);
        });
    }

    #[test]
    fn manifest_uninstall_missing_is_noop() {
        with_scratch_data_home("manifest_missing", || {
            let dir = std::env::temp_dir();
            record_uninstall(AgentKind::Claude, &dir).unwrap();
            assert!(load_entries().is_empty());
        });
    }
}
