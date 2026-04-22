//! GitHub Copilot CLI hook installer.
//!
//! Writes to `<project>/.github/hooks/tebis.json` — a single file owned
//! entirely by tebis. Copilot loads every `*.json` in `.github/hooks/`
//! and merges them, so a sentinel filename lets us co-exist cleanly
//! with user-authored files.
//!
//! Schema (docs.github.com/en/copilot/reference/hooks-configuration,
//! verified against CLI v1.0.32, April 2026):
//!
//! ```jsonc
//! {
//!   "version": 1,
//!   "hooks": {
//!     "notification": [
//!       {
//!         "type": "command",
//!         "bash": "/path/to/hook.sh",
//!         "timeoutSec": 10
//!       }
//!     ]
//!   }
//! }
//! ```
//!
//! Event-name casing: camelCase is the native CLI form. `PascalCase`
//! keys (`SessionStart`, etc.) are accepted and activate a
//! `VS Code`-compatible payload variant with ISO timestamps.
//!
//! ## Events we install (verified against v1.0.32 changelog)
//!
//! - `userPromptSubmitted` — inject the "end with a summary" context
//!   (same pattern as our Claude install). `additionalContext` output
//!   is honored (v1.0.24+).
//! - `agentStop` — primary per-turn reply signal. Added in v0.0.401
//!   (2026-02-03); the CLI's main "agent completed this turn" hook.
//! - `subagentStop` — same as above for task-tool subagents.
//! - `sessionStart` — one-shot "agent is ready" notification. Fires
//!   once per session (v1.0.22+). Can inject `additionalContext`
//!   (v1.0.18+) with a tebis banner. Passes `source=new|resume|...`.
//! - `sessionEnd` — clean-exit signal so the phone can distinguish
//!   "agent quit" from silent death.
//! - `notification` — async catch-all: permission prompts, shell
//!   completion, elicitation dialogs (v1.0.18+).
//!
//! Event-name casing: we use camelCase throughout. `PascalCase` keys
//! (`SessionStart`, etc.) are accepted and activate a
//! `VS Code`-compatible payload variant with ISO timestamps and
//! `hook_event_name` / `session_id` (v1.0.21+); the script handles
//! both forms for compat.
//!
//! Events we intentionally skip: `preToolUse` / `postToolUse` are
//! noisy and not part of the reply-forwarding contract;
//! `PermissionRequest` (v1.0.16+) is a phase-2 feature (bidirectional
//! approve/deny from Telegram); `preCompact` is a phase-2 feature
//! (pre-compaction summary save).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::AgentKind;
use super::jsonfile;

pub struct CopilotHooks;

/// Events we install + their timeout seconds. Every entry is confirmed
/// in the Copilot CLI changelog (v0.0.401 for `agentStop`/`subagentStop`,
/// v1.0.18 for `notification`, v1.0.22 for `sessionStart` hot-reload).
/// Adding unsupported events writes dead JSON entries that the CLI
/// silently ignores and trip up `tebis hooks status` accuracy — only
/// add events you've confirmed in the changelog.
const EVENTS: &[(&str, u64)] = &[
    ("userPromptSubmitted", 5),
    ("agentStop", 15),
    ("subagentStop", 15),
    ("notification", 10),
];
// NOTE: sessionStart / sessionEnd are intentionally NOT installed —
// same rationale as Claude. The first agent reply proves the session
// is up; explicit "[up]" / "[end]" pings are UX noise on a single-user
// bot where the user drives the whole lifecycle.

/// Sentinel file name. Nothing else in `.github/hooks/` is ours; we own
/// this file outright.
const TEBIS_HOOKS_FILE: &str = "tebis.json";

fn hooks_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".github/hooks")
}

fn hooks_file(project_dir: &Path) -> PathBuf {
    hooks_dir(project_dir).join(TEBIS_HOOKS_FILE)
}

impl super::HookManager for CopilotHooks {
    fn agent(&self) -> AgentKind {
        AgentKind::Copilot
    }

    fn install(&self, project_dir: &Path, script_path: &Path) -> Result<super::InstallReport> {
        let dir = hooks_dir(project_dir);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let path = hooks_file(project_dir);
        // If the file already exists, it must be a tebis-owned doc —
        // i.e. `{version:1, hooks:{…}}` where every hook entry points
        // at a tebis script. Anything else is a user-authored file that
        // coincidentally collided with our sentinel name, and we'd
        // rather fail loudly than silently overwrite it.
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
            let entry = json!({
                "type": "command",
                "bash": script_path.to_string_lossy(),
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
        // Probe data_dir first — see the Claude uninstaller for the
        // same invariant. We also use data_dir via looks_like_tebis_owned
        // in install; this keeps the behavior symmetric.
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
        // Report only events that were actually in the file (not the
        // EVENTS constant — users on older tebis versions might have
        // a subset).
        let events_removed: Vec<String> = jsonfile::load_or_empty(&path)
            .ok()
            .and_then(|doc| {
                doc.get("hooks")
                    .and_then(Value::as_object)
                    .map(|h| h.keys().cloned().collect())
            })
            .unwrap_or_default();

        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;

        // Prune empty dirs we created (never remove user dirs that
        // still contain other files). `.github` predates tebis in most
        // repos — only prune it when it's entirely empty AND hooks_dir
        // is now gone, i.e. we're the reason it exists at all.
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

/// Remove a directory only if it's empty. Never touches a dir we didn't
/// plausibly create. `NotFound` is fine; anything else bubbles up.
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

/// Shape check: the doc must be an object with `hooks` and, if present,
/// every hook command points at a tebis-owned script. Prevents the
/// installer from overwriting a hand-written `tebis.json` that happens
/// to live at our sentinel path.
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
                    .is_some_and(|s| super::is_our_script(Path::new(s)))
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
                assert_eq!(arr[0]["bash"], script.to_string_lossy().into_owned());
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
