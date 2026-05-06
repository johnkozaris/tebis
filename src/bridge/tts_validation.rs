//! Validate TTS env-file config before persisting a backend switch via `/tts`.
//!
//! Run at the moment of the switch (not at startup) because the user may edit
//! the env file between runs and expect the next `/tts <backend>` to pick it up.

use std::path::Path;

use crate::sanitize;

pub(super) fn validate_kokoro_local_tts_env() -> Result<std::path::PathBuf, String> {
    #[cfg(not(feature = "kokoro-local"))]
    {
        Err(
            "Can't switch to <code>kokoro-local</code>: this binary was built without the \
             <code>kokoro-local</code> cargo feature. Rebuild with \
             <code>cargo build --features kokoro-local</code>, or use \
             <code>kokoro-remote</code>."
                .to_string(),
        )
    }

    #[cfg(feature = "kokoro-local")]
    {
        if crate::audio::espeak::probe().is_none() {
            return Err(kokoro_local_missing_espeak_msg());
        }
        crate::setup::onnxruntime::probe().ok_or_else(kokoro_local_missing_ort_msg)
    }
}

#[cfg(feature = "kokoro-local")]
fn kokoro_local_missing_espeak_msg() -> String {
    #[cfg(windows)]
    {
        "Can't switch to <code>kokoro-local</code>: <code>espeak-ng</code> is not on \
         PATH. Windows Kokoro-local is manual/Advanced only; install espeak-ng, \
         open a new terminal, then retry. <code>kokoro-remote</code> or \
         <code>winrt</code> are the recommended Windows paths."
            .to_string()
    }
    #[cfg(not(windows))]
    {
        "Can't switch to <code>kokoro-local</code>: <code>espeak-ng</code> is not on \
         PATH. Install it with your OS package manager or rerun <code>tebis setup</code>."
            .to_string()
    }
}

#[cfg(feature = "kokoro-local")]
fn kokoro_local_missing_ort_msg() -> String {
    #[cfg(windows)]
    {
        "Can't switch to <code>kokoro-local</code>: <code>onnxruntime.dll</code> is not \
         on any known path. Set <code>ORT_DYLIB_PATH=C:\\path\\to\\onnxruntime.dll</code> \
         or place it under <code>%LOCALAPPDATA%\\Programs\\onnxruntime\\lib\\</code> or \
         <code>%ProgramFiles%\\onnxruntime\\lib\\</code>, then retry."
            .to_string()
    }
    #[cfg(not(windows))]
    {
        "Can't switch to <code>kokoro-local</code>: <code>libonnxruntime</code> is not \
         on any known path. Run <code>tebis setup</code> or set \
         <code>ORT_DYLIB_PATH=/path/to/libonnxruntime</code>, then retry."
            .to_string()
    }
}

pub(super) fn validate_remote_tts_env(env_path: &Path) -> Result<(), String> {
    let url = read_env_key_for_tts(env_path, "TELEGRAM_TTS_REMOTE_URL")?;
    let Some(url) = url.filter(|s| !s.trim().is_empty()) else {
        return Err("Can't switch to <code>kokoro-remote</code>: set \
             <code>TELEGRAM_TTS_REMOTE_URL=https://...</code> in the env file \
             or run <code>tebis setup</code> first."
            .to_string());
    };

    let allow_http = match read_env_key_for_tts(env_path, "TELEGRAM_TTS_REMOTE_ALLOW_HTTP")? {
        Some(raw) => crate::env_file::parse_toggle(&raw)
            .map_err(|e| {
                format!(
                    "Can't switch to <code>kokoro-remote</code>: \
                     <code>TELEGRAM_TTS_REMOTE_ALLOW_HTTP</code> is invalid: <code>{}</code>.",
                    sanitize::escape_html(&e.to_string())
                )
            })?
            .unwrap_or(false),
        None => false,
    };

    if let Some(raw) = read_env_key_for_tts(env_path, "TELEGRAM_TTS_REMOTE_TIMEOUT_SEC")? {
        let timeout_sec = raw.parse::<u32>().map_err(|_| {
            "Can't switch to <code>kokoro-remote</code>: \
             <code>TELEGRAM_TTS_REMOTE_TIMEOUT_SEC</code> must be a positive integer."
                .to_string()
        })?;
        if !(1..=300).contains(&timeout_sec) {
            return Err("Can't switch to <code>kokoro-remote</code>: \
                 <code>TELEGRAM_TTS_REMOTE_TIMEOUT_SEC</code> must be between 1 and 300."
                .to_string());
        }
    }

    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("https://") || allow_http && lower.starts_with("http://")) {
        return Err("Can't switch to <code>kokoro-remote</code>: \
         <code>TELEGRAM_TTS_REMOTE_URL</code> must start with <code>https://</code> \
         (or set <code>TELEGRAM_TTS_REMOTE_ALLOW_HTTP=true</code> for LAN HTTP)."
            .to_string());
    }

    Ok(())
}

