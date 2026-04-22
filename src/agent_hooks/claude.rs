//! Claude Code hook installer — merges into `<project>/.claude/settings.local.json`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::AgentKind;
use super::jsonfile;

pub struct ClaudeHooks;

/// Installed events + timeouts (seconds). Shorter than Claude's 600s default
/// — a slow hook should fall back to pane-settle, not hang the turn.
/// SessionStart/SessionEnd deliberately omitted: one is noise, the other
/// doesn't fire on tmux `kill-session`.
const EVENTS: &[(&str, u64)] = &[
    ("UserPromptSubmit", 5),
    ("Stop", 15),
    ("SubagentStop", 15),
    ("Notification", 10),
];

fn settings_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".claude/settings.local.json")
}

impl super::HookManager for ClaudeHooks {
    fn agent(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn install(&self, project_dir: &Path, script_path: &Path) -> Result<super::InstallReport> {
        let path = settings_path(project_dir);
        let mut settings = jsonfile::load_or_empty(&path)?;
        let root = as_object_mut(&mut settings, &path)?;

        let hooks = root.entry("hooks".to_string()).or_insert_with(|| json!({}));
        let hooks_obj = hooks
            .as_object_mut()
            .context("`.hooks` in settings.local.json must be an object")?;

        for (event, timeout) in EVENTS {
            // Reuse whatever case the user already has so we don't double-fire.
            let canonical = (*event).to_string();
            let key = hooks_obj
                .keys()
                .find(|k| k.eq_ignore_ascii_case(event))
                .cloned()
                .unwrap_or(canonical);
            let entries = hooks_obj.entry(key).or_insert_with(|| json!([]));
            let arr = entries
                .as_array_mut()
                .with_context(|| format!("`.hooks.{event}` must be an array"))?;
            arr.retain(|e| !entry_points_at(e, script_path));
            arr.push(json!({
                "hooks": [
                    {
                        "type": "command",
                        "command": script_path.to_string_lossy(),
                        "timeout": *timeout,
                    }
                ]
            }));
        }

        jsonfile::atomic_write_json(&path, &settings)?;
        if let Err(e) = super::manifest::record_install(AgentKind::Claude, project_dir) {
            tracing::warn!(
                err = %e,
                dir = %project_dir.display(),
                "claude hooks install: failed to record manifest row — \
                 `tebis hooks list` may omit this install"
            );
        }
        Ok(super::InstallReport {
            files_written: vec![path],
            events: EVENTS.iter().map(|(e, _)| *e).collect(),
        })
    }

    fn uninstall(&self, project_dir: &Path) -> Result<super::UninstallReport> {
        // Fail loud if data_dir is unresolvable — otherwise is_our_script silently
        // returns false and we report success while leaving entries in place.
        super::data_dir().context("resolving tebis data dir for ownership check")?;
        // Drop manifest row unconditionally; a stale row would make `tebis hooks list` lie.
        if let Err(e) = super::manifest::record_uninstall(AgentKind::Claude, project_dir) {
            tracing::warn!(
                err = %e,
                dir = %project_dir.display(),
                "claude hooks uninstall: failed to drop manifest row — \
                 `tebis hooks list` may show a stale entry"
            );
        }

        let path = settings_path(project_dir);
        if !path.exists() {
            return Ok(super::UninstallReport::default());
        }
        let mut settings = jsonfile::load_or_empty(&path)?;
        let root = as_object_mut(&mut settings, &path)?;

        let mut events_removed = Vec::new();
        if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
            for (event, entries) in hooks.iter_mut() {
                if let Some(arr) = entries.as_array_mut() {
                    let before = arr.len();
                    arr.retain(|e| !is_tebis_entry(e));
                    // Drop wrappers with empty inner hooks — dead weight.
                    arr.retain(|e| {
                        e.get("hooks")
                            .and_then(Value::as_array)
                            .is_some_and(|inner| !inner.is_empty())
                    });
                    if arr.len() < before {
                        events_removed.push(event.clone());
                    }
                }
            }
            hooks.retain(|_, v| !v.as_array().is_some_and(Vec::is_empty));
            let hooks_empty = hooks.is_empty();
            if hooks_empty {
                root.remove("hooks");
            }
        }

        if root.is_empty() {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing empty {}", path.display()))?;
            Ok(super::UninstallReport {
                files_deleted: vec![path],
                events_removed,
                ..Default::default()
            })
        } else {
            jsonfile::atomic_write_json(&path, &settings)?;
            Ok(super::UninstallReport {
                files_modified: vec![path],
                events_removed,
                ..Default::default()
            })
        }
    }

