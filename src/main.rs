//! Process entry point — argv dispatch + foreground run loop.

use anyhow::{Context, Result};
use std::env;
use std::process;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing_subscriber::EnvFilter;

use tebis::bridge::session;
use tebis::{
    audio, bridge, config, hooks_cli, inspect, lockfile, metrics, notify, security, service, setup,
    telegram, tmux,
};

const HELP: &str = "\
tebis — Telegram-tmux bridge

Usage:
  tebis                 Run in foreground (auto-loads ~/.config/tebis/env).
  tebis setup           Interactive first-run config wizard.
  tebis install         Install as a background service (launchd / systemd user).
  tebis uninstall       Remove the background service.
  tebis start           Start the installed background service.
  tebis stop            Stop the installed background service.
  tebis restart         Stop + start the installed service (e.g. after config edit).
  tebis status          Show service + foreground state.
  tebis hooks <verb>    Manage agent hooks: install | uninstall | status.
  tebis --help / -h     This message.
  tebis --version / -V  Print version.

Required env (set by `tebis setup` or your own scripts):
  TELEGRAM_BOT_TOKEN            Bot token from @BotFather
  TELEGRAM_ALLOWED_USER         Your numeric Telegram user id

Optional env:
  TELEGRAM_ALLOWED_SESSIONS     Comma-separated tmux session allowlist
  TELEGRAM_POLL_TIMEOUT         Long-poll seconds (default 30, 1..=900)
  TELEGRAM_MAX_OUTPUT_CHARS     /read truncation cap (default 4000)
  TELEGRAM_AUTOSTART_SESSION    Autostart session name
  TELEGRAM_AUTOSTART_DIR        Autostart working directory
  TELEGRAM_AUTOSTART_COMMAND    Autostart command (e.g. `claude`)
  NOTIFY_CHAT_ID                Enable hook-forward UDS listener
  NOTIFY_SOCKET_PATH            UDS path (default $XDG_RUNTIME_DIR/tebis.sock)
  INSPECT_PORT                  Local control dashboard on 127.0.0.1:<port>
  BRIDGE_ENV_FILE               Env file path (enables dashboard Settings edits)
  TELEGRAM_AUTOREPLY            `off` to disable pane-settle auto-reply
  TELEGRAM_HOOKS_MODE           `auto` to auto-install agent hooks at autostart
  TELEGRAM_NOTIFY               `off` to disable outbound-notify UDS listener

Docs: README.md · CLAUDE.md.
";

fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some("--help" | "-h") => {
            print!("{HELP}");
            Ok(())
        }
        Some("--version" | "-V") => {
            println!("tebis {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("setup" | "init") => handle_setup(),
        Some("install") => service::install(),
        Some("uninstall") => service::uninstall(),
        Some("start") => service::start(),
        Some("stop") => service::stop(),
        Some("restart") => service::restart(),
        Some("status") => service::status(),
        Some("hooks") => hooks_cli::run(&env::args().skip(2).collect::<Vec<_>>()),
        Some(other) => {
            eprintln!("tebis: unknown argument: {other}\n\n{HELP}");
            process::exit(2);
        }
        None => {
            ensure_env_loaded()?;
            run_bridge()
        }
    }
}

/// Dispatch what the wizard asked for — foreground run / service install /
/// nothing (already printed instructions).
fn handle_setup() -> Result<()> {
    match setup::run()? {
        setup::Next::Exit => Ok(()),
        setup::Next::Install => service::install(),
        setup::Next::RunForeground => {
            let env_path = setup::env_file_path()?;
            // SAFETY: we're still in `main` before any async runtime or
            // thread is spawned. `run_bridge` below creates the tokio
            // runtime; env is loaded strictly before that.
            unsafe { config::load_env_file(&env_path) }
                .with_context(|| format!("loading env file {}", env_path.display()))?;
            run_bridge()
        }
    }
}