fn read_env_key_for_tts(env_path: &Path, key: &str) -> Result<Option<String>, String> {
    crate::env_file::read_key(env_path, key).map_err(|e| {
        format!(
            "Can't switch TTS at runtime: failed to read env file: <code>{}</code>.",
            sanitize::escape_html(&e.to_string())
        )
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::validate_remote_tts_env;

    fn env_file(body: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tebis-tts-remote-{pid}-{nonce:x}-{seq}",
            pid = std::process::id(),
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("env");
        fs::write(&path, body).unwrap();
        path
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn kokoro_remote_switch_requires_url() {
        let path = env_file("TELEGRAM_TTS_BACKEND=none\n");
        let err = validate_remote_tts_env(&path).unwrap_err();
        assert!(err.contains("TELEGRAM_TTS_REMOTE_URL"));
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_accepts_https_url() {
        let path = env_file("TELEGRAM_TTS_REMOTE_URL=https://kokoro.example.com\n");
        validate_remote_tts_env(&path).unwrap();
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_rejects_http_by_default() {
        let path = env_file("TELEGRAM_TTS_REMOTE_URL=http://127.0.0.1:8880\n");
        let err = validate_remote_tts_env(&path).unwrap_err();
        assert!(err.contains("https://"));
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_allows_http_when_enabled() {
        let path = env_file(
            "TELEGRAM_TTS_REMOTE_URL=http://127.0.0.1:8880\n\
             TELEGRAM_TTS_REMOTE_ALLOW_HTTP=true\n",
        );
        validate_remote_tts_env(&path).unwrap();
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_rejects_invalid_allow_http_toggle() {
        let path = env_file(
            "TELEGRAM_TTS_REMOTE_URL=http://127.0.0.1:8880\n\
             TELEGRAM_TTS_REMOTE_ALLOW_HTTP=maybe\n",
        );
        let err = validate_remote_tts_env(&path).unwrap_err();
        assert!(err.contains("TELEGRAM_TTS_REMOTE_ALLOW_HTTP"));
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_rejects_invalid_timeout() {
        let path = env_file(
            "TELEGRAM_TTS_REMOTE_URL=https://kokoro.example.com\n\
             TELEGRAM_TTS_REMOTE_TIMEOUT_SEC=slow\n",
        );
        let err = validate_remote_tts_env(&path).unwrap_err();
        assert!(err.contains("TELEGRAM_TTS_REMOTE_TIMEOUT_SEC"));
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_rejects_out_of_range_timeout() {
        let path = env_file(
            "TELEGRAM_TTS_REMOTE_URL=https://kokoro.example.com\n\
             TELEGRAM_TTS_REMOTE_TIMEOUT_SEC=301\n",
        );
        let err = validate_remote_tts_env(&path).unwrap_err();
        assert!(err.contains("between 1 and 300"));
        cleanup(&path);
    }

    #[test]
    fn kokoro_remote_switch_accepts_timeout_range_edges() {
        for timeout in ["1", "300"] {
            let path = env_file(&format!(
                "TELEGRAM_TTS_REMOTE_URL=https://kokoro.example.com\n\
                 TELEGRAM_TTS_REMOTE_TIMEOUT_SEC={timeout}\n"
            ));
            validate_remote_tts_env(&path).unwrap();
            cleanup(&path);
        }
    }
}
