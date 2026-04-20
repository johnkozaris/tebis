//! HTTP accept loop, routing, action endpoints, CSRF, env I/O.

use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::{LiveContext, Snapshot, render};
use crate::env_file;

/// Origins a legitimate same-host browser POST could carry.
pub(super) fn expected_origins_for(port: u16) -> Vec<String> {
    vec![
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ]
}

pub(super) async fn accept_loop(
    listener: TcpListener,
    shutdown: CancellationToken,
    snapshot: Arc<Snapshot>,
    live: Arc<LiveContext>,
    expected_origins: Arc<Vec<String>>,
    _tracker: TaskTracker,
) {
    loop {
        let (stream, _peer) = tokio::select! {
            res = listener.accept() => match res {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(err = %e, "inspect: accept failed");
                    continue;
                }
            },
            () = shutdown.cancelled() => {
                tracing::info!("Inspect dashboard shutting down");
                return;
            }
        };
        let io = TokioIo::new(stream);
        let snapshot = snapshot.clone();
        let live = live.clone();
        let expected_origins = expected_origins.clone();
        let conn_shutdown = shutdown.clone();
        // Per-connection tasks are NOT tracked: a browser with the
        // dashboard open holds a keep-alive connection that would stall
        // the shutdown drain for its full timeout. These tasks serve
        // non-critical HTML — fine to drop on Ctrl-C. On shutdown we
        // also race serve_connection against the cancel token so the
        // handler stops accepting new requests on the same connection.
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let snapshot = snapshot.clone();
                let live = live.clone();
                let expected_origins = expected_origins.clone();
                async move { handle(req, snapshot, live, expected_origins).await }
            });
            let serve = http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(io, service);
            tokio::pin!(serve);
            tokio::select! {
                res = &mut serve => {
                    if let Err(e) = res {
                        tracing::debug!(err = %e, "inspect: connection ended");
                    }
                }
                () = conn_shutdown.cancelled() => {
                    serve.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        serve,
                    ).await;
                }
            }
        });
    }
}

async fn handle(
    req: Request<Incoming>,
    snapshot: Arc<Snapshot>,
    live: Arc<LiveContext>,
    expected_origins: Arc<Vec<String>>,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path().to_string();
    match (req.method(), path.as_str()) {
        (&Method::GET, "/" | "/index.html") => {
            let body = render::html(&snapshot, &live).await;
            Ok(html_response(body))
        }
        (&Method::GET, "/status") => {
            let body = render::json(&snapshot, &live).await;
            Ok(json_response(body))
        }
        (&Method::POST, "/actions/kill-all-sessions") => {
            if !origin_is_trusted(&req, &expected_origins) {
                return Ok(text_response(StatusCode::FORBIDDEN, "forbidden\n"));
            }
            let killed = kill_all(&live).await;
            tracing::warn!(count = killed, "Inspect: killed all allowlisted sessions");
            Ok(redirect_to("/"))
        }
        (&Method::POST, "/actions/restart") => {
            if !origin_is_trusted(&req, &expected_origins) {
                return Ok(text_response(StatusCode::FORBIDDEN, "forbidden\n"));
            }
            tracing::warn!("Inspect: restart requested");
            schedule_graceful_restart(&live.shutdown);
            Ok(redirect_to("/"))
        }
        (&Method::POST, "/actions/config") => {
            if !origin_is_trusted(&req, &expected_origins) {
                return Ok(text_response(StatusCode::FORBIDDEN, "forbidden\n"));
            }
            handle_config_post(req, &snapshot, &live).await
        }
        (&Method::POST, p) if p.starts_with("/actions/kill/") => {
            if !origin_is_trusted(&req, &expected_origins) {
                return Ok(text_response(StatusCode::FORBIDDEN, "forbidden\n"));
            }
            let name = p.strip_prefix("/actions/kill/").unwrap_or("");
            // Strict mode: must be in the allowlist. Permissive mode: any
            // valid-name live session is fair game — `kill_session` itself
            // enforces the name regex, so we just fall through.
            if !live.tmux.is_permissive()
                && !live.tmux.allowlisted_sessions().iter().any(|s| s == name)
            {
                return Ok(text_response(
                    StatusCode::NOT_FOUND,
                    "session not in allowlist\n",
                ));
            }
            // `kill_session` is idempotent (NotFound → Ok) and validates
            // the name regex itself, so a bogus URL suffix like
            // `/actions/kill/../etc` is rejected at the tmux layer.
            let _ = live.tmux.kill_session(name).await;
            live.session.clear_target_if(name);
            tracing::warn!(session = name, "Inspect: killed session");
            Ok(redirect_to("/"))
        }
        (&Method::GET, _) => Ok(text_response(StatusCode::NOT_FOUND, "not found\n")),
        _ => Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed\n",
        )),
    }
}

