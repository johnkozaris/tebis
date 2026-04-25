//! GitHub Copilot CLI hook installer — writes sentinel `<project>/.github/hooks/tebis.json`.
//! Copilot loads every `*.json` in that dir and merges them.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::AgentKind;
use super::jsonfile;

pub struct CopilotHooks;

/// Events + timeouts. Copilot CLI exposes `notification` for async agent
/// completion / permission prompts; there is no Claude-style `agentStop`.
const EVENTS: &[(&str, u64)] = &[("userPromptSubmitted", 5), ("notification", 10)];

const TEBIS_HOOKS_FILE: &str = "tebis.json";

fn hooks_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".github/hooks")
}

fn hooks_file(project_dir: &Path) -> PathBuf {
    hooks_dir(project_dir).join(TEBIS_HOOKS_FILE)
}

impl super::HookManager for CopilotHooks {
    fn install(&self, project_dir: &Path, script_path: &Path) -> Result<super::InstallReport> {
        let dir = hooks_dir(project_dir);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let path = hooks_file(project_dir);
        // Fail loud if the sentinel already exists but isn't ours.
        if path.exists() {
            let existing = jsonfile::load_or_empty(&path)?;
            if !looks_like_tebis_owned(&existing) {
                bail!(
                    "refusing to overwrite {} — file exists but doesn't look like a tebis-owned \
                     hooks config. We expect every `bash` field to point at {}. \
                     Inspect and remove/rename the file before re-installing.",
                    path.display(),
                    script_path.display(),
                );
            }
        }

        let mut hooks_obj = serde_json::Map::new();
        for (event, timeout) in EVENTS {
            // `bash` is Copilot's field name for the command string. On
            // Unix this is just the script path; on Windows it's a
            // PowerShell wrapper — see `super::script_command`. Git
            // Bash on Windows happily executes a `powershell.exe ...`
            // line in the `bash` field.
            let entry = json!({
                "type": "command",
                "bash": super::script_command(script_path),
                "timeoutSec": *timeout,
            });
            hooks_obj.insert((*event).to_string(), Value::Array(vec![entry]));
        }
        let doc = json!({
            "version": 1,
            "hooks": Value::Object(hooks_obj),
        });

        jsonfile::atomic_write_json(&path, &doc)?;
        if let Err(e) = super::manifest::record_install(AgentKind::Copilot, project_dir) {
            tracing::warn!(
                err = %e,
                dir = %project_dir.display(),
                "copilot hooks install: failed to record manifest row — \
                 `tebis hooks list` may omit this install"
            );
        }
        Ok(super::InstallReport {
            files_written: vec![path],
            events: EVENTS.iter().map(|(e, _)| *e).collect(),
        })
    }

    fn uninstall(&self, project_dir: &Path) -> Result<super::UninstallReport> {
        // See Claude uninstaller for rationale.
        super::data_dir().context("resolving tebis data dir for ownership check")?;
        if let Err(e) = super::manifest::record_uninstall(AgentKind::Copilot, project_dir) {
            tracing::warn!(
                err = %e,
                dir = %project_dir.display(),
                "copilot hooks uninstall: failed to drop manifest row — \
                 `tebis hooks list` may show a stale entry"
            );
        }

        let path = hooks_file(project_dir);
        if !path.exists() {
            return Ok(super::UninstallReport::default());
        }
        // Report only what's actually in the file — older tebis versions had fewer events.
        let events_removed: Vec<String> = jsonfile::load_or_empty(&path)
            .ok()
            .and_then(|doc| {
                doc.get("hooks")
                    .and_then(Value::as_object)
                    .map(|h| h.keys().cloned().collect())
            })
            .unwrap_or_default();

        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;

        // Prune empty dirs we may have created. `.github` only when hooks_dir is gone too.
        prune_if_empty(&hooks_dir(project_dir))?;
        if !hooks_dir(project_dir).exists() {
            prune_if_empty(&project_dir.join(".github"))?;
        }

        Ok(super::UninstallReport {
            files_deleted: vec![path],
            events_removed,
            ..Default::default()
        })
    }

