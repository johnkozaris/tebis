//! Parse an existing env file so the wizard can pre-fill each prompt.

use std::fs;
use std::path::Path;

use super::{Autostart, HooksChoice, VoiceChoice};
use crate::env_file;

/// Previously-saved values extracted from the env file. Every field is
/// `Option` so a missing or partial file falls through to fresh defaults.
#[derive(Default)]
pub(super) struct Discovered {
    pub(super) bot_token: Option<String>,
    pub(super) allowed_user: Option<i64>,
    pub(super) allowed_sessions: Option<Vec<String>>,
    pub(super) autostart: Option<Autostart>,
    pub(super) inspect_port: Option<u16>,
    pub(super) hooks_mode: Option<HooksChoice>,
    pub(super) voice: Option<VoiceChoice>,
}

/// Parse `KEY=VALUE` lines. Comments (`#`) and blank lines skipped;
/// unknown keys ignored; malformed integer values silently fall back
/// to `None`. Uses [`env_file::parse_kv_line`] so `export FOO=bar` and
/// quoted values round-trip identically with the `load_env_file` path.
pub(super) fn discover(env_path: &Path) -> Discovered {
    let Ok(content) = fs::read_to_string(env_path) else {
        return Discovered::default();
    };
    let mut d = Discovered::default();
    let mut auto_session: Option<String> = None;
    let mut auto_dir: Option<String> = None;
    let mut auto_command: Option<String> = None;
    let mut stt_enabled: Option<bool> = None;
    let mut stt_model: Option<String> = None;
    for line in content.lines() {
        let Some((key, value)) = env_file::parse_kv_line(line) else {
            continue;
        };
        match key {
            "TELEGRAM_BOT_TOKEN" if !value.is_empty() => {
                d.bot_token = Some(value.to_string());
            }
            "TELEGRAM_ALLOWED_USER" => {
                d.allowed_user = value.parse().ok().filter(|&n: &i64| n > 0);
            }
            "TELEGRAM_ALLOWED_SESSIONS" => {
                let names: Vec<String> = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !names.is_empty() {
                    d.allowed_sessions = Some(names);
                }
            }
            "TELEGRAM_AUTOSTART_SESSION" if !value.is_empty() => {
                auto_session = Some(value.to_string());
            }
            "TELEGRAM_AUTOSTART_DIR" if !value.is_empty() => {
                auto_dir = Some(value.to_string());
            }
            "TELEGRAM_AUTOSTART_COMMAND" if !value.is_empty() => {
                auto_command = Some(value.to_string());
            }
            "INSPECT_PORT" => {
                d.inspect_port = value.parse().ok().filter(|&n: &u16| n >= 1024);
            }
            "TELEGRAM_HOOKS_MODE" => {
                d.hooks_mode = match value.trim().to_ascii_lowercase().as_str() {
                    "auto" | "on" | "true" | "1" | "yes" => Some(HooksChoice::Auto),
                    "off" | "false" | "0" | "no" | "" => Some(HooksChoice::Off),
                    _ => None, // unknown → let the wizard prompt fresh
                };
            }
            "TELEGRAM_STT" => {
                stt_enabled = crate::env_file::parse_toggle(value).ok().flatten();
            }
            "TELEGRAM_STT_MODEL" if !value.is_empty() => {
                stt_model = Some(value.to_string());
            }
            _ => {}
        }
    }
    if let (Some(session), Some(dir), Some(command)) = (auto_session, auto_dir, auto_command) {
        d.autostart = Some(Autostart {
            session,
            dir,
            command,
        });
    }
    if let Some(enabled) = stt_enabled {
        d.voice = Some(VoiceChoice {
            enabled,
            // Honor the existing model if the user picked one; otherwise
            // leave it empty and let `step_voice` fall through to the
            // manifest default.
            model: stt_model.unwrap_or_default(),
        });
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_parses_full_env_file() {
        let tmp = std::env::temp_dir().join(format!("tebis-discover-{}.env", std::process::id()));
        fs::write(
            &tmp,
            "\
# Written by `tebis setup`.

TELEGRAM_BOT_TOKEN=123:ABCdefGHIjklMNOpqrSTUvwxYZ-1234567890_abcd
TELEGRAM_ALLOWED_USER=1234567890
TELEGRAM_ALLOWED_SESSIONS=claude-code,shell

TELEGRAM_AUTOSTART_SESSION=demo
TELEGRAM_AUTOSTART_DIR=/tmp
TELEGRAM_AUTOSTART_COMMAND=claude

INSPECT_PORT=51624
",
        )
        .unwrap();

        let d = discover(&tmp);
        assert_eq!(
            d.bot_token.as_deref(),
            Some("123:ABCdefGHIjklMNOpqrSTUvwxYZ-1234567890_abcd")
        );
        assert_eq!(d.allowed_user, Some(1_234_567_890));
        assert_eq!(
            d.allowed_sessions.as_deref(),
            Some(&["claude-code".to_string(), "shell".to_string()][..]),
        );
        let a = d.autostart.expect("autostart triple present");
        assert_eq!(a.session, "demo");
        assert_eq!(a.dir, "/tmp");
        assert_eq!(a.command, "claude");
        assert_eq!(d.inspect_port, Some(51_624));

        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn discover_ignores_partial_autostart_triple() {
        let tmp =
            std::env::temp_dir().join(format!("tebis-discover-partial-{}.env", std::process::id()));
        fs::write(
            &tmp,
            "TELEGRAM_AUTOSTART_SESSION=foo\nTELEGRAM_AUTOSTART_DIR=/tmp\n",
        )
        .unwrap();
        let d = discover(&tmp);
        assert!(d.autostart.is_none());
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn discover_returns_default_when_file_missing() {
        let d = discover(Path::new("/tmp/tebis-does-not-exist-xyz"));
        assert!(d.bot_token.is_none());
        assert!(d.allowed_user.is_none());
        assert!(d.allowed_sessions.is_none());
        assert!(d.autostart.is_none());
        assert!(d.inspect_port.is_none());
    }

    #[test]
    fn discover_handles_permissive_allowlist() {
        let tmp = std::env::temp_dir().join(format!(
            "tebis-discover-permissive-{}.env",
            std::process::id()
        ));
        fs::write(
            &tmp,
            "TELEGRAM_BOT_TOKEN=123:ABCdefGHIjklMNOpqrSTUvwxYZ-1234567890_abcd\n\
             # TELEGRAM_ALLOWED_SESSIONS=commented,out\n",
        )
        .unwrap();
        let d = discover(&tmp);
        assert!(d.allowed_sessions.is_none());
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn discover_reads_hooks_mode() {
        let tmp =
            std::env::temp_dir().join(format!("tebis-discover-hooks-{}.env", std::process::id()));
        fs::write(&tmp, "TELEGRAM_HOOKS_MODE=auto\n").unwrap();
        assert!(matches!(discover(&tmp).hooks_mode, Some(HooksChoice::Auto)));
        fs::write(&tmp, "TELEGRAM_HOOKS_MODE=off\n").unwrap();
        assert!(matches!(discover(&tmp).hooks_mode, Some(HooksChoice::Off)));
        fs::write(&tmp, "TELEGRAM_HOOKS_MODE=garbage\n").unwrap();
        assert!(discover(&tmp).hooks_mode.is_none());
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn discover_handles_quoted_and_exported_values() {
        let tmp =
            std::env::temp_dir().join(format!("tebis-discover-quoted-{}.env", std::process::id()));
        fs::write(
            &tmp,
            "export TELEGRAM_BOT_TOKEN=\"123:ABCdefGHIjklMNOpqrSTUvwxYZ-1234567890_abcd\"\n\
             TELEGRAM_AUTOSTART_DIR='/my/path'\n",
        )
        .unwrap();
        let d = discover(&tmp);
        assert_eq!(
            d.bot_token.as_deref(),
            Some("123:ABCdefGHIjklMNOpqrSTUvwxYZ-1234567890_abcd")
        );
        // Autostart isn't built (only 1 of 3 keys), but the parser
        // successfully handled both `export` and matched quotes above.
        let _ = fs::remove_file(&tmp);
    }
}
