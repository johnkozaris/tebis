//! Command parsing and execution.
//!
//! Split into three layers:
//!
//! - **Parser** ([`parse`]) — pure: `&str` → [`Command`]. No I/O, no deps.
//! - **Executor** ([`execute`]) — glues commands to [`crate::tmux::Tmux`]
//!   and [`crate::session::SessionState`], converts tmux errors into a
//!   Telegram-safe [`Response`].
//! - **State** — owned by [`crate::session::SessionState`], not this module.
//!
//! Recovery path: when a tmux operation on the default target fails with
//! [`crate::tmux::TmuxError::NotFound`], the executor clears the stale
//! default via [`SessionState::clear_target_if`]. For plain-text messages
//! it also retries once via autostart so the user's message lands in a
//! fresh session — this is the "phone drove Claude, Claude exited between
//! messages" case, and making it self-heal beats leaving the user to
//! debug tmux.

use std::collections::HashSet;
use std::time::Instant;

use crate::sanitize;
use crate::session::SessionState;
use crate::tmux::Tmux;

/// Parsed command from a Telegram message.
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
    PlainText(String),
}

/// Response shape. `Text` sends a chat reply; `ReactSuccess` reacts 👍 to
/// the originating message — lighter UX for fire-and-forget commands.
pub enum Response {
    Text(String),
    ReactSuccess,
}

/// Dependencies the executor needs. Kept small so `execute` stays the thin
/// seam between "parsed command" and "tmux side effect + response".
pub struct Deps<'a> {
    pub tmux: &'a Tmux,
    pub session: &'a SessionState,
    /// Process start instant — used by `/status` to report uptime. Copy-cheap
    /// so we pass by value rather than shared state.
    pub started_at: Instant,
}

/// Strip a command prefix (`/cmd `) with either a space or tab separator,
/// returning the remainder trimmed of leading whitespace. Returns `None`
/// if the text doesn't start with `/cmd` followed by a separator — so
/// `/newt` won't be mistaken for `/new t`.
fn strip_cmd<'a>(text: &'a str, cmd: &str) -> Option<&'a str> {
    let after = text.strip_prefix(cmd)?;
    let rest = after
        .strip_prefix(' ')
        .or_else(|| after.strip_prefix('\t'))?;
    Some(rest)
}

/// Accept a session name the user typed into a command. Lenient: if the
/// user copy-pasted `=NAME` out of a tmux error message, strip the prefix
/// before it hits `security::is_valid_session_name` (which rejects `=`).
/// The bare name is what every other caller expects.
fn normalize_session_arg(raw: &str) -> String {
    raw.strip_prefix('=').unwrap_or(raw).to_string()
}

/// Parse a message into a command.
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

    if text.starts_with('/') {
        return Command::Help;
    }

    Command::PlainText(text.to_string())
}

/// Execute a command. Errors become a Telegram-safe text response — the
/// message content is HTML-escaped so a `<` in a tmux stderr can't break
/// `parse_mode=HTML` and cause Telegram to reject the reply.
pub async fn execute(cmd: Command, deps: &Deps<'_>) -> Response {
    match handle(cmd, deps).await {
        Ok(r) => r,
        Err(e) => Response::Text(format!("Error: {}", sanitize::escape_html(&e))),
    }
}

/// Inner error type — `String` lets us collapse `TmuxError` /
/// `ResolveError` into a single shape before it hits the Telegram layer.
/// The Display strings are already user-safe (no `=` prefix, no tokens).
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
        Command::PlainText(text) => plain_text(deps, &text).await,
    }
}