    fn status(&self, project_dir: &Path) -> Result<super::StatusReport> {
        super::data_dir().context("resolving tebis data dir for ownership check")?;
        let path = hooks_file(project_dir);
        if !path.exists() {
            return Ok(super::StatusReport::default());
        }
        let doc = jsonfile::load_or_empty(&path)?;
        let events = doc
            .get("hooks")
            .and_then(Value::as_object)
            .map(|h| h.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        Ok(super::StatusReport {
            installed_events: events,
        })
    }
}

/// Remove `dir` only if empty. `NotFound` is fine.
fn prune_if_empty(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let is_empty = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .next()
        .is_none();
    if is_empty {
        fs::remove_dir(dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

/// True when every `bash` in the doc points at a tebis-owned script.
fn looks_like_tebis_owned(doc: &Value) -> bool {
    let Some(obj) = doc.as_object() else {
        return false;
    };
    let Some(hooks) = obj.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    hooks.values().all(|v| {
        v.as_array().is_some_and(|arr| {
            arr.iter().all(|entry| {
                entry
                    .get("bash")
                    .and_then(Value::as_str)
                    .is_some_and(super::command_references_our_script)
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::with_hook_fixtures;
    use super::*;
    use std::fs;

    #[test]
    fn install_writes_sentinel_file_with_all_events() {
        with_hook_fixtures("install", AgentKind::Copilot, |mgr, proj, script| {
            let r = mgr.install(proj, script).unwrap();
            assert_eq!(r.files_written.len(), 1);
            assert_eq!(r.events.len(), EVENTS.len());

            let doc: Value =
                serde_json::from_str(&fs::read_to_string(&r.files_written[0]).unwrap()).unwrap();
            assert_eq!(doc["version"], 1);
            for (event, _) in EVENTS {
                let arr = doc["hooks"][event].as_array().unwrap();
                assert_eq!(arr.len(), 1);
                assert_eq!(arr[0]["type"], "command");
                // On Unix `bash` holds the raw script path; on Windows it's
                // the `powershell.exe … -File "<path>"` wrapper from
                // `super::script_command` — match the installer's behavior.
                assert_eq!(arr[0]["bash"], super::super::script_command(script));
            }
        });
    }

    #[test]
    fn install_is_idempotent() {
        with_hook_fixtures("idempotent", AgentKind::Copilot, |mgr, proj, script| {
            mgr.install(proj, script).unwrap();
            mgr.install(proj, script).unwrap();
            let doc: Value =
                serde_json::from_str(&fs::read_to_string(hooks_file(proj)).unwrap()).unwrap();
            for (event, _) in EVENTS {
                assert_eq!(doc["hooks"][event].as_array().unwrap().len(), 1);
            }
        });
    }

    #[test]
    fn uninstall_leaves_sibling_user_files_alone() {
        with_hook_fixtures("siblings", AgentKind::Copilot, |mgr, proj, script| {
            let dir = hooks_dir(proj);
            fs::create_dir_all(&dir).unwrap();
            let user_file = dir.join("user-own.json");
            fs::write(&user_file, r#"{"version":1,"hooks":{}}"#).unwrap();

            mgr.install(proj, script).unwrap();
            mgr.uninstall(proj).unwrap();

            assert!(!hooks_file(proj).exists(), "tebis.json removed");
            assert!(user_file.exists(), "user's own file untouched");
            assert!(dir.exists(), "dir still has content");
        });
    }

    #[test]
    fn uninstall_prunes_empty_dirs_we_created() {
        with_hook_fixtures("prune", AgentKind::Copilot, |mgr, proj, script| {
            mgr.install(proj, script).unwrap();
            mgr.uninstall(proj).unwrap();
            assert!(!hooks_dir(proj).exists());
            assert!(!proj.join(".github").exists());
        });
    }

    #[test]
    fn uninstall_is_idempotent_on_missing_file() {
        with_hook_fixtures("missing", AgentKind::Copilot, |mgr, proj, _| {
            let r = mgr.uninstall(proj).unwrap();
            assert!(r.files_deleted.is_empty());
        });
    }

    #[test]
    fn status_reports_events() {
        with_hook_fixtures("status", AgentKind::Copilot, |mgr, proj, script| {
            mgr.install(proj, script).unwrap();
            let r = mgr.status(proj).unwrap();
            assert_eq!(r.installed_events.len(), EVENTS.len());
        });
    }
}
