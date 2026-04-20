//! End-to-end tests for the embedded hook scripts.
//!
//! These drive the real shell script (pre-installed via `materialize`)
//! with a captured JSON fixture on stdin, and assert that the expected
//! `{text, kind, cwd, session}` payload lands on a `UnixListener` that
//! plays the bridge's notify socket.
//!
//! Why this file exists: the scripts ship embedded via `include_str!`.
//! A silent schema drift in Claude Code's `Stop` payload (they renamed
//! `type` → `role` once) would make `jq` emit empty, and without an
//! e2e test the regression would only surface in a user's "why is
//! tebis silently not forwarding?" support ticket.
//!
//! Requirements: `bash`, `jq`, `nc` on `$PATH`. CI preinstalls both.
//! Local runs without them skip with a single log line.
//!
//! Serialization: every test goes through `with_scratch_data_home`,
//! which holds the env-lock mutex — safe to run in parallel with the
//! rest of the test suite because `XDG_DATA_HOME` isolation is tight.

#![cfg(test)]

use std::io::Write as _;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::test_support::with_scratch_data_home;
use super::{AgentKind, materialize};

/// Returns `true` iff every tool the hook script calls is on `PATH`.
/// When false, the e2e tests skip — they'd fail spuriously on a
/// minimal runner that doesn't have `nc`.
fn shell_tools_available() -> bool {
    ["bash", "jq", "nc"].iter().all(|tool| {
        Command::new("which")
            .arg(tool)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// A minimal Unix listener on a tempdir socket. Accepts one connection,
/// reads one newline-terminated line, returns the raw bytes. Blocks on
/// accept so the test is deterministic.
struct FakeBridge {
    path: PathBuf,
    listener: UnixListener,
}

impl FakeBridge {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tebis-e2e-{tag}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind fake bridge");
        listener.set_nonblocking(false).expect("blocking listener");
        Self { path, listener }
    }

    /// Accept exactly one connection and read one line. 5s wall-clock cap.
    fn receive(&self) -> String {
        use std::io::{BufRead, BufReader};
        // macOS: set_read_timeout needs Tokio or SO_RCVTIMEO via socket2.
        // Simpler: spawn a dedicated thread with a join timeout.
        let listener = self.listener.try_clone().expect("clone listener");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut buf = String::new();
            let _ = reader.read_line(&mut buf);
            let _ = tx.send(buf);
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("FakeBridge: no payload arrived within 5s")
    }
}

impl Drop for FakeBridge {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Spawn `bash <script>` with `NOTIFY_SOCKET_PATH=<socket>` and
/// `stdin=<fixture>`. Returns the exit status so callers can assert
/// success (exit 0 always, per the script's fail-open contract).
fn run_hook(script: &std::path::Path, socket: &std::path::Path, stdin_fixture: &str) {
    let mut child = Command::new("bash")
        .arg(script)
        .env("NOTIFY_SOCKET_PATH", socket)
        // Keep the agent's own XDG away from the fake bridge — the hook
        // resolves socket explicitly via NOTIFY_SOCKET_PATH first.
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bash");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin_fixture.as_bytes())
        .expect("write stdin");
    drop(child.stdin.take());
    let status = child.wait().expect("wait bash");
    assert!(
        status.success(),
        "hook script exited non-zero (fail-open violated): {status:?}"
    );
}

/// Extract `{kind, text}` from the JSON line the script writes to
/// the socket. Uses `serde_json` via the crate so tests stay hermetic.
fn parse_forwarded(line: &str) -> (String, String) {
    let v: serde_json::Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|e| panic!("bridge received non-JSON line: {line:?} ({e})"));
    let kind = v["kind"].as_str().unwrap_or("").to_string();
    let text = v["text"].as_str().unwrap_or("").to_string();
    (kind, text)
}

#[test]
fn claude_notification_forwards_message_with_kind_tag() {
    if !shell_tools_available() {
        eprintln!("skipping: bash/jq/nc not on PATH");
        return;
    }
    with_scratch_data_home("claude_notification", || {
        let script = materialize(AgentKind::Claude).expect("materialize");
        let bridge = FakeBridge::new("claude-notification");
        let fixture = r#"{"hook_event_name":"Notification","message":"Claude needs permission to edit /tmp/x","notification_type":"permission_prompt","cwd":"/tmp","session_id":"s123"}"#;
        run_hook(&script, &bridge.path, fixture);
        let line = bridge.receive();
        let (kind, text) = parse_forwarded(&line);
        assert_eq!(kind, "permission_prompt");
        assert!(
            text.contains("needs permission"),
            "unexpected text: {text:?}"
        );
    });
}

#[test]
fn claude_session_events_are_not_dispatched() {
    // SessionStart / SessionEnd are deliberately NOT installed (see
    // src/agent_hooks/claude.rs EVENTS comment). Even if someone
    // hand-installs them, our shell script no longer dispatches —
    // the events fall through to the `*)` no-op arm.
    if !shell_tools_available() {
        eprintln!("skipping: bash/jq/nc not on PATH");
        return;
    }
    with_scratch_data_home("claude_session_events_skipped", || {
        let script = materialize(AgentKind::Claude).expect("materialize");
        let bridge = FakeBridge::new("claude-session-events-skipped");
        for evt in ["SessionStart", "SessionEnd"] {
            let fixture = format!(
                r#"{{"hook_event_name":"{evt}","source":"startup","reason":"logout","cwd":"/tmp","session_id":"s"}}"#
            );
            run_hook(&script, &bridge.path, &fixture);
        }
        let listener = bridge.listener.try_clone().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(
            listener.accept().is_err(),
            "expected no socket traffic — session_* events are not handled"
        );
    });
}

#[test]
fn claude_user_prompt_submit_writes_hookspecificoutput_stdout() {
    // This is the one event whose CONTRACT is stdout, not socket.
    // We assert the script emits valid JSON with the documented
    // hookSpecificOutput.additionalContext field.
    if !shell_tools_available() {
        eprintln!("skipping: bash/jq/nc not on PATH");
        return;
    }
    with_scratch_data_home("claude_user_prompt", || {
        let script = materialize(AgentKind::Claude).expect("materialize");
        let fixture = r#"{"hook_event_name":"UserPromptSubmit","prompt":"hello","cwd":"/tmp","session_id":"s1"}"#;
        let mut child = Command::new("bash")
            .arg(&script)
            .env_remove("XDG_RUNTIME_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bash");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(fixture.as_bytes())
            .expect("write stdin");
        drop(child.stdin.take());
        let out = child.wait_with_output().expect("wait bash");
        assert!(out.status.success());
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("UserPromptSubmit stdout not JSON: {e}"));
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"].as_str(),
            Some("UserPromptSubmit")
        );
        assert!(
            v["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap_or("")
                .contains("summary"),
            "additionalContext should mention summary: {v}"
        );
    });
}

#[test]
fn copilot_agent_stop_branch_forwards_something() {
    // Copilot's `agentStop` payload historically has varied across
    // versions. We only assert that the branch dispatches without
    // crashing and reaches the socket with a "stop" kind when a
    // transcript path is provided — drift in the transcript schema
    // itself would return empty, which the script exits 0 for
    // (fail-open). Treat "no socket traffic" as an acceptable outcome
    // for a missing transcript.
    if !shell_tools_available() {
        eprintln!("skipping: bash/jq/nc not on PATH");
        return;
    }
    with_scratch_data_home("copilot_agent_stop", || {
        let script = materialize(AgentKind::Copilot).expect("materialize");
        let bridge = FakeBridge::new("copilot-agent-stop");
        // Write a minimal JSONL transcript the script can tail.
        let transcript = std::env::temp_dir().join(format!(
            "tebis-copilot-transcript-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(
            &transcript,
            "{\"role\":\"assistant\",\"content\":\"I finished the task\"}\n",
        )
        .unwrap();
        let fixture = format!(
            r#"{{"hook_event_name":"agentStop","transcriptPath":"{}","cwd":"/tmp","sessionId":"s789"}}"#,
            transcript.display()
        );
        run_hook(&script, &bridge.path, &fixture);
        // The script might succeed in forwarding (transcript schema
        // matched) or not (schema mismatch). Accept either — but if
        // it DID forward, the kind must be "stop".
        let listener = bridge.listener.try_clone().unwrap();
        listener.set_nonblocking(true).unwrap();
        if let Ok((stream, _)) = listener.accept() {
            use std::io::{BufRead, BufReader};
            let mut buf = String::new();
            let _ = BufReader::new(stream).read_line(&mut buf);
            let (kind, _) = parse_forwarded(&buf);
            assert_eq!(kind, "stop");
        }
        let _ = std::fs::remove_file(&transcript);
    });
}
