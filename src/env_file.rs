//! Env-file I/O: atomic 0600 write + `KEY=VAL` parse + toggle parser.

use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Atomic write with mode 0600: tmp file opened 0600 (no umask window), fsync,
/// rename, fsync parent.
pub fn atomic_write_0600(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("env")
    ));

    // `mode(0o600)` on open bypasses umask. chmod after write guards
    // against ACL layers that lose creation mode.
    {
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("opening {}", tmp.display()))?;
        f.write_all(content.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
    // POSIX: rename durability needs fsync on the containing dir.
    // Best-effort — NFS/tmpfs may reject dir-fsync.
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
        && let Err(e) = dir.sync_all()
    {
        tracing::debug!(err = %e, dir = %parent.display(), "atomic_write_0600: parent dir fsync failed");
    }
    Ok(())
}

/// `KEY=VALUE`. No shell expansion; matches systemd's `EnvironmentFile=`.
pub fn parse_kv_line(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    let (key, value) = trimmed.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    let value = value.trim();
    let value = strip_matched_quotes(value);
    Some((key, value))
}

fn strip_matched_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// On/off synonym parser. Empty → `None` (use default); unknown → error
/// so typos fail loudly.
pub fn parse_toggle(value: &str) -> Result<Option<bool>> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "on" | "auto" | "true" | "yes" | "1" | "enable" | "enabled" => Ok(Some(true)),
        "off" | "false" | "no" | "0" | "disable" | "disabled" => Ok(Some(false)),
        other => bail!(
            "unrecognized toggle value {other:?} — use on|off \
             (synonyms: auto, true/false, yes/no, 1/0, enable/disable)"
        ),
    }
}

/// Upsert `KEY=value` pairs in `path`, preserving comments + line order.
/// Missing file is treated as empty. Keys not already present are appended.
/// Atomic 0600 write via [`atomic_write_0600`].
pub fn upsert_keys(path: &Path, updates: &[(&str, String)]) -> Result<()> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = current.lines().map(str::to_string).collect();

    for (key, value) in updates {
        let replaced = lines
            .iter_mut()
            .find(|line| parse_kv_line(line).is_some_and(|(k, _)| k == *key));
        if let Some(line) = replaced {
            *line = format!("{key}={value}");
        } else {
            lines.push(format!("{key}={value}"));
        }
    }

    let mut body = lines.join("\n");
    body.push('\n');
    atomic_write_0600(path, &body)
}

/// Remove `keys` from `path`. Silently succeeds if a key (or the file)
/// isn't present. Atomic 0600 write via [`atomic_write_0600`].
pub fn remove_keys(path: &Path, keys: &[&str]) -> Result<()> {
    use std::collections::HashSet;
    let current = std::fs::read_to_string(path).unwrap_or_default();
    let drop: HashSet<&str> = keys.iter().copied().collect();
    let kept: Vec<String> = current
        .lines()
        .filter(|line| match parse_kv_line(line) {
            Some((k, _)) => !drop.contains(k),
            None => true,
        })
        .map(str::to_string)
        .collect();
    let mut body = kept.join("\n");
    body.push('\n');
    atomic_write_0600(path, &body)
}