async fn kill_all(live: &LiveContext) -> usize {
    // In strict mode, only the pre-declared allowlist is touchable. In
    // permissive mode, every live session is touchable, so we iterate
    // `list_sessions` instead. Either way `kill_session` enforces the
    // name regex, so malformed names from a weird tmux output don't
    // reach argv.
    let targets: Vec<String> = if live.tmux.is_permissive() {
        live.tmux.list_sessions().await.unwrap_or_default()
    } else {
        live.tmux.allowlisted_sessions()
    };
    let mut killed = 0;
    for name in targets {
        if live.tmux.kill_session(&name).await.is_ok() {
            killed += 1;
        }
        live.session.clear_target_if(&name);
    }
    killed
}

async fn handle_config_post(
    req: Request<Incoming>,
    snapshot: &Snapshot,
    live: &LiveContext,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    let Some(env_file) = snapshot.env_file.clone() else {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "config editing disabled — set BRIDGE_ENV_FILE\n",
        ));
    };
    let body = match Limited::new(req.into_body(), 4096).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            return Ok(text_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large\n",
            ));
        }
    };
    let updates = match parse_config_form(&body) {
        Ok(u) if !u.is_empty() => u,
        Ok(_) => {
            return Ok(text_response(
                StatusCode::BAD_REQUEST,
                "no valid settings in request\n",
            ));
        }
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, msg)),
    };
    if let Err(e) = write_env_file(Path::new(&env_file), &updates) {
        tracing::error!(err = %e, "Inspect: env file write failed");
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to write env file\n",
        ));
    }
    tracing::warn!(
        fields = updates.len(),
        path = %env_file,
        "Inspect: config updated, restarting"
    );
    schedule_graceful_restart(&live.shutdown);
    Ok(redirect_to("/"))
}

/// Cancel the shared shutdown token after a short delay so the in-flight
/// HTTP response flushes to the browser before the socket closes.
fn schedule_graceful_restart(shutdown: &CancellationToken) {
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown.cancel();
    });
}

