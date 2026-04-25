//! Command parser + executor. Stale-target recovery: `NotFound` clears the
//! cached default; plain-text retries once via autostart.

use std::collections::HashSet;
use std::time::Instant;

use super::session::SessionState;
use crate::platform::multiplexer::Mux;
use crate::sanitize;

pub enum Command {
    List,
    Send {
        session: String,
        text: String,
    },
    Read {
        session: Option<String>,
        lines: Option<usize>,
    },
    Target {
        session: String,
    },
    New {
        session: String,
    },
    Kill {
        session: String,
    },
    Status,
    Restart,
    Help,
    /// TTS backend selection — handled by `bridge::handle_update`, not
    /// `execute`, because writing the env file + graceful restart needs
    /// `HandlerContext` fields that `Deps` doesn't carry.
    Tts(TtsVerb),
    PlainText(String),
}

/// What the user asked `/tts` to do. `Unknown` carries the raw argument
/// so the reply can point at the usage line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsVerb {
    Status,
    Off,
    Say,
    WinRt,
    KokoroLocal,
    KokoroRemote,
    Unknown(String),
}

/// `Text`: reply body. `ReactSuccess`: 👍. `Sent`: text delivered to a
/// session, routed to typing+autoreply or 👍 by `handle_update`.
/// `baseline` is the pre-send pane capture for diff-based autoreply.
pub enum Response {
    Text(String),
    ReactSuccess,
    Sent {
        session: String,
        baseline: Option<String>,
    },
}

pub struct Deps<'a> {
    pub tmux: &'a Mux,
    pub session: &'a SessionState,
    pub started_at: Instant,
}

/// Returns `None` on `/cmdmore` so `/newt` isn't mistaken for `/new t`.
fn strip_cmd<'a>(text: &'a str, cmd: &str) -> Option<&'a str> {
    let after = text.strip_prefix(cmd)?;
    let rest = after
        .strip_prefix(' ')
        .or_else(|| after.strip_prefix('\t'))?;
    Some(rest)
}

/// Strip `=` so users can paste `=NAME` from tmux errors.
fn normalize_session_arg(raw: &str) -> String {
    raw.strip_prefix('=').unwrap_or(raw).to_string()
}

pub fn parse(text: &str) -> Command {
    let text = text.trim();

    if text.eq_ignore_ascii_case("/list") {
        return Command::List;
    }
    if text.eq_ignore_ascii_case("/status") {
        return Command::Status;
    }
    if text.eq_ignore_ascii_case("/restart") {
        return Command::Restart;
    }
    if text.eq_ignore_ascii_case("/help") || text.eq_ignore_ascii_case("/start") {
        return Command::Help;
    }

    if let Some(rest) = strip_cmd(text, "/send") {
        let rest = rest.trim();
        if let Some(space_pos) = rest.find(|c: char| c.is_whitespace()) {
            let session = normalize_session_arg(&rest[..space_pos]);
            let msg = rest[space_pos..].trim().to_string();
            return Command::Send { session, text: msg };
        }
    }

    if let Some(rest) = text
        .strip_prefix("/read")
        .filter(|r| r.is_empty() || r.starts_with(' ') || r.starts_with('\t'))
    {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        let session = parts.first().map(|s| normalize_session_arg(s));
        let lines = parts.get(1).and_then(|s| s.parse().ok());
        return Command::Read { session, lines };
    }

    if let Some(rest) = strip_cmd(text, "/target") {
        let session = normalize_session_arg(rest.trim());
        if !session.is_empty() {
            return Command::Target { session };
        }
    }

    if let Some(rest) = strip_cmd(text, "/new") {
        let session = normalize_session_arg(rest.trim());
        if !session.is_empty() {
            return Command::New { session };
        }
    }

    if let Some(rest) = strip_cmd(text, "/kill") {
        let session = normalize_session_arg(rest.trim());
        if !session.is_empty() {
            return Command::Kill { session };
        }
    }

    // `/tts` — backend picker. `/tts` alone (or `/tts status`) reports;
    // `/tts {off,say,winrt,kokoro-local,kokoro-remote}` switches (via env-file
    // write + graceful restart in `bridge::handle_update`).
    if text.eq_ignore_ascii_case("/tts") {
        return Command::Tts(TtsVerb::Status);
    }
    if let Some(rest) = strip_cmd(text, "/tts") {
        let verb = match rest.trim().to_ascii_lowercase().as_str() {
            "" | "status" => TtsVerb::Status,
            "off" | "none" | "disable" | "disabled" => TtsVerb::Off,
            "say" => TtsVerb::Say,
            "winrt" => TtsVerb::WinRt,
            "kokoro-local" | "kokoro_local" | "local" => TtsVerb::KokoroLocal,
            "kokoro-remote" | "kokoro_remote" | "remote" => TtsVerb::KokoroRemote,
            other => TtsVerb::Unknown(other.to_string()),
        };
        return Command::Tts(verb);
    }

    if text.starts_with('/') {
        return Command::Help;
    }

    Command::PlainText(text.to_string())
}