/// First occurrence of `key`, or `None` if file/key is missing.
pub fn read_key(path: &Path, key: &str) -> io::Result<Option<String>> {
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    for line in content.lines() {
        if let Some((k, v)) = parse_kv_line(line)
            && k == key
        {
            return Ok(Some(v.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kv_line_basic() {
        assert_eq!(parse_kv_line("FOO=bar"), Some(("FOO", "bar")));
        assert_eq!(parse_kv_line("  FOO=bar  "), Some(("FOO", "bar")));
    }

    #[test]
    fn parse_kv_line_skips_blanks_and_comments() {
        assert_eq!(parse_kv_line(""), None);
        assert_eq!(parse_kv_line("   "), None);
        assert_eq!(parse_kv_line("# a comment"), None);
        assert_eq!(parse_kv_line("   # indented"), None);
    }

    #[test]
    fn parse_kv_line_strips_export_prefix() {
        assert_eq!(parse_kv_line("export FOO=bar"), Some(("FOO", "bar")));
    }

    #[test]
    fn parse_kv_line_strips_matched_quotes() {
        assert_eq!(parse_kv_line(r#"FOO="bar baz""#), Some(("FOO", "bar baz")));
        assert_eq!(parse_kv_line("FOO='bar baz'"), Some(("FOO", "bar baz")));
    }

    #[test]
    fn parse_kv_line_leaves_mismatched_quotes() {
        assert_eq!(parse_kv_line(r#"FOO="bar"#), Some(("FOO", r#""bar"#)));
        assert_eq!(parse_kv_line(r#"FOO='bar""#), Some(("FOO", r#"'bar""#)));
    }

    #[test]
    fn parse_kv_line_rejects_no_equals() {
        assert_eq!(parse_kv_line("FOO"), None);
    }

    #[test]
    fn parse_kv_line_accepts_empty_value() {
        assert_eq!(parse_kv_line("FOO="), Some(("FOO", "")));
    }

    #[test]
    fn atomic_write_0600_creates_with_mode() {
        let p = std::env::temp_dir().join(format!("tebis-env-0600-{}", std::process::id()));
        let _ = fs::remove_file(&p);
        atomic_write_0600(&p, "FOO=bar\n").unwrap();
        let meta = fs::metadata(&p).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&p).unwrap(), "FOO=bar\n");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn atomic_write_0600_overwrite_tightens_perms() {
        let p = std::env::temp_dir().join(format!("tebis-env-0600-tight-{}", std::process::id()));
        fs::write(&p, "old").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write_0600(&p, "new\n").unwrap();
        let meta = fs::metadata(&p).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&p).unwrap(), "new\n");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn remove_keys_drops_matching_lines_and_preserves_others() {
        let p = std::env::temp_dir().join(format!("tebis-env-rm-{}", std::process::id()));
        fs::write(
            &p,
            "# preamble comment\nFOO=bar\nBAZ=  qux  \n\n# trailing\nORT_DYLIB_PATH=/x\n",
        )
        .unwrap();
        remove_keys(&p, &["ORT_DYLIB_PATH", "MISSING"]).unwrap();
        let body = fs::read_to_string(&p).unwrap();
        assert!(!body.contains("ORT_DYLIB_PATH"), "key must be gone: {body:?}");
        assert!(body.contains("FOO=bar"), "other key must survive: {body:?}");
        assert!(body.contains("BAZ="), "other key must survive: {body:?}");
        assert!(body.contains("# preamble comment"), "comments preserved");
        assert!(body.contains("# trailing"), "comments preserved");
        // Mode 0600 guaranteed by atomic_write_0600.
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn remove_keys_no_op_when_file_missing() {
        let p = std::env::temp_dir().join("tebis-env-rm-missing-nonexistent-xyz");
        let _ = fs::remove_file(&p);
        // Must succeed + create an empty-ish file.
        remove_keys(&p, &["WHATEVER"]).unwrap();
        assert!(p.exists());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn read_key_finds_and_trims() {
        let p = std::env::temp_dir().join(format!("tebis-env-read-{}", std::process::id()));
        fs::write(&p, "FOO=bar\n# comment\nBAZ=  qux  \n").unwrap();
        assert_eq!(read_key(&p, "FOO").unwrap().as_deref(), Some("bar"));
        assert_eq!(read_key(&p, "BAZ").unwrap().as_deref(), Some("qux"));
        assert_eq!(read_key(&p, "MISSING").unwrap(), None);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn read_key_returns_none_when_file_missing() {
        let p = std::env::temp_dir().join("tebis-env-does-not-exist-xyz-missing");
        let _ = fs::remove_file(&p);
        assert!(read_key(&p, "FOO").unwrap().is_none());
    }

    #[test]
    fn parse_toggle_accepts_all_documented_synonyms() {
        for on in [
            "on", "auto", "true", "yes", "1", "enable", "enabled", "ON", "Auto", "TRUE",
        ] {
            assert_eq!(parse_toggle(on).unwrap(), Some(true), "`{on}` should be on");
        }
        for off in ["off", "false", "no", "0", "disable", "disabled", "OFF"] {
            assert_eq!(
                parse_toggle(off).unwrap(),
                Some(false),
                "`{off}` should be off"
            );
        }
    }

    #[test]
    fn parse_toggle_empty_returns_none() {
        assert_eq!(parse_toggle("").unwrap(), None);
        assert_eq!(parse_toggle("   ").unwrap(), None);
    }

    #[test]
    fn parse_toggle_rejects_unknown_values() {
        assert!(parse_toggle("maybe").is_err());
        assert!(parse_toggle("sometimes").is_err());
        assert!(parse_toggle("2").is_err());
    }
}
