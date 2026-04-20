//! Shared atomic-write and JSON load/save for hook config files.
//!
//! Atomic = write to `<path>.tebis.tmp`, fsync, then `rename` over the
//! target. A crash at any point leaves either the old or new file
//! intact — never a torn JSON that would make Claude refuse the file.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

pub(super) fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tebis.tmp");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    {
        let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

pub(super) fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .with_context(|| format!("serializing {}", path.display()))?;
    bytes.push(b'\n');
    atomic_write_bytes(path, &bytes)
}

/// Load JSON, returning `Value::Object(empty)` if the file doesn't exist.
/// Errors on a present-but-malformed file so callers can refuse to
/// clobber user-authored data.
pub(super) fn load_or_empty(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(Value::Object(Map::default())),
        Ok(s) => serde_json::from_str(&s).with_context(|| {
            format!(
                "parsing {} — refusing to overwrite user JSON",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Map::default())),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tebis-jsonfile-{tag}-{}", std::process::id()))
    }

    #[test]
    fn atomic_write_creates_file() {
        let p = tmp_path("create");
        let _ = fs::remove_file(&p);
        atomic_write_bytes(&p, b"hello").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"hello");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn atomic_write_overwrites() {
        let p = tmp_path("overwrite");
        fs::write(&p, b"old").unwrap();
        atomic_write_bytes(&p, b"new").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"new");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn atomic_tempfile_is_gone_after_success() {
        let p = tmp_path("tempclean");
        atomic_write_bytes(&p, b"x").unwrap();
        assert!(!p.with_extension("tebis.tmp").exists());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn load_or_empty_returns_empty_object_when_missing() {
        let p = tmp_path("missing");
        let _ = fs::remove_file(&p);
        let v = load_or_empty(&p).unwrap();
        assert!(v.as_object().unwrap().is_empty());
    }

    #[test]
    fn load_or_empty_parses_existing() {
        let p = tmp_path("exists");
        fs::write(&p, r#"{"a": 1}"#).unwrap();
        let v = load_or_empty(&p).unwrap();
        assert_eq!(v["a"], 1);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn load_or_empty_errors_on_garbage() {
        let p = tmp_path("garbage");
        fs::write(&p, "not json").unwrap();
        assert!(load_or_empty(&p).is_err());
        let _ = fs::remove_file(&p);
    }
}
