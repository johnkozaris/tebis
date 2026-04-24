//! Host-wide manifest at `$XDG_DATA_HOME/tebis/installed.json`. Auxiliary state —
//! read/write failures log warn but never block install/uninstall.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{AgentKind, jsonfile};

const MANIFEST_FILE: &str = "installed.json";
const MANIFEST_LOCK_FILE: &str = "installed.json.lock";

/// `dir` is canonicalized absolute; `installed_at` is RFC 3339 UTC.
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

/// RAII exclusive-lock guard — serializes concurrent `record_install`
/// read-modify-writes. `File::lock` is `flock(2)` on Unix and
/// `LockFileEx` on Windows (std since 1.89). The OS releases the lock
/// on file close, so dropping the `File` is sufficient — do **not**
/// `remove_file` the lock, since another waiter's `open` would race it.
struct ManifestLock {
    _file: std::fs::File,
}

impl ManifestLock {
    fn acquire() -> Result<Self> {
        let dir = super::data_dir()?;
        crate::platform::secure_file::ensure_private_dir(&dir)?;
        let path = manifest_lock_path()?;
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path)?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}

/// Empty on any error; logs warn.
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

/// Record install of `(agent, canon(dir))`. Replaces any prior entry for the pair.
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

/// Drop `(agent, dir)`. Silent when absent — idempotent.
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

/// Drop entries whose `dir` no longer exists; returns dropped so callers can report.
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

/// RFC 3339 UTC to second precision — no chrono / time dep.
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_rfc3339_utc(secs)
}

/// Epoch seconds → `YYYY-MM-DDThh:mm:ssZ`. Extracted so tests can pin input.
#[expect(
    clippy::cast_possible_wrap,
    reason = "days-since-epoch fits in i64 for any realistic install timestamp."
)]
fn format_rfc3339_utc(secs: u64) -> String {
    let (days, sod) = (secs / 86_400, secs % 86_400);
    let (hh, mm, ss) = (sod / 3600, (sod / 60) % 60, sod % 60);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Howard Hinnant's `civil_from_days` — public-domain, avoids chrono/time.
/// <https://howardhinnant.github.io/date_algorithms.html>
#[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    reason = "Hinnant's algorithm uses bounded year arithmetic that fits in the target types."
)]
const fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
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
        assert_eq!(format_rfc3339_utc(1_776_643_200), "2026-04-20T00:00:00Z");
    }

    #[test]
    fn format_rfc3339_leap_year_boundary() {
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
