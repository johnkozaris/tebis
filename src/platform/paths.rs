//! Filesystem layout. One place to ask "where does tebis keep
//! config / data / cache / runtime / the env file / the lockfile /
//! the notify socket".
//!
//! # Layout contract
//!
//! | Purpose             | Linux                                            | macOS                           | Windows                                  |
//! |---------------------|--------------------------------------------------|---------------------------------|------------------------------------------|
//! | `config_dir`        | `$XDG_CONFIG_HOME/tebis` or `~/.config/tebis`    | `~/.config/tebis`               | `%APPDATA%\tebis`                        |
//! | `data_dir`          | `$XDG_DATA_HOME/tebis` or `~/.local/share/tebis` | `~/.local/share/tebis`          | `%LOCALAPPDATA%\tebis`                   |
//! | `env_file_path`     | `<config_dir>/env`                               | `<config_dir>/env`              | `<config_dir>\env`                       |
//! | `lock_file_path`    | `$XDG_RUNTIME_DIR/tebis.lock` or `/tmp/tebis-$USER.lock` | `/tmp/tebis-$USER.lock` | `%LOCALAPPDATA%\tebis\run\tebis.lock`    |
//! | `notify_address`    | `$XDG_RUNTIME_DIR/tebis.sock` or `/tmp/tebis-$USER.sock` | `/tmp/tebis-$USER.sock` | `\\.\pipe\tebis-<user>-notify` (pipe name) |
//! | `models_dir`        | `<data_dir>/models`                              | `<data_dir>/models`             | `<data_dir>\models`                      |
//! | `hook_manifest_path`| `<data_dir>/installed.json`                      | `<data_dir>/installed.json`     | `<data_dir>\installed.json`              |
//!
//! Unix paths preserve the flat layout tebis has always shipped —
//! `$XDG_RUNTIME_DIR/tebis.sock` / `/tmp/tebis-$USER.sock`, not
//! `$XDG_RUNTIME_DIR/tebis/tebis.sock`. That's what the embedded hook
//! scripts (`contrib/claude/claude-hook.sh`, `contrib/copilot/…`)
//! compute, and any user with an existing deployment expects.
//!
//! Windows uses the Known Folder API via `directories` for config /
//! data, and a synthetic `run` subdirectory under `%LOCALAPPDATA%`
//! for the lockfile (no NT analogue of `XDG_RUNTIME_DIR`). The
//! notify address on Windows is a **named pipe name**, not a
//! filesystem path — carried as `PathBuf` for shape compatibility
//! with the Unix UDS path.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Test-only path override. When `TEBIS_SCRATCH_DIR` is set,
/// `config_dir()` returns `<scratch>/config` and `data_dir()` returns
/// `<scratch>/data`, on every platform — including Windows, where
/// `XDG_*` env vars don't reach the Known Folder API. This lets
/// `agent_hooks::test_support::with_scratch_data_home` work uniformly
/// across targets.
///
/// In release builds the function always returns `None`; the env var
/// is never consulted, so prod behavior is unaffected.
#[cfg(test)]
fn test_override() -> Option<PathBuf> {
    std::env::var("TEBIS_SCRATCH_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

#[cfg(not(test))]
const fn test_override() -> Option<PathBuf> {
    None
}

#[cfg(unix)]
mod unix {
    use super::{Context, PathBuf, Result, test_override};

    fn home() -> Result<PathBuf> {
        let h = std::env::var("HOME").context("HOME not set")?;
        if h.is_empty() {
            anyhow::bail!("HOME is empty");
        }
        Ok(PathBuf::from(h))
    }

    pub fn config_dir() -> Result<PathBuf> {
        if let Some(root) = test_override() {
            return Ok(root.join("config"));
        }
        if let Ok(x) = std::env::var("XDG_CONFIG_HOME")
            && !x.is_empty()
        {
            return Ok(PathBuf::from(x).join("tebis"));
        }
        Ok(home()?.join(".config/tebis"))
    }

    pub fn data_dir() -> Result<PathBuf> {
        if let Some(root) = test_override() {
            return Ok(root.join("data"));
        }
        if let Ok(x) = std::env::var("XDG_DATA_HOME")
            && !x.is_empty()
        {
            return Ok(PathBuf::from(x).join("tebis"));
        }
        Ok(home()?.join(".local/share/tebis"))
    }

    pub fn lock_file_path() -> Result<PathBuf> {
        if let Ok(x) = std::env::var("XDG_RUNTIME_DIR")
            && !x.is_empty()
        {
            return Ok(PathBuf::from(x).join("tebis.lock"));
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
        Ok(PathBuf::from(format!("/tmp/tebis-{user}.lock")))
    }

    pub fn notify_address() -> Result<PathBuf> {
        if let Ok(x) = std::env::var("XDG_RUNTIME_DIR")
            && !x.is_empty()
        {
            return Ok(PathBuf::from(x).join("tebis.sock"));
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
        Ok(PathBuf::from(format!("/tmp/tebis-{user}.sock")))
    }
}

#[cfg(windows)]
mod windows {
    use super::{Context, PathBuf, Result, test_override};

    fn dirs() -> Result<directories::ProjectDirs> {
        directories::ProjectDirs::from("", "", "tebis")
            .context("resolving per-user Known Folders")
    }

    pub fn config_dir() -> Result<PathBuf> {
        if let Some(root) = test_override() {
            return Ok(root.join("config"));
        }
        Ok(dirs()?.config_dir().to_path_buf())
    }

    pub fn data_dir() -> Result<PathBuf> {
        // `data_local_dir` → `%LOCALAPPDATA%` — per-machine, not roaming.
        // Models + lockfile + cache all belong here; roaming (`%APPDATA%`)
        // is reserved for small config.
        if let Some(root) = test_override() {
            return Ok(root.join("data"));
        }
        Ok(dirs()?.data_local_dir().to_path_buf())
    }

    pub fn lock_file_path() -> Result<PathBuf> {
        // No XDG_RUNTIME_DIR analogue; synthesize a `run` subdir under
        // `%LOCALAPPDATA%\tebis\` for ephemeral runtime state.
        Ok(dirs()?.data_local_dir().join("run").join("tebis.lock"))
    }

    pub fn notify_address() -> Result<PathBuf> {
        // Named pipe — not a filesystem path. The listener interprets
        // this as the pipe name for `CreateNamedPipeW`.
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        Ok(PathBuf::from(format!(r"\\.\pipe\tebis-{user}-notify")))
    }
}

#[cfg(unix)]
pub use unix::{config_dir, data_dir, lock_file_path, notify_address};
#[cfg(windows)]
pub use windows::{config_dir, data_dir, lock_file_path, notify_address};

/// `<config_dir>/env` — the canonical tebis env file (`KEY=VAL` pairs,
/// owner-only permissions, written through
/// [`super::secure_file::atomic_write_private`]).
pub fn env_file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("env"))
}

/// `<data_dir>/models` — cached model files (Whisper, future Kokoro).
pub fn models_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("models"))
}

/// `<data_dir>/installed.json` — host-wide hook manifest.
pub fn hook_manifest_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("installed.json"))
}

/// Cross-platform home-dir lookup — honors `HOME` first (POSIX shells,
/// including Git Bash on Windows), falls back to `USERPROFILE` on
/// Windows. Used for `~` expansion in user-typed paths (autostart
/// dir, env-file path prompts).
pub fn home_dir() -> Result<PathBuf> {
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        return Ok(PathBuf::from(h));
    }
    if let Ok(h) = std::env::var("USERPROFILE")
        && !h.is_empty()
    {
        return Ok(PathBuf::from(h));
    }
    anyhow::bail!("neither HOME nor USERPROFILE is set")
}
