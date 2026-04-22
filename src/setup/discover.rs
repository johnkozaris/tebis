//! Parse an existing env file so the wizard can pre-fill each prompt.

use std::fs;
use std::path::Path;

use super::{Autostart, HooksChoice, TtsChoice, VoiceChoice};
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
    pub(super) tts: Option<TtsChoice>,
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

    // TTS parsing — new env layout. We collect raw values and build the
    // TtsChoice variant at the end so ordering in the file doesn't
    // matter.
    let mut tts_backend: Option<String> = None;
    let mut legacy_tts_on: Option<bool> = None;
    let mut tts_voice: Option<String> = None;
    let mut tts_model: Option<String> = None;
    let mut tts_respond_to_all: Option<bool> = None;
    let mut tts_remote_url: Option<String> = None;
    let mut tts_remote_api_key: Option<String> = None;
    let mut tts_remote_model: Option<String> = None;
    let mut tts_remote_timeout_sec: Option<u32> = None;
    let mut tts_remote_allow_http: Option<bool> = None;

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
            "TELEGRAM_TTS" => {
                legacy_tts_on = crate::env_file::parse_toggle(value).ok().flatten();
            }
            "TELEGRAM_TTS_BACKEND" if !value.is_empty() => {
                tts_backend = Some(value.trim().to_ascii_lowercase());
            }
            "TELEGRAM_TTS_VOICE" if !value.is_empty() => {
                tts_voice = Some(value.to_string());
            }
            "TELEGRAM_TTS_MODEL" if !value.is_empty() => {
                tts_model = Some(value.to_string());
            }
            "TELEGRAM_TTS_RESPOND_TO_ALL" => {
                tts_respond_to_all = crate::env_file::parse_toggle(value).ok().flatten();
            }
            "TELEGRAM_TTS_REMOTE_URL" if !value.is_empty() => {
                tts_remote_url = Some(value.trim().to_string());
            }
            "TELEGRAM_TTS_REMOTE_API_KEY" if !value.is_empty() => {
                tts_remote_api_key = Some(value.to_string());
            }
            "TELEGRAM_TTS_REMOTE_MODEL" if !value.is_empty() => {
                tts_remote_model = Some(value.to_string());
            }
            "TELEGRAM_TTS_REMOTE_TIMEOUT_SEC" => {
                tts_remote_timeout_sec = value.parse().ok().filter(|&n: &u32| (1..=300).contains(&n));
            }
            "TELEGRAM_TTS_REMOTE_ALLOW_HTTP" => {
                tts_remote_allow_http = crate::env_file::parse_toggle(value).ok().flatten();
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

    // Resolve TTS choice. Priority:
    //   1. Explicit TELEGRAM_TTS_BACKEND → use that.
    //   2. Legacy `TELEGRAM_TTS=on` (pre-v2 env files) → interpret as
    //      Say (macOS) / unknown (Linux falls through to None).
    //   3. Nothing set → None (wizard starts fresh on the TTS step).
    d.tts = resolve_tts_choice(
        tts_backend.as_deref(),
        legacy_tts_on,
        tts_voice,
        tts_model,
        tts_respond_to_all.unwrap_or(false),
        tts_remote_url,
        tts_remote_api_key,
        tts_remote_model,
        tts_remote_timeout_sec,
        tts_remote_allow_http.unwrap_or(false),
    );
    d
}