pub async fn execute(cmd: Command, deps: &Deps<'_>) -> Response {
    match handle(cmd, deps).await {
        Ok(r) => r,
        Err(e) => Response::Text(format!("Error: {}", sanitize::escape_html(&e))),
    }
}

type HandleResult = Result<Response, String>;

async fn handle(cmd: Command, deps: &Deps<'_>) -> HandleResult {
    match cmd {
        Command::List => list(deps).await,
        Command::Send { session, text } => send(deps, &session, &text).await,
        Command::Read { session, lines } => read(deps, session, lines).await,
        Command::Target { session } => target(deps, session),
        Command::New { session } => new(deps, &session).await,
        Command::Kill { session } => kill(deps, &session).await,
        Command::Status => Ok(Response::Text(status(deps))),
        Command::Restart => restart(deps).await,
        Command::Help => Ok(Response::Text(help_text(deps.session.autostart_session()))),
        // `/tts` is intercepted in `bridge::handle_update` — needs env-file
        // path + shutdown token from `HandlerContext`, not `Deps`.
        Command::Tts(_) => Ok(Response::Text(
            "internal: /tts should be intercepted upstream".to_string(),
        )),
        Command::PlainText(text) => plain_text(deps, &text).await,
    }
}

async fn list(deps: &Deps<'_>) -> HandleResult {
    let live = deps.tmux.list_sessions().await.map_err(|e| e.to_string())?;
    if live.is_empty() {
        return Ok(Response::Text(
            "No active multiplexer sessions.".to_string(),
        ));
    }

    let permissive = deps.tmux.is_permissive();
    let allowed = deps.tmux.allowlisted_sessions();
    let allowed_set: HashSet<&str> = allowed.iter().map(String::as_str).collect();

    let lines: Vec<String> = live
        .iter()
        .map(|s| {
            let marker = if permissive || allowed_set.contains(s.as_str()) {
                "✓"
            } else {
                "✗"
            };
            format!("{marker} {s}")
        })
        .collect();

    let body = sanitize::escape_html(&lines.join("\n"));
    Ok(Response::Text(sanitize::wrap_and_truncate(
        &body, "<pre>", "</pre>",
    )))
}

async fn send(deps: &Deps<'_>, session: &str, text: &str) -> HandleResult {
    // Pre-send baseline so autoreply can forward only new content.
    let baseline = match deps.tmux.capture_pane(session, 100).await {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::debug!(
                err = %e, session,
                "baseline capture_pane failed — autoreply will fall back to pane tail"
            );
            None
        }
    };
    match deps.tmux.send_keys(session, text).await {
        Ok(()) => Ok(Response::Sent {
            session: session.to_string(),
            baseline,
        }),
        Err(e) => {
            if e.is_not_found() {
                deps.session.clear_target_if(session);
                deps.session.unmark_hooked(session);
            }
            Err(e.to_string())
        }
    }
}

async fn read(deps: &Deps<'_>, session: Option<String>, lines: Option<usize>) -> HandleResult {
    let session = deps
        .session
        .resolve_explicit(session)
        .map_err(|e| e.to_string())?;
    let lines = lines.unwrap_or(50).min(5_000);
    let output = match deps.tmux.capture_pane(&session, lines).await {
        Ok(o) => o,
        Err(e) => {
            if e.is_not_found() {
                deps.session.clear_target_if(&session);
            }
            return Err(e.to_string());
        }
    };
    if output.trim().is_empty() {
        return Ok(Response::Text(format!(
            "<code>{}</code>: (empty pane)",
            sanitize::escape_html(&session)
        )));
    }
    let escaped = sanitize::escape_html(&output);
    Ok(Response::Text(sanitize::wrap_and_truncate(
        &escaped, "<pre>", "</pre>",
    )))
}