async fn list(deps: &Deps<'_>) -> HandleResult {
    let live = deps.tmux.list_sessions().await.map_err(|e| e.to_string())?;
    if live.is_empty() {
        return Ok(Response::Text("No active tmux sessions.".to_string()));
    }

    // In permissive mode (empty allowlist) every valid name is touchable,
    // so every live session gets the ✓. In strict mode the set marks
    // which ones we can `/send` / `/read` / `/kill`.
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
    match deps.tmux.send_keys(session, text).await {
        Ok(()) => Ok(Response::ReactSuccess),
        Err(e) => {
            if e.is_not_found() {
                deps.session.clear_target_if(session);
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
    // Cap at 5000 lines. `sanitize::sanitize_tmux_output` eventually
    // truncates to `max_output_chars`, so huge values only cost a slower
    // capture subprocess, but the bound is cheap insurance against a
    // user typing `/read sess 99999999`.
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
    // `kill_session` is idempotent on NotFound at the tmux layer; any error
    // here is a real one. Always clear the default target if it matched —
    // the user's intent is unambiguous.
    let result = deps.tmux.kill_session(session).await;
    deps.session.clear_target_if(session);
    result.map_err(|e| e.to_string())?;
    Ok(Response::ReactSuccess)
}

/// `/status` — snapshot of bridge state the user would otherwise have to
/// deduce from error messages. HTML-escaped so a session name with an
/// HTML-special char can't break `parse_mode=HTML`.
fn status(deps: &Deps<'_>) -> String {
    let target = deps
        .session
        .target()
        .unwrap_or_else(|| "(none)".to_string());
    let autostart = deps
        .session
        .autostart_session()
        .unwrap_or("(not configured)");
    // Permissive (empty) mode reports "(any)" so the user sees it's not
    // an error state — distinct from strict mode with an empty list.
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

/// `/restart` — kill the autostart session and drop the cached target so
/// the next plain-text message re-provisions. Explicit, fast way to
/// recover from "Claude hung, restart please" without the user having to
/// `/kill` + send a new message.
async fn restart(deps: &Deps<'_>) -> HandleResult {
    let Some(name) = deps.session.autostart_session() else {
        return Err("No autostart session configured; nothing to restart.".to_string());
    };
    let name = name.to_string(); // detach lifetime from SessionState
    deps.tmux
        .kill_session(&name)
        .await
        .map_err(|e| e.to_string())?;
    deps.session.clear_target_if(&name);
    Ok(Response::Text(format!(
        "Killed <code>{}</code>. Next plain-text message will re-provision.",
        sanitize::escape_html(&name)
    )))
}

/// Plain-text path: resolve (or autostart) a target, send to it. If the
/// resolved session turns out to be stale (tmux says `NotFound`), clear the
/// cached default, drain any zombie state, and retry once with fresh
/// provisioning. One retry only — good enough for the transient-death
/// case (Claude exited between messages) without risking loops if
/// autostart itself keeps failing.
///
/// The retry explicitly calls `kill_session` (idempotent) before
/// reprovisioning. tmux's teardown of a dying session can briefly lag,
/// during which `has_session` returns `true` even though `send_keys`
/// fails. Killing first guarantees the subsequent `has_session` check
/// inside `resolve_or_autostart` returns `false`, so we always
/// re-provision — no chance of replaying the same failure.
async fn plain_text(deps: &Deps<'_>, text: &str) -> HandleResult {
    let session = resolve_or_autostart_str(deps).await?;
    match deps.tmux.send_keys(&session, text).await {
        Ok(()) => Ok(Response::ReactSuccess),
        Err(e) if e.is_not_found() => {
            deps.session.clear_target_if(&session);
            // Drain any zombie tmux state before reprovisioning.
            // `kill_session` is idempotent on NotFound, so an already-
            // gone session is a no-op.
            let _ = deps.tmux.kill_session(&session).await;
            let fresh = resolve_or_autostart_str(deps).await?;
            deps.tmux
                .send_keys(&fresh, text)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Response::ReactSuccess)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Thin wrapper that flattens `ResolveError` into the `String` error channel
/// `handle` uses. Factored because `plain_text` calls it twice (initial + retry).
async fn resolve_or_autostart_str(deps: &Deps<'_>) -> Result<String, String> {
    deps.session
        .resolve_or_autostart(deps.tmux)
        .await
        .map_err(|e| e.to_string())
}

/// Render uptime into a short human-friendly form (e.g. `3d 4h 12m`).
/// Never zero-pads (so `5m 3s` reads naturally). Always shows seconds so
/// newly-started bridges don't say just `0m`.
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

// ---------- help text ----------

const HELP_BASE: &str = concat!(
    "<b>Commands:</b>\n",
    "/list — list tmux sessions (✓ = allowlisted)\n",
    "/status — show bridge state\n",
    "/send &lt;session&gt; &lt;text&gt; — send text to session\n",
    "/read [session] [lines] — read pane output\n",
    "/target &lt;session&gt; — set default session\n",
    "/new &lt;session&gt; — create an empty detached tmux session\n",
    "/kill &lt;session&gt; — kill a tmux session\n",
    "/restart — kill autostart session, re-provision on next message\n",
    "/help — show this help\n\n",
    "Plain text is sent to the default target session. If autostart is ",
    "configured and no target is set, the first plain-text message ",
    "auto-provisions a session running the configured command ",
    "(e.g. <code>claude</code>) in the configured directory.",
);

// Keep the static base under the Telegram 4096-char ceiling with slack.
// If this fails, someone added too much to HELP_BASE.
const _: () = assert!(
    HELP_BASE.len() < 3800,
    "HELP_BASE must leave room for autostart suffix + Telegram's 4096-char send cap"
);

/// Render help text with the concrete autostart session name appended
/// (if configured). Rendered per-call so `/help` always reflects current
/// config — cheap because it's just a `String::from` + `push_str`.
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

    /// Helper — match on the shape of a parsed command so assertions read
    /// like expectations rather than pattern acrobatics.
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
        // `/send sessname` alone (no trailing text) isn't a valid Send —
        // we fall through to the catch-all `/cmd → Help` at the bottom.
        assert_eq!(kind(&parse("/send mysession")), "help");
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
        // Regression: bare-prefix matching would incorrectly treat
        // `/newt` as `/new t`.
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
        // Users who see `=demo` in a tmux error message (e.g.
        // `duplicate session: =demo` from new-session) should be able
        // to paste it back into /kill without hitting the validator.
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
        // Name can't actually contain `<` (allowlist rejects it), but the
        // escape path must still be HTML-safe in case of future regex
        // relaxation.
        let out = help_text(Some("a&b"));
        assert!(out.contains("a&amp;b"));
    }
}