/// Make bare `tebis` Just Work after `tebis setup`. If the env vars are
/// already present (systemd / launchd path), this is a no-op.
fn ensure_env_loaded() -> Result<()> {
    if env::var("TELEGRAM_BOT_TOKEN").is_ok_and(|v| !v.is_empty()) {
        return Ok(());
    }
    let Ok(path) = setup::env_file_path() else {
        nudge_to_setup();
    };
    if !path.exists() {
        nudge_to_setup();
    }
    // SAFETY: called from `main` before the tokio runtime is built.
    unsafe { config::load_env_file(&path) }
        .with_context(|| format!("loading env file {}", path.display()))?;
    Ok(())
}

/// `auto` / `off` — the user-facing name for `HooksMode`. Inline
/// rather than adding an `impl Display` on `HooksMode` because the
/// dashboard row is the only consumer.
const fn hooks_mode_label(mode: tebis::agent_hooks::HooksMode) -> &'static str {
    match mode {
        tebis::agent_hooks::HooksMode::Auto => "auto",
        tebis::agent_hooks::HooksMode::Off => "off",
    }
}

fn nudge_to_setup() -> ! {
    eprintln!(
        "tebis: no config found — run `tebis setup` first.\n\
         (Expected env vars like TELEGRAM_BOT_TOKEN; see `tebis --help`.)"
    );
    process::exit(2);
}

/// Translate a 401 from Telegram into an operator-actionable anyhow error.
/// Returned from `run_bridge` so launchd / systemd still see a failure
/// exit, but the user gets a clear "re-run setup" message instead of a
/// cryptic `API error 401: Unauthorized` crash loop.
fn unauthorized_dead_end(err: &telegram::TelegramError) -> anyhow::Error {
    eprintln!();
    eprintln!(
        "  {}  Telegram rejected the bot token (401 Unauthorized).",
        console::style("✗").red().bold()
    );
    eprintln!();
    eprintln!("  The token in ~/.config/tebis/env is wrong, revoked, or was regenerated");
    eprintln!("  in BotFather. Re-run `tebis setup` to paste a fresh one.");
    eprintln!();
    anyhow::anyhow!("bot token rejected by Telegram (401 Unauthorized): {err}")
}

/// Single-user workload: realistic concurrency is 1–2. 8 bounds subprocess
/// fan-out when Telegram delivers a queued burst.
const MAX_CONCURRENT_HANDLERS: usize = 8;

/// Acquire the single-instance lock. On conflict, tell the user exactly
/// who holds it — the background service, a foreground run, or unknown.
fn acquire_instance_lock() -> Result<lockfile::LockFile> {
    let path = lockfile::default_path();
    match lockfile::acquire(&path) {
        Ok(lock) => Ok(lock),
        Err(lockfile::AcquireError::Locked { pid, .. }) => {
            eprintln!();
            eprintln!("tebis: another instance is already running.");
            if let Some(pid) = pid {
                eprintln!("  holder pid:   {pid}");
            }
            if service::is_running() {
                eprintln!(
                    "  source:       the background service (stop it with `tebis stop`, \
                     or remove it with `tebis uninstall`)."
                );
                eprintln!(
                    "  note:         if you're running a dev build from source and the \
                     service is installed too, run `tebis stop` before testing."
                );
            } else if let Some(pid) = pid {
                eprintln!(
                    "  source:       another foreground run — stop it with `kill {pid}` \
                     if you're sure."
                );
            } else {
                eprintln!(
                    "  source:       unknown (lock file is empty). Try \
                     `ps aux | grep tebis` to find the holder."
                );
            }
            eprintln!();
            process::exit(1);
        }
        Err(e) => Err(anyhow::Error::new(e)),
    }
}

