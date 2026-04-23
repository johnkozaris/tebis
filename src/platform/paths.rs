//! Filesystem layout. One place to ask "where does tebis keep
//! config / data / cache / runtime / the env file / the lockfile".
//!
//! # Layout contract
//!
//! | Purpose         | Linux                                 | macOS                                 | Windows                           |
//! |-----------------|---------------------------------------|---------------------------------------|-----------------------------------|
//! | `config_dir`    | `$XDG_CONFIG_HOME/tebis` or `~/.config/tebis` | `~/.config/tebis`                     | `%APPDATA%\tebis`                 |
//! | `data_dir`      | `$XDG_DATA_HOME/tebis` or `~/.local/share/tebis` | `~/.local/share/tebis`                | `%LOCALAPPDATA%\tebis`            |
//! | `runtime_dir`   | `$XDG_RUNTIME_DIR/tebis`, fallback `/tmp/tebis-$USER` | `/tmp/tebis-$USER`                    | `%LOCALAPPDATA%\tebis\run`        |
//! | `env_file_path` | `<config_dir>/env`                    | `<config_dir>/env`                    | `<config_dir>\env`                |
//! | `lock_file_path`| `<runtime_dir>/tebis.lock`            | `<runtime_dir>/tebis.lock`            | `<runtime_dir>\tebis.lock`        |
//!
//! Unix paths keep the XDG-style layout tebis has always used, to match
//! the invariants and docs already referencing `~/.config/tebis` etc.
//! Windows uses the Known Folder API via the `directories` crate — the
//! standard location for per-user app config and data on Windows.

use std::path::PathBuf;

use anyhow::{Context, Result};

#[cfg(unix)]
mod unix {
    use super::{Context, PathBuf, Result};

    fn home() -> Result<PathBuf> {
        let h = std::env::var("HOME").context("HOME not set")?;
        if h.is_empty() {
            anyhow::bail!("HOME is empty");
        }
        Ok(PathBuf::from(h))
    }

    pub fn config_dir() -> Result<PathBuf> {
        if let Ok(x) = std::env::var("XDG_CONFIG_HOME")
            && !x.is_empty()
        {
            return Ok(PathBuf::from(x).join("tebis"));
        }
        Ok(home()?.join(".config/tebis"))
    }

    pub fn data_dir() -> Result<PathBuf> {
        if let Ok(x) = std::env::var("XDG_DATA_HOME")
            && !x.is_empty()
        {
            return Ok(PathBuf::from(x).join("tebis"));
        }
        Ok(home()?.join(".local/share/tebis"))
    }

    pub fn runtime_dir() -> Result<PathBuf> {
        if let Ok(x) = std::env::var("XDG_RUNTIME_DIR")
            && !x.is_empty()
        {
            return Ok(PathBuf::from(x).join("tebis"));
        }
        // macOS + BSD: no runtime dir concept; per-user tmp fallback.
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
        Ok(PathBuf::from(format!("/tmp/tebis-{user}")))
    }
}

#[cfg(windows)]
mod windows {
    use super::{Context, PathBuf, Result};

    fn dirs() -> Result<directories::ProjectDirs> {
        directories::ProjectDirs::from("", "", "tebis")
            .context("resolving per-user Known Folders")
    }

    pub fn config_dir() -> Result<PathBuf> {
        Ok(dirs()?.config_dir().to_path_buf())
    }

    pub fn data_dir() -> Result<PathBuf> {
        // `data_local_dir` → `%LOCALAPPDATA%` — per-machine, not roaming.
        // Models + lockfile + cache all belong here; roaming (`%APPDATA%`)
        // is reserved for small config.
        Ok(dirs()?.data_local_dir().to_path_buf())
    }

    pub fn runtime_dir() -> Result<PathBuf> {
        // Windows has no XDG_RUNTIME_DIR analogue. `%LOCALAPPDATA%\tebis\run`
        // is per-user and local (never syncs to a domain controller), which
        // is what we want for a lockfile + tmp socket-like state.
        Ok(dirs()?.data_local_dir().join("run"))
    }
}

#[cfg(unix)]
pub use unix::{config_dir, data_dir, runtime_dir};
#[cfg(windows)]
pub use windows::{config_dir, data_dir, runtime_dir};

/// `<config_dir>/env` — the canonical tebis env file (`KEY=VAL` pairs,
/// owner-only permissions, written through
/// [`super::secure_file::atomic_write_private`]).
pub fn env_file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("env"))
}

/// `<runtime_dir>/tebis.lock` — single-instance advisory lock path.
pub fn lock_file_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("tebis.lock"))
}

/// `<data_dir>/models` — cached model files (Whisper, future Kokoro).
pub fn models_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("models"))
}

/// `<data_dir>/installed.json` — host-wide hook manifest.
pub fn hook_manifest_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("installed.json"))
}