fn target(deps: &Deps<'_>, session: String) -> HandleResult {
    deps.tmux
        .validate_session(&session)
        .map_err(|e| e.to_string())?;
    deps.session.set_target(session);
    Ok(Response::ReactSuccess)
}

async fn new(deps: &Deps<'_>, session: &str) -> HandleResult {
    deps.tmux
        .new_session(session, None, None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Response::ReactSuccess)
}

async fn kill(deps: &Deps<'_>, session: &str) -> HandleResult {
    // Validate + execute first, mutate state only on success. `kill_session`
    // is idempotent (NotFound → Ok), so "already gone" still reaches the
    // clear + unmark below. Mutating before validation was a no-op for
    // invalid-name input (both helpers are no-ops on non-matches), but
    // side-effects-before-check is a fragile pattern.
    deps.tmux
        .kill_session(session)
        .await
        .map_err(|e| e.to_string())?;
    deps.session.clear_target_if(session);
    deps.session.unmark_hooked(session);
    Ok(Response::ReactSuccess)
}

fn status(deps: &Deps<'_>) -> String {
    let target = deps
        .session
        .target()
        .unwrap_or_else(|| "(none)".to_string());
    let autostart = deps
        .session
        .autostart_session()
        .unwrap_or("(not configured)");
    let allowlist = if deps.tmux.is_permissive() {
        "(any)".to_string()
    } else {
        deps.tmux.allowlisted_sessions().join(", ")
    };
    let uptime = format_uptime(deps.started_at.elapsed());

    let body = format!(
        "target:    {target}\nautostart: {autostart}\nallowlist: {allowlist}\nuptime:    {uptime}"
    );
    format!("<pre>{}</pre>", sanitize::escape_html(&body))
}

async fn restart(deps: &Deps<'_>) -> HandleResult {
    let Some(name) = deps.session.autostart_session() else {
        return Err("No autostart session configured; nothing to restart.".to_string());
    };
    let name = name.to_string();
    deps.tmux
        .kill_session(&name)
        .await
        .map_err(|e| e.to_string())?;
    deps.session.clear_target_if(&name);
    deps.session.unmark_hooked(&name);
    Ok(Response::Text(format!(
        "Killed <code>{}</code>. Next plain-text message will re-provision.",
        sanitize::escape_html(&name)
    )))
}

/// Resolve-or-autostart + send; on `NotFound` kill + reprovision + retry once.
async fn plain_text(deps: &Deps<'_>, text: &str) -> HandleResult {
    let session = resolve_or_autostart_str(deps).await?;
    let baseline = match deps.tmux.capture_pane(&session, 100).await {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::debug!(
                err = %e, session,
                "plain_text baseline capture failed — autoreply will fall back to pane tail"
            );
            None
        }
    };
    match deps.tmux.send_keys(&session, text).await {
        Ok(()) => Ok(Response::Sent { session, baseline }),
        Err(e) if e.is_not_found() => {
            deps.session.clear_target_if(&session);
            if let Err(kill_err) = deps.tmux.kill_session(&session).await {
                tracing::debug!(
                    err = %kill_err, session,
                    "plain_text retry: kill_session drain failed (usually harmless — session was already gone)"
                );
            }
            let fresh = resolve_or_autostart_str(deps).await?;
            deps.tmux
                .send_keys(&fresh, text)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Response::Sent {
                session: fresh,
                baseline: None,
            })
        }
        Err(e) => Err(e.to_string()),
    }
}

async fn resolve_or_autostart_str(deps: &Deps<'_>) -> Result<String, String> {
    deps.session
        .resolve_or_autostart(deps.tmux)
        .await
        .map_err(|e| e.to_string())
}

fn format_uptime(d: std::time::Duration) -> String {
    use std::fmt::Write as _;
    let mut s = d.as_secs();
    let days = s / 86_400;
    s %= 86_400;
    let hours = s / 3_600;
    s %= 3_600;
    let mins = s / 60;
    let secs = s % 60;
    let mut out = String::new();
    if days > 0 {
        let _ = write!(out, "{days}d ");
    }
    if hours > 0 || days > 0 {
        let _ = write!(out, "{hours}h ");
    }
    if mins > 0 || hours > 0 || days > 0 {
        let _ = write!(out, "{mins}m ");
    }
    let _ = write!(out, "{secs}s");
    out
}