#[tokio::main]
#[expect(
    clippy::too_many_lines,
    reason = "top-level wiring; factoring just shuffles it"
)]
async fn run_bridge() -> Result<()> {
    print_startup_banner();

    // Single-instance guard. Held for the lifetime of the daemon; dropped
    // at function exit, which releases the flock and removes the file.
    let _lock = acquire_instance_lock()?;

    // HTTP/TLS crates pinned at warn so bot tokens embedded in request URLs
    // never appear at debug level in the journal.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,hyper_util=warn,rustls=warn")),
        )
        .with_target(false)
        .init();

    std::panic::set_hook(Box::new(|info| {
        tracing::error!(
            "PANIC at {}: panicked",
            info.location()
                .map_or_else(|| "unknown".to_string(), ToString::to_string)
        );
    }));

    telegram::install_crypto_provider();

    let mut config = config::Config::from_env()?;
    tracing::info!(
        allowed_user = config.allowed_user_id,
        sessions = ?config.allowed_sessions,
        "Config loaded"
    );

    // Build the shutdown token + signal listener BEFORE the Telegram
    // startup so a SIGTERM during a Telegram outage breaks out of the
    // getMe / deleteWebhook retry budget (up to ~50s without this race).
    let shutdown = CancellationToken::new();
    let tracker = TaskTracker::new();
    {
        let shutdown = shutdown.clone();
        tracker.spawn(async move {
            shutdown_signal().await;
            tracing::info!("Shutdown signal received");
            shutdown.cancel();
        });
    }

    let tg = Arc::new(telegram::TelegramClient::new(&config.bot_token));
    let delete_webhook_result = tokio::select! {
        r = tg.delete_webhook() => r,
        () = shutdown.cancelled() => return Ok(()),
    };
    if let Err(e) = delete_webhook_result {
        if e.is_unauthorized() {
            return Err(unauthorized_dead_end(&e));
        }
        return Err(e.into());
    }
    let get_me_result = tokio::select! {
        r = tg.get_me() => r,
        () = shutdown.cancelled() => return Ok(()),
    };
    let me = match get_me_result {
        Ok(me) => me,
        Err(e) if e.is_unauthorized() => return Err(unauthorized_dead_end(&e)),
        Err(e) => return Err(e.into()),
    };
    tracing::info!(
        bot_id = me.id,
        bot_username = ?me.username,
        "Bot connected: {}",
        me.first_name
    );

    let inspect_port: Option<u16> = match env::var("INSPECT_PORT") {
        Ok(s) => Some(
            s.parse()
                .context("INSPECT_PORT must be a valid port number (1..=65535)")?,
        ),
        Err(_) => None,
    };

    let autoreply_cfg = config.autoreply.take().map(Arc::new);

    // Audio subsystem: constructed lazily. With STT off (the
    // public-release default) `new` touches nothing — no download, no
    // memory. When STT is on, this is the blocking model-download path
    // (~53 s on first run for `base.en`); we run it on the current
    // task so startup waits for a cached-and-ready subsystem before
    // the bridge accepts messages.
    //
    // Fail-open: if model download or whisper-rs load fails we log a
    // warn and continue text-only. Bot token problems are different
    // (handled by `unauthorized_dead_end` above); audio problems are
    // recoverable by unsetting TELEGRAM_STT or fixing the underlying
    // cause, so we shouldn't crash.
    let audio = if config.audio.any_enabled() {
        match audio::AudioSubsystem::new(&config.audio, &tracker, shutdown.clone()).await {
            Ok(a) => {
                if let Some(m) = a.stt_model_name() {
                    tracing::info!(model = %m, "Audio: local STT ready");
                }
                Some(a)
            }
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    "Audio subsystem failed to initialize; continuing text-only. \
                     Set TELEGRAM_STT=off or fix the cause above to silence this."
                );
                None
            }
        }
    } else {
        None
    };

    // Snapshot is built AFTER the audio subsystem so the dashboard's
    // Voice section can reflect real initialization state (model loaded
    // vs download failed) rather than guessing from config alone.
    let inspect_snapshot = if inspect_port.is_some() {
        let tmux_ver = inspect::tmux_version().await;
        // Voice section reflects ACTUAL runtime state, not config
        // intent. On Linux with TELEGRAM_TTS=on, TTS init fails with
        // UnsupportedPlatform and the subsystem continues STT-only —
        // the dashboard should say "TTS disabled" for that user, not
        // dangle a voice name that isn't actually synthesizing.
        let voice_info = if config.audio.any_enabled() {
            Some(inspect::VoiceInfo {
                stt_model: config.audio.stt.as_ref().map(|s| s.model.clone()),
                stt_ready: audio
                    .as_ref()
                    .is_some_and(|a| a.stt_model_name().is_some()),
                tts_backend: audio.as_ref().map_or("none", |a| a.tts_backend_kind()),
                tts_voice: audio.as_ref().and_then(|a| a.tts_voice().map(String::from)),
                tts_detail: audio.as_ref().and_then(|a| a.tts_detail().map(String::from)),
                tts_scope: audio
                    .as_ref()
                    .map_or("", |a| if a.tts_respond_to_all() { "all" } else { "voice-only" }),
            })
        } else {
            None
        };
        Some(Arc::new(inspect::Snapshot {
            bridge: inspect::BridgeInfo {
                version: env!("CARGO_PKG_VERSION"),
                pid: std::process::id(),
                hostname: inspect::hostname(),
                tmux_version: tmux_ver,
            },
            bot: Some(inspect::BotInfo {
                id: me.id,
                first_name: me.first_name.clone(),
                username: me.username.clone(),
            }),
            allowed_user_id: config.allowed_user_id,
            allowed_sessions: config.allowed_sessions.clone(),
            poll_timeout: config.poll_timeout,
            max_output_chars: config.max_output_chars,
            max_concurrent_handlers: MAX_CONCURRENT_HANDLERS,
            autostart: config.autostart.as_ref().map(|a| inspect::AutostartInfo {
                session: a.session.clone(),
                dir: a.dir.clone(),
                command: a.command.clone(),
            }),
            notify: config.notify.as_ref().map(|n| inspect::NotifyInfo {
                socket_path: n.socket_path.to_string_lossy().into_owned(),
                chat_id: n.chat_id,
            }),
            hooks: inspect::HooksInfo {
                mode: hooks_mode_label(config.hooks_mode),
                entries: tebis::agent_hooks::manifest::load_entries()
                    .into_iter()
                    .map(|e| inspect::HooksEntryInfo {
                        agent: e.agent,
                        dir: e.dir.display().to_string(),
                        installed_at: e.installed_at,
                    })
                    .collect(),
            },
            voice: voice_info,
            env_file: env::var("BRIDGE_ENV_FILE").ok(),
        }))
    } else {
        None
    };

    let tmux = Arc::new(tmux::Tmux::new(
        config.allowed_sessions.clone(),
        config.max_output_chars,
    ));
    if let Some(a) = config.autostart.as_ref() {
        tracing::info!(
            session = %a.session,
            dir = %a.dir,
            command = %a.command,
            "Autostart configured — first plain-text message will provision this session"
        );
    }

    // Snapshot everything `print_ready_status` needs BEFORE we move
    // `config.autostart` into `SessionState`. Reading from env vars at
    // print time would work but drifts from the validated Config.
    let ready_allowlist = config.allowed_sessions.clone();
    let ready_autostart = config
        .autostart
        .as_ref()
        .map(|a| format!("{} · {} · {}", a.session, a.dir, a.command));

    let session_state = Arc::new(session::SessionState::new(
        config.autostart.take(),
        config.hooks_mode,
    ));
    let rate_limiter = Arc::new(security::RateLimiter::new(30, 10));
    let handler_sem = Arc::new(Semaphore::new(MAX_CONCURRENT_HANDLERS));
    let metrics = Arc::new(metrics::Metrics::new());
    let started_at = Instant::now();

    if let Some(n) = config.notify {
        let forwarder = Arc::new(notify::TelegramForwarder::new(tg.clone(), n.chat_id));
        notify::spawn(&tracker, shutdown.clone(), n.socket_path, forwarder)?;
    }

    if let (Some(port), Some(snapshot)) = (inspect_port, inspect_snapshot) {
        let live = inspect::LiveContext::new(
            tmux.clone(),
            session_state.clone(),
            handler_sem.clone(),
            metrics.clone(),
            started_at,
            shutdown.clone(),
        );
        inspect::spawn(&tracker, shutdown.clone(), port, snapshot, live)?;
    }

    let mut offset: Option<i64> = None;
    let mut backoff = Duration::from_secs(1);
    // 409 Conflict → another poller holds the long-poll. Wait at least
    // `poll_timeout + 5s` so the prior long-poll expires before we retry.
    let conflict_backoff = Duration::from_secs(u64::from(config.poll_timeout) + 5);

    tracing::info!(
        max_concurrent_handlers = MAX_CONCURRENT_HANDLERS,
        poll_timeout_secs = config.poll_timeout,
        "Bridge ready"
    );

    // Human-friendly status block — complements the structured log above.
    print_ready_status(
        &me,
        config.allowed_user_id,
        &ready_allowlist,
        ready_autostart.as_deref(),
        inspect_port,
    );

    loop {
        let poll_result = tokio::select! {
            r = tg.get_updates(offset, config.poll_timeout) => r,
            () = shutdown.cancelled() => break,
        };

        match poll_result {
            Ok(updates) => {
                backoff = Duration::from_secs(1);
                metrics.record_poll_success();
                if updates.len() >= 100 {
                    tracing::info!(
                        count = updates.len(),
                        "getUpdates returned full batch — more may be pending"
                    );
                }

                for update in updates {
                    offset = Some(update.update_id + 1);

                    if !security::is_authorized(&update, config.allowed_user_id) {
                        continue;
                    }

                    let Some(message) = update.message else {
                        continue;
                    };
                    let chat_id = message.chat.id;
                    let message_id = message.message_id;

                    let payload = if let Some(text) = message.text {
                        tracing::debug!(chat_id, bytes = text.len(), "Received text message");
                        bridge::Payload::Text(text)
                    } else if let Some(v) = message.voice {
                        tracing::debug!(
                            chat_id,
                            duration_sec = v.duration,
                            size = ?v.file_size,
                            "Received voice message"
                        );
                        bridge::Payload::Voice {
                            file_id: v.file_id,
                            duration_sec: v.duration,
                            size_bytes: v.file_size,
                        }
                    } else if let Some(a) = message.audio {
                        // Music-file upload, same code path as voice.
                        // The codec only accepts OGG/Opus; MP3/M4A uploads
                        // will be rejected downstream with a clear error.
                        tracing::debug!(
                            chat_id,
                            duration_sec = a.duration,
                            size = ?a.file_size,
                            mime = ?a.mime_type,
                            "Received audio file"
                        );
                        bridge::Payload::Voice {
                            file_id: a.file_id,
                            duration_sec: a.duration,
                            size_bytes: a.file_size,
                        }
                    } else {
                        // No handled content (sticker, photo, …). Drop
                        // silently — different from voice-with-STT-off
                        // which gets a user-facing reply.
                        continue;
                    };

                    let ctx = bridge::HandlerContext {
                        tg: tg.clone(),
                        tmux: tmux.clone(),
                        session: session_state.clone(),
                        rate_limiter: rate_limiter.clone(),
                        handler_sem: handler_sem.clone(),
                        started_at,
                        metrics: metrics.clone(),
                        autoreply: autoreply_cfg.clone(),
                        tracker: tracker.clone(),
                        audio: audio.clone(),
                    };

                    tracker.spawn(bridge::handle_update(ctx, chat_id, message_id, payload));
                }
            }
            Err(e) if e.is_conflict() => {
                metrics.record_poll_error();
                tracing::warn!(
                    err = %e,
                    backoff_secs = conflict_backoff.as_secs(),
                    "409 Conflict — another poller is active (maybe the background service?)"
                );
                tokio::select! {
                    () = tokio::time::sleep(conflict_backoff) => {}
                    () = shutdown.cancelled() => break,
                }
            }
            // 401 at runtime means the bot token was revoked mid-session.
            // The poll loop would otherwise retry with exponential backoff
            // forever, spamming the journal. Exit with a clean error so
            // the operator sees the paste-a-fresh-token message on next
            // launch (startup re-runs `get_me` which also dead-ends 401).
            Err(e) if e.is_unauthorized() => {
                metrics.record_poll_error();
                return Err(unauthorized_dead_end(&e));
            }
            Err(e) => {
                metrics.record_poll_error();
                tracing::error!(
                    err = %e,
                    backoff_secs = backoff.as_secs(),
                    "getUpdates failed, backing off"
                );
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    () = shutdown.cancelled() => break,
                }
                backoff = backoff.saturating_mul(2).min(Duration::from_mins(1));
            }
        }
    }

    tracing::info!("Draining in-flight handlers (15s budget)…");
    tracker.close();
    if tokio::time::timeout(Duration::from_secs(15), tracker.wait())
        .await
        .is_ok()
    {
        tracing::info!("Shutdown complete");
    } else {
        tracing::warn!("Shutdown drain timed out; exiting anyway");
    }
    Ok(())
}