    fn status(&self, project_dir: &Path) -> Result<super::StatusReport> {
        super::data_dir().context("resolving tebis data dir for ownership check")?;
        let path = settings_path(project_dir);
        if !path.exists() {
            return Ok(super::StatusReport::default());
        }
        let settings = jsonfile::load_or_empty(&path)?;
        let installed = settings
            .get("hooks")
            .and_then(Value::as_object)
            .map(|hooks| {
                hooks
                    .iter()
                    .filter(|(_, v)| {
                        v.as_array()
                            .is_some_and(|arr| arr.iter().any(is_tebis_entry))
                    })
                    .map(|(k, _)| k.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(super::StatusReport {
            installed_events: installed,
        })
    }
}

fn as_object_mut<'a>(
    v: &'a mut Value,
    path: &Path,
) -> Result<&'a mut serde_json::Map<String, Value>> {
    let kind = type_name(v);
    v.as_object_mut().with_context(|| {
        format!(
            "root of {} must be a JSON object (found {kind})",
            path.display()
        )
    })
}

const fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// True when an entry's inner `command` resolves to `script_path` (via `paths_eq`).
fn entry_points_at(entry: &Value, script_path: &Path) -> bool {
    let Some(inner) = entry.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    inner.iter().any(|h| {
        h.get("command")
            .and_then(Value::as_str)
            .is_some_and(|s| super::paths_eq(Path::new(s), script_path))
    })
}

/// True when an entry points at any path under our data dir — sweep tolerant to renames.
fn is_tebis_entry(entry: &Value) -> bool {
    let Some(inner) = entry.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    inner.iter().any(|h| {
        h.get("command")
            .and_then(Value::as_str)
            .is_some_and(|s| super::is_our_script(Path::new(s)))
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::with_hook_fixtures;
    use super::*;
    use std::fs;

    #[test]
    fn install_creates_file_with_all_events() {
        with_hook_fixtures("create", AgentKind::Claude, |mgr, proj, script| {
            let report = mgr.install(proj, script).expect("install");
            assert_eq!(report.files_written.len(), 1);
            assert_eq!(report.events.len(), EVENTS.len());

            let v: Value =
                serde_json::from_str(&fs::read_to_string(settings_path(proj)).unwrap()).unwrap();
            for (event, _) in EVENTS {
                assert!(
                    v["hooks"][event].is_array(),
                    "event {event} missing or wrong shape"
                );
            }
        });
    }

    #[test]
    fn install_preserves_user_entries_in_same_event() {
        with_hook_fixtures("preserve", AgentKind::Claude, |mgr, proj, script| {
            fs::create_dir_all(proj.join(".claude")).unwrap();
            fs::write(
                settings_path(proj),
                r#"{
                    "hooks": {
                        "Stop": [
                            {
                                "hooks": [
                                    {"type": "command", "command": "/usr/local/bin/users-own-hook", "timeout": 7}
                                ]
                            }
                        ]
                    },
                    "permissions": {"allow": ["Bash(echo:*)"]}
                }"#,
            )
            .unwrap();

            mgr.install(proj, script).unwrap();
            let v: Value =
                serde_json::from_str(&fs::read_to_string(settings_path(proj)).unwrap()).unwrap();
            let stop = v["hooks"]["Stop"].as_array().unwrap();
            assert_eq!(stop.len(), 2, "user's Stop hook + tebis's");
            assert_eq!(v["permissions"]["allow"][0], "Bash(echo:*)");
        });
    }

    #[test]
    fn install_is_idempotent_no_duplicates() {
        with_hook_fixtures("idempotent", AgentKind::Claude, |mgr, proj, script| {
            mgr.install(proj, script).unwrap();
            mgr.install(proj, script).unwrap();

            let v: Value =
                serde_json::from_str(&fs::read_to_string(settings_path(proj)).unwrap()).unwrap();
            for (event, _) in EVENTS {
                let arr = v["hooks"][event].as_array().unwrap();
                let ours = arr.iter().filter(|e| entry_points_at(e, script)).count();
                assert_eq!(ours, 1, "event {event} has {ours} tebis entries, want 1");
            }
        });
    }

    #[test]
    fn uninstall_removes_only_ours_and_deletes_empty_file() {
        with_hook_fixtures("uninstall", AgentKind::Claude, |mgr, proj, script| {
            mgr.install(proj, script).unwrap();
            let r = mgr.uninstall(proj).unwrap();
            assert_eq!(r.files_deleted.len(), 1);
            assert_eq!(r.events_removed.len(), EVENTS.len());
            assert!(!settings_path(proj).exists());
        });
    }

    #[test]
    fn uninstall_keeps_file_when_user_entries_remain() {
        with_hook_fixtures("partial", AgentKind::Claude, |mgr, proj, script| {
            fs::create_dir_all(proj.join(".claude")).unwrap();
            fs::write(
                settings_path(proj),
                r#"{
                    "hooks": {
                        "Stop": [
                            {"hooks": [{"type": "command", "command": "/usr/local/bin/users-own", "timeout": 5}]}
                        ]
                    }
                }"#,
            )
            .unwrap();
            mgr.install(proj, script).unwrap();
            mgr.uninstall(proj).unwrap();

            let v: Value =
                serde_json::from_str(&fs::read_to_string(settings_path(proj)).unwrap()).unwrap();
            let stop = v["hooks"]["Stop"].as_array().unwrap();
            assert_eq!(stop.len(), 1);
            assert!(
                stop[0]["hooks"][0]["command"]
                    .as_str()
                    .unwrap()
                    .contains("users-own")
            );
        });
    }

    #[test]
    fn uninstall_is_idempotent_on_missing_file() {
        with_hook_fixtures("missing", AgentKind::Claude, |mgr, proj, _| {
            let r = mgr.uninstall(proj).unwrap();
            assert!(r.files_deleted.is_empty());
            assert!(r.files_modified.is_empty());
        });
    }

    #[test]
    fn install_refuses_malformed_user_file() {
        with_hook_fixtures("malformed", AgentKind::Claude, |mgr, proj, script| {
            fs::create_dir_all(proj.join(".claude")).unwrap();
            fs::write(settings_path(proj), "this is not json").unwrap();
            let err = mgr.install(proj, script).unwrap_err();
            assert!(
                err.to_string().contains("refusing to overwrite"),
                "error should name the protection: {err}"
            );
            assert_eq!(
                fs::read_to_string(settings_path(proj)).unwrap(),
                "this is not json"
            );
        });
    }

    #[test]
    fn status_reports_installed_events() {
        with_hook_fixtures("status", AgentKind::Claude, |mgr, proj, script| {
            mgr.install(proj, script).unwrap();
            let r = mgr.status(proj).unwrap();
            assert_eq!(r.installed_events.len(), EVENTS.len());
        });
    }
}