const HELP_BASE: &str = concat!(
    "<b>Commands:</b>\n",
    "/list — list multiplexer sessions (✓ = allowlisted)\n",
    "/status — show bridge state\n",
    "/send &lt;session&gt; &lt;text&gt; — send text to session\n",
    "/read [session] [lines] — read pane output\n",
    "/target &lt;session&gt; — set default session\n",
    "/new &lt;session&gt; — create an empty detached multiplexer session\n",
    "/kill &lt;session&gt; — kill a multiplexer session\n",
    "/restart — kill autostart session, re-provision on next message\n",
    "/tts [off|say|winrt|kokoro-local|kokoro-remote|status] — pick or disable voice replies\n",
    "/help — show this help\n\n",
    "Plain text is sent to the default target session. If autostart is ",
    "configured and no target is set, the first plain-text message ",
    "auto-provisions a session running the configured command ",
    "(e.g. <code>claude</code>) in the configured directory.",
);

// Keep HELP_BASE well under Telegram's 4096-char ceiling.
const _: () = assert!(HELP_BASE.len() < 3800, "HELP_BASE too long");

fn help_text(autostart_session: Option<&str>) -> String {
    let mut out = HELP_BASE.to_string();
    if let Some(name) = autostart_session {
        out.push_str("\n\nAutostart session: <code>");
        out.push_str(&sanitize::escape_html(name));
        out.push_str("</code>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(cmd: &Command) -> &'static str {
        match cmd {
            Command::List => "list",
            Command::Send { .. } => "send",
            Command::Read { .. } => "read",
            Command::Target { .. } => "target",
            Command::New { .. } => "new",
            Command::Kill { .. } => "kill",
            Command::Status => "status",
            Command::Restart => "restart",
            Command::Help => "help",
            Command::Tts(_) => "tts",
            Command::PlainText(_) => "plain",
        }
    }

    #[test]
    fn list_is_case_insensitive() {
        assert_eq!(kind(&parse("/list")), "list");
        assert_eq!(kind(&parse("/LIST")), "list");
        assert_eq!(kind(&parse("  /list  ")), "list");
    }

    #[test]
    fn start_is_aliased_to_help() {
        assert_eq!(kind(&parse("/start")), "help");
        assert_eq!(kind(&parse("/help")), "help");
    }

    #[test]
    fn status_and_restart_parse() {
        assert_eq!(kind(&parse("/status")), "status");
        assert_eq!(kind(&parse("/STATUS")), "status");
        assert_eq!(kind(&parse("/restart")), "restart");
        assert_eq!(kind(&parse("/RESTART")), "restart");
    }

    #[test]
    fn send_splits_session_and_text() {
        match parse("/send mysession hello world") {
            Command::Send { session, text } => {
                assert_eq!(session, "mysession");
                assert_eq!(text, "hello world");
            }
            c => panic!("expected Send, got {}", kind(&c)),
        }
    }

    #[test]
    fn send_without_text_falls_through_to_help() {
        assert_eq!(kind(&parse("/send mysession")), "help");
    }

    #[test]
    fn tts_verbs_parse() {
        fn verb(cmd: &Command) -> &TtsVerb {
            match cmd {
                Command::Tts(v) => v,
                c => panic!("expected Tts, got {}", kind(c)),
            }
        }
        assert_eq!(verb(&parse("/tts")), &TtsVerb::Status);
        assert_eq!(verb(&parse("/TTS")), &TtsVerb::Status);
        assert_eq!(verb(&parse("/tts status")), &TtsVerb::Status);
        assert_eq!(verb(&parse("/tts off")), &TtsVerb::Off);
        assert_eq!(verb(&parse("/tts OFF")), &TtsVerb::Off);
        assert_eq!(verb(&parse("/tts none")), &TtsVerb::Off);
        assert_eq!(verb(&parse("/tts disable")), &TtsVerb::Off);
        assert_eq!(verb(&parse("/tts say")), &TtsVerb::Say);
        assert_eq!(verb(&parse("/tts winrt")), &TtsVerb::WinRt);
        assert_eq!(verb(&parse("/tts kokoro-local")), &TtsVerb::KokoroLocal);
        assert_eq!(verb(&parse("/tts kokoro_local")), &TtsVerb::KokoroLocal);
        assert_eq!(verb(&parse("/tts local")), &TtsVerb::KokoroLocal);
        assert_eq!(verb(&parse("/tts kokoro-remote")), &TtsVerb::KokoroRemote);
        assert_eq!(verb(&parse("/tts remote")), &TtsVerb::KokoroRemote);
        // Unknown verbs round-trip their arg so the reply can cite it.
        match verb(&parse("/tts banana")) {
            TtsVerb::Unknown(arg) => assert_eq!(arg, "banana"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn ttsextra_suffix_is_not_tts() {
        // `/ttsextra` must not be mistaken for `/tts` with arg `extra`.
        // strip_cmd requires a whitespace separator; without one we hit
        // the generic `/` fallback → Help.
        assert_eq!(kind(&parse("/ttsextra")), "help");
    }

    #[test]
    fn read_accepts_optional_args() {
        match parse("/read") {
            Command::Read { session, lines } => {
                assert!(session.is_none());
                assert!(lines.is_none());
            }
            c => panic!("{}", kind(&c)),
        }
        match parse("/read sess 100") {
            Command::Read { session, lines } => {
                assert_eq!(session.as_deref(), Some("sess"));
                assert_eq!(lines, Some(100));
            }
            c => panic!("{}", kind(&c)),
        }
    }

    #[test]
    fn target_requires_arg() {
        match parse("/target sess") {
            Command::Target { session } => assert_eq!(session, "sess"),
            c => panic!("{}", kind(&c)),
        }
        assert_eq!(kind(&parse("/target")), "help");
        assert_eq!(kind(&parse("/target ")), "help");
    }

    #[test]
    fn new_and_kill_require_arg() {
        match parse("/new foo") {
            Command::New { session } => assert_eq!(session, "foo"),
            c => panic!("{}", kind(&c)),
        }
        match parse("/kill foo") {
            Command::Kill { session } => assert_eq!(session, "foo"),
            c => panic!("{}", kind(&c)),
        }
        assert_eq!(kind(&parse("/new")), "help");
        assert_eq!(kind(&parse("/kill")), "help");
    }

    #[test]
    fn newt_is_not_new() {
        assert_eq!(kind(&parse("/newt")), "help");
        assert_eq!(kind(&parse("/killr anything")), "help");
    }

    #[test]
    fn unknown_slash_falls_through_to_help() {
        assert_eq!(kind(&parse("/bogus")), "help");
        assert_eq!(kind(&parse("/ ")), "help");
    }

    #[test]
    fn plain_text_passes_through_trimmed() {
        match parse("  hello there  ") {
            Command::PlainText(s) => assert_eq!(s, "hello there"),
            c => panic!("{}", kind(&c)),
        }
    }

    #[test]
    fn tab_separator_accepted() {
        match parse("/send\tsess\thello") {
            Command::Send { session, text } => {
                assert_eq!(session, "sess");
                assert_eq!(text, "hello");
            }
            c => panic!("{}", kind(&c)),
        }
    }

    #[test]
    fn equals_prefix_stripped_from_user_args() {
        match parse("/kill =demo") {
            Command::Kill { session } => assert_eq!(session, "demo"),
            c => panic!("{}", kind(&c)),
        }
        match parse("/target =foo") {
            Command::Target { session } => assert_eq!(session, "foo"),
            c => panic!("{}", kind(&c)),
        }
        match parse("/new =bar") {
            Command::New { session } => assert_eq!(session, "bar"),
            c => panic!("{}", kind(&c)),
        }
        match parse("/send =foo hi") {
            Command::Send { session, text } => {
                assert_eq!(session, "foo");
                assert_eq!(text, "hi");
            }
            c => panic!("{}", kind(&c)),
        }
        match parse("/read =foo 10") {
            Command::Read { session, lines } => {
                assert_eq!(session.as_deref(), Some("foo"));
                assert_eq!(lines, Some(10));
            }
            c => panic!("{}", kind(&c)),
        }
    }

    #[test]
    fn format_uptime_shapes() {
        use std::time::Duration;
        assert_eq!(format_uptime(Duration::from_secs(0)), "0s");
        assert_eq!(format_uptime(Duration::from_secs(5)), "5s");
        assert_eq!(format_uptime(Duration::from_secs(65)), "1m 5s");
        assert_eq!(format_uptime(Duration::from_secs(3_661)), "1h 1m 1s");
        assert_eq!(format_uptime(Duration::from_secs(86_461)), "1d 0h 1m 1s");
    }

    #[test]
    fn help_text_appends_autostart_when_configured() {
        let without = help_text(None);
        let with = help_text(Some("demo"));
        assert!(!without.contains("Autostart session:"));
        assert!(with.contains("Autostart session:"));
        assert!(with.contains("demo"));
    }

    #[test]
    fn help_text_escapes_autostart_name() {
        let out = help_text(Some("a&b"));
        assert!(out.contains("a&amp;b"));
    }
}