/// One-line identity banner. Suppressed when stdout isn't a terminal so
/// systemd / launchd logs stay clean.
fn print_startup_banner() {
    let term = console::Term::stdout();
    if !term.is_term() {
        return;
    }
    let version = env!("CARGO_PKG_VERSION");
    println!(
        "  {}  {}  {}",
        console::style("tebis").bold().cyan(),
        console::style(format!("v{version}")).dim(),
        console::style("· Telegram → tmux bridge").dim(),
    );
}

/// Human-readable "what's running" block. Printed after the bridge reaches
/// ready, only when stdout is a tty (systemd / launchd logs stay structured).
fn print_ready_status(
    me: &tebis::telegram::types::BotUser,
    allowed: i64,
    allowlist: &[String],
    autostart: Option<&str>,
    inspect_port: Option<u16>,
) {
    use console::style;
    let term = console::Term::stdout();
    if !term.is_term() {
        return;
    }
    let username = me
        .username
        .as_deref()
        .map_or_else(|| "(no @username)".to_string(), |u| format!("@{u}"));

    println!();
    println!(
        "  {}  {}",
        style("online").green().bold(),
        style("— waiting for messages").dim(),
    );
    println!();
    kv(
        "Bot",
        &format!("{} · {} · id {}", me.first_name, username, me.id),
    );
    kv("Allowed user", &format!("id {allowed}"));
    if allowlist.is_empty() {
        kv("Sessions", "(any — permissive)");
    } else {
        kv("Sessions", &allowlist.join(", "));
    }
    if let Some(a) = autostart {
        kv("Autostart", a);
    }
    if let Some(port) = inspect_port {
        let url = format!("http://127.0.0.1:{port}");
        kv_url("Dashboard", &url);
    }
    println!();
    println!(
        "  {} to stop · logs above are also captured by tracing",
        style("Ctrl-C").bold(),
    );
    println!();
}

fn kv(label: &str, value: &str) {
    use console::style;
    println!("  {}  {value}", style(format!("{label:<13}")).dim());
}

fn kv_url(label: &str, url: &str) {
    use console::style;
    println!(
        "  {}  {}",
        style(format!("{label:<13}")).dim(),
        style(url).cyan().underlined(),
    );
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }
}