/// Parse `application/x-www-form-urlencoded` into validated env-file
/// updates. Whitelist-only (unknown keys silently ignored) and range-
/// validated server-side so a tampering client can't write `=0`. Values
/// are numeric for the fields we currently accept, so no URL decoding
/// is needed — non-numeric fails `parse::<u32>()` and gets rejected.
fn parse_config_form(
    body: &[u8],
) -> std::result::Result<Vec<(&'static str, String)>, &'static str> {
    let s = std::str::from_utf8(body).map_err(|_| "body is not valid utf-8\n")?;
    let mut poll_timeout: Option<u32> = None;
    let mut max_output_chars: Option<usize> = None;
    let mut autostart_dir: Option<String> = None;
    for kv in s.split('&') {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        match k {
            "poll_timeout" => {
                let n: u32 = v.parse().map_err(|_| "poll_timeout must be an integer\n")?;
                if !(1..=900).contains(&n) {
                    return Err("poll_timeout must be 1..=900\n");
                }
                poll_timeout = Some(n);
            }
            "max_output_chars" => {
                let n: usize = v
                    .parse()
                    .map_err(|_| "max_output_chars must be an integer\n")?;
                if !(100..=20_000).contains(&n) {
                    return Err("max_output_chars must be 100..=20000\n");
                }
                max_output_chars = Some(n);
            }
            "autostart_dir" => {
                let decoded = url_decode(v);
                if decoded.trim().is_empty() {
                    continue;
                }
                if decoded.chars().any(char::is_control) {
                    return Err("autostart_dir must not contain control characters\n");
                }
                if !Path::new(&decoded).is_dir() {
                    return Err("autostart_dir must be an existing directory\n");
                }
                autostart_dir = Some(decoded);
            }
            _ => {}
        }
    }
    let mut out: Vec<(&'static str, String)> = Vec::new();
    if let Some(n) = poll_timeout {
        out.push(("TELEGRAM_POLL_TIMEOUT", n.to_string()));
    }
    if let Some(n) = max_output_chars {
        out.push(("TELEGRAM_MAX_OUTPUT_CHARS", n.to_string()));
    }
    if let Some(d) = autostart_dir {
        out.push(("TELEGRAM_AUTOSTART_DIR", d));
    }
    Ok(out)
}

/// Minimal `application/x-www-form-urlencoded` decoder. Covers `+`
/// (space) and `%NN` (byte-hex) sequences. Broken encodings fall through
/// verbatim — the caller re-validates semantically so leftover `%`
/// characters just cause a `is_dir` rejection.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
                    && let Ok(byte) = u8::from_str_radix(hex, 16)
                {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Rewrite the env file atomically, mode 0600. Preserves comments and
/// existing line order. Keys not already in the file are appended.
///
/// The 0600 guarantee comes from [`env_file::atomic_write_0600`]; a
/// previous implementation used `fs::write` which creates under the
/// process umask (0644 on macOS by default) and silently demoted the
/// file's perms through the rename, exposing the bot token.
fn write_env_file(path: &Path, updates: &[(&'static str, String)]) -> anyhow::Result<()> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = current.lines().map(str::to_string).collect();

    for (key, value) in updates {
        let replaced = lines
            .iter_mut()
            .find(|line| env_file::parse_kv_line(line).is_some_and(|(k, _)| k == *key));
        if let Some(line) = replaced {
            *line = format!("{key}={value}");
        } else {
            lines.push(format!("{key}={value}"));
        }
    }

    let mut body = lines.join("\n");
    body.push('\n');
    env_file::atomic_write_0600(path, &body)
}

/// Origin-header CSRF check. Missing `Origin` = same-origin form POST
/// (accepted). Present but mismatched = reject.
fn origin_is_trusted(req: &Request<Incoming>, expected: &[String]) -> bool {
    let Some(origin) = req.headers().get(hyper::header::ORIGIN) else {
        return true;
    };
    let Ok(origin_str) = origin.to_str() else {
        return false;
    };
    expected.iter().any(|e| e == origin_str)
}

// ---------- response constructors ----------

fn html_response(body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .header(hyper::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(hyper::header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(body)))
        .expect("response headers are statically valid")
}

fn json_response(body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .header(
            hyper::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )
        .header(hyper::header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(body)))
        .expect("response headers are statically valid")
}

fn text_response(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(hyper::header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        .expect("response headers are statically valid")
}

fn redirect_to(location: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(hyper::header::LOCATION, location)
        .header(hyper::header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::new()))
        .expect("response headers are statically valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_origins_includes_loopback_and_localhost() {
        let origins = expected_origins_for(8080);
        assert!(origins.contains(&"http://127.0.0.1:8080".to_string()));
        assert!(origins.contains(&"http://localhost:8080".to_string()));
    }

    #[test]
    fn parse_config_form_accepts_valid_numeric_fields() {
        let out = parse_config_form(b"poll_timeout=45&max_output_chars=5000").unwrap();
        assert_eq!(out.len(), 2);
        assert!(
            out.iter()
                .any(|(k, v)| *k == "TELEGRAM_POLL_TIMEOUT" && v == "45")
        );
        assert!(
            out.iter()
                .any(|(k, v)| *k == "TELEGRAM_MAX_OUTPUT_CHARS" && v == "5000")
        );
    }

    #[test]
    fn parse_config_form_rejects_out_of_range() {
        assert!(parse_config_form(b"poll_timeout=0").is_err());
        assert!(parse_config_form(b"poll_timeout=99999").is_err());
        assert!(parse_config_form(b"max_output_chars=50").is_err());
    }

    #[test]
    fn parse_config_form_ignores_unknown_keys() {
        let out = parse_config_form(b"poll_timeout=30&unknown=whatever").unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn parse_config_form_rejects_non_numeric() {
        assert!(parse_config_form(b"poll_timeout=abc").is_err());
    }
}