#[allow(clippy::too_many_arguments, reason = "wizard-internal helper; grouping adds nothing")]
fn resolve_tts_choice(
    backend: Option<&str>,
    legacy_on: Option<bool>,
    voice: Option<String>,
    model: Option<String>,
    respond_to_all: bool,
    remote_url: Option<String>,
    remote_api_key: Option<String>,
    remote_model: Option<String>,
    remote_timeout_sec: Option<u32>,
    remote_allow_http: bool,
) -> Option<TtsChoice> {
    let backend_kind = match backend {
        Some(s) => s,
        None => {
            // Legacy path — if the old TELEGRAM_TTS toggle was set,
            // interpret it: `on` → Say (the only backend that existed
            // in Phase 4a), `off` → TtsChoice::Off. Unset → no
            // discovery (wizard prompts fresh).
            return match legacy_on {
                Some(true) => Some(TtsChoice::Say {
                    voice: voice.unwrap_or_else(|| "Samantha".to_string()),
                    respond_to_all,
                }),
                Some(false) => Some(TtsChoice::Off),
                None => None,
            };
        }
    };

    match backend_kind {
        "none" | "off" | "false" | "0" => Some(TtsChoice::Off),
        "say" => Some(TtsChoice::Say {
            voice: voice.unwrap_or_else(|| "Samantha".to_string()),
            respond_to_all,
        }),
        "kokoro-local" | "kokoro_local" | "local" => Some(TtsChoice::KokoroLocal {
            model: model.unwrap_or_default(),
            voice: voice.unwrap_or_else(|| "af_sarah".to_string()),
            respond_to_all,
        }),
        "kokoro-remote" | "kokoro_remote" | "remote" => {
            // URL is the only hard requirement; without it, fall back
            // to None so the wizard surfaces a fresh prompt instead of
            // pre-filling a broken config.
            let url = remote_url?;
            Some(TtsChoice::KokoroRemote {
                url,
                api_key: remote_api_key,
                model: remote_model.unwrap_or_else(|| "kokoro".to_string()),
                voice: voice.unwrap_or_else(|| "af_sarah".to_string()),
                timeout_sec: remote_timeout_sec.unwrap_or(10),
                allow_http: remote_allow_http,
                respond_to_all,
            })
        }
        _ => None, // unknown backend — let wizard prompt fresh
    }
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
    fn discover_reads_tts_backend_say() {
        let tmp =
            std::env::temp_dir().join(format!("tebis-discover-tts-say-{}.env", std::process::id()));
        fs::write(
            &tmp,
            "TELEGRAM_TTS_BACKEND=say\n\
             TELEGRAM_TTS_VOICE=Alex\n\
             TELEGRAM_TTS_RESPOND_TO_ALL=on\n",
        )
        .unwrap();
        let d = discover(&tmp);
        match d.tts.expect("tts choice") {
            TtsChoice::Say { voice, respond_to_all } => {
                assert_eq!(voice, "Alex");
                assert!(respond_to_all);
            }
            other => panic!("expected Say, got {other:?}"),
        }
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn discover_reads_tts_backend_remote() {
        let tmp = std::env::temp_dir()
            .join(format!("tebis-discover-tts-remote-{}.env", std::process::id()));
        fs::write(
            &tmp,
            "TELEGRAM_TTS_BACKEND=kokoro-remote\n\
             TELEGRAM_TTS_REMOTE_URL=https://kokoro.example.com\n\
             TELEGRAM_TTS_REMOTE_API_KEY=secret123\n\
             TELEGRAM_TTS_REMOTE_MODEL=kokoro-v2\n\
             TELEGRAM_TTS_VOICE=af_sarah\n\
             TELEGRAM_TTS_REMOTE_TIMEOUT_SEC=20\n",
        )
        .unwrap();
        let d = discover(&tmp);
        match d.tts.expect("tts choice") {
            TtsChoice::KokoroRemote {
                url,
                api_key,
                model,
                voice,
                timeout_sec,
                allow_http,
                ..
            } => {
                assert_eq!(url, "https://kokoro.example.com");
                assert_eq!(api_key.as_deref(), Some("secret123"));
                assert_eq!(model, "kokoro-v2");
                assert_eq!(voice, "af_sarah");
                assert_eq!(timeout_sec, 20);
                assert!(!allow_http);
            }
            other => panic!("expected KokoroRemote, got {other:?}"),
        }
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn discover_remote_without_url_returns_none() {
        // Explicit kokoro-remote backend but missing URL — we refuse
        // to pre-fill a broken config. Wizard falls through to a fresh
        // prompt on the TTS step.
        let tmp = std::env::temp_dir()
            .join(format!("tebis-discover-tts-noremote-{}.env", std::process::id()));
        fs::write(&tmp, "TELEGRAM_TTS_BACKEND=kokoro-remote\n").unwrap();
        let d = discover(&tmp);
        assert!(d.tts.is_none());
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn discover_legacy_tts_on_interpreted_as_say() {
        // Pre-v2 env files used `TELEGRAM_TTS=on` (no backend selector).
        // Re-running the wizard must recognize this and offer Say as
        // the prefilled default instead of treating it as "nothing set."
        let tmp = std::env::temp_dir()
            .join(format!("tebis-discover-legacy-{}.env", std::process::id()));
        fs::write(
            &tmp,
            "TELEGRAM_TTS=on\nTELEGRAM_TTS_VOICE=Samantha\n",
        )
        .unwrap();
        let d = discover(&tmp);
        match d.tts.expect("legacy tts on") {
            TtsChoice::Say { voice, .. } => assert_eq!(voice, "Samantha"),
            other => panic!("expected Say, got {other:?}"),
        }
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn discover_tts_backend_none_is_off() {
        let tmp = std::env::temp_dir()
            .join(format!("tebis-discover-ttsnone-{}.env", std::process::id()));
        fs::write(&tmp, "TELEGRAM_TTS_BACKEND=none\n").unwrap();
        let d = discover(&tmp);
        assert!(matches!(d.tts, Some(TtsChoice::Off)));
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
