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
use tebis::platform::multiplexer as mux;
use tebis::platform::signal::shutdown_signal;
use tebis::{
    audio, bridge, config, hooks_cli, inspect, lockfile, metrics, notify, security, service, setup,
    telegram,
};

const HELP: &str = "\
tebis - Telegram to local terminal bridge

Usage:
  tebis                 Run in foreground (auto-loads the per-user env file).
  tebis setup           Interactive first-run config wizard.
  tebis install         Install as a background service (launchd / systemd user).
  tebis uninstall [--purge]
                        Remove the background service. `--purge` also
                        deletes the installed binary and per-user tebis
                        config/data dirs (env + model cache + hook
                        manifest). Per-project hooks and system
                        packages (espeak-ng) are left alone.
  tebis start           Start the installed background service.
  tebis stop            Stop the installed background service.
  tebis restart         Stop + start the installed service (e.g. after config edit).
  tebis status          Show service + foreground state.
  tebis doctor          Diagnose system, privileges, deps, and service state.
                        Pass `-v` / `--verbose` to include OK rows.
  tebis hooks <verb>    Manage agent hooks: install | uninstall | status.
  tebis --help / -h     This message.
  tebis --version / -V  Print version.

Required env (set by `tebis setup` or your own scripts):
  TELEGRAM_BOT_TOKEN            Bot token from @BotFather
  TELEGRAM_ALLOWED_USER         Your numeric Telegram user id

Optional env:
  TELEGRAM_ALLOWED_SESSIONS     Comma-separated terminal session allowlist
  TELEGRAM_POLL_TIMEOUT         Long-poll seconds (default 30, 1..=900)
  TELEGRAM_MAX_OUTPUT_CHARS     /read truncation cap (default 4000)
  TELEGRAM_SUBMIT_GAP_MS        Text→Enter sleep in ms (default 300, 50..=5000)
  TELEGRAM_AUTOSTART_SESSION    Autostart session name
  TELEGRAM_AUTOSTART_DIR        Autostart working directory
  TELEGRAM_AUTOSTART_COMMAND    Autostart command (e.g. `claude`)
  NOTIFY_CHAT_ID                Chat id for agent hook replies
  NOTIFY_SOCKET_PATH            Local notification socket path on Unix
  INSPECT_PORT                  Local control dashboard on 127.0.0.1:<port>
  BRIDGE_ENV_FILE               Env file path (enables dashboard Settings edits)
  TELEGRAM_AUTOREPLY            `off` to disable terminal-output auto-reply
  TELEGRAM_HOOKS_MODE           `auto` to auto-install agent hooks at autostart
  TELEGRAM_NOTIFY               `off` to disable local hook notifications

Docs: README.md · SECURITY.md.
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
        Some("uninstall") => {
            let purge = env::args().any(|a| a == "--purge");
            service::uninstall(purge)
        }
        Some("start") => service::start(),
        Some("stop") => service::stop(),
        Some("restart") => service::restart(),
        Some("status") => service::status(),
        Some("doctor") => {
            let verbose = env::args().any(|a| a == "-v" || a == "--verbose");
            let report = tebis::preflight::run_doctor();
            println!();
            tebis::preflight::render(&report, verbose);
            println!();
            if report.has_blockers() {
                std::process::exit(1);
            }
            Ok(())
        }
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

fn handle_setup() -> Result<()> {
    let next = setup::run()?;
    // Eager-download audio models so the first daemon boot doesn't hit
    // a 346 MB wait. No-op when STT + TTS are both off; also no-op when
    // the wizard ended with Next::Exit (user bailed before finalizing).
    if !matches!(next, setup::Next::Exit) {
        let env_path = setup::env_file_path()?;
        if env_path.exists() {
            // SAFETY: still in `main` before any runtime/thread spawn.
            unsafe { config::load_env_file(&env_path) }
                .with_context(|| format!("loading env file {}", env_path.display()))?;
            prepare_audio_downloads()?;
        }
    }
    match next {
        setup::Next::Exit => Ok(()),
        setup::Next::Install => service::install(),
        setup::Next::RunForeground => run_bridge(),
    }
}

/// Pre-download STT + TTS model files during setup (not on first daemon start). Discards
/// the loaded subsystem; log-and-continue on failure so HF outages don't block the wizard.
fn prepare_audio_downloads() -> Result<()> {
    let config = match config::Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(err = %e, "prepare_audio_downloads: config load failed, skipping");
            return Ok(());
        }
    };
    if config.audio.stt.is_none() && config.audio.tts.is_none() {
        return Ok(());
    }
    println!();
    println!(
        "{}  Preparing audio models (one-time download)…",
        console::style("▶").cyan().bold()
    );
    telegram::install_crypto_provider();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("prepare_audio_downloads: tokio runtime")?;
    rt.block_on(async {
        let shutdown = CancellationToken::new();
        let tracker = TaskTracker::new();
        match audio::AudioSubsystem::new(&config.audio, &tracker, shutdown).await {
            Ok(_sub) => {
                println!(
                    "{}  Audio models ready.",
                    console::style("✓").green().bold()
                );
            }
            Err(e) => {
                println!(
                    "{}  Audio prep failed — the daemon will retry on first run. ({e})",
                    console::style("⚠").yellow().bold(),
                );
            }
        }
    });
    Ok(())
}

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
    // SAFETY: called from `main` before any runtime/thread spawn.
    unsafe { config::load_env_file(&path) }
        .with_context(|| format!("loading env file {}", path.display()))?;
    Ok(())
}

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

fn unauthorized_dead_end(err: &telegram::TelegramError) -> anyhow::Error {
    let env_path = setup::env_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "the tebis env file".to_string());
    eprintln!();
    eprintln!(
        "  {}  Telegram rejected the bot token (401 Unauthorized).",
        console::style("✗").red().bold()
    );
    eprintln!();
    eprintln!("  The token in {env_path} is wrong, revoked, or was regenerated");
    eprintln!("  in BotFather. Re-run `tebis setup` to paste a fresh one.");
    eprintln!();
    anyhow::anyhow!("bot token rejected by Telegram (401 Unauthorized): {err}")
}

use bridge::MAX_CONCURRENT_HANDLERS;

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

    let _lock = acquire_instance_lock()?;

    // Pin HTTP/TLS crates at warn — bot tokens live in request URLs.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,hyper_util=warn,rustls=warn")),
        )
        .with_target(false)
        .init();

    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&'static str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string payload>");
        tracing::error!(
            "PANIC at {}: {msg}",
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

    // Shutdown token built before Telegram startup so SIGTERM during
    // a getMe/deleteWebhook outage breaks out of the retry budget.
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

    // Fail-open: STT/TTS init problems log and continue text-only.
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

    // Built after the audio subsystem so the Voice section reflects
    // real init state (model loaded vs download failed).
    let inspect_snapshot = if inspect_port.is_some() {
        let tmux_ver = inspect::tmux_version().await;
        let voice_info = if config.audio.any_enabled() {
            Some(inspect::VoiceInfo {
                stt_model: config.audio.stt.as_ref().map(|s| s.model.clone()),
                stt_ready: audio.as_ref().is_some_and(|a| a.stt_model_name().is_some()),
                tts_backend: audio.as_ref().map_or("none", |a| a.tts_backend_kind()),
                tts_voice: audio.as_ref().and_then(|a| a.tts_voice().map(String::from)),
                tts_detail: audio
                    .as_ref()
                    .and_then(|a| a.tts_detail().map(String::from)),
                tts_scope: audio.as_ref().map_or("", |a| {
                    if a.tts_respond_to_all() {
                        "all"
                    } else {
                        "voice-only"
                    }
                }),
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

    let tmux = Arc::new(mux::Mux::new(
        config.allowed_sessions.clone(),
        config.max_output_chars,
        Duration::from_millis(u64::from(config.submit_gap_ms)),
    ));
    if let Some(a) = config.autostart.as_ref() {
        tracing::info!(
            session = %a.session,
            dir = %a.dir,
            command = %a.command,
            "Autostart configured — first plain-text message will provision this session"
        );
    }

    // Snapshot before `config.autostart` is moved into `SessionState`.
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

    let env_file_path: Option<std::path::PathBuf> =
        env::var("BRIDGE_ENV_FILE").ok().map(std::path::PathBuf::from);

    let mut offset: Option<i64> = None;
    let mut backoff = Duration::from_secs(1);
    // 409: wait poll_timeout+5s so the other poller's long-poll expires.
    let conflict_backoff = Duration::from_secs(u64::from(config.poll_timeout) + 5);

    tracing::info!(
        max_concurrent_handlers = MAX_CONCURRENT_HANDLERS,
        poll_timeout_secs = config.poll_timeout,
        "Bridge ready"
    );

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
                        // Audio uploads share the voice path; codec only accepts OGG/Opus.
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
                        shutdown: shutdown.clone(),
                        audio: audio.clone(),
                        env_file_path: env_file_path.clone(),
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
            // 401 mid-session: token was revoked. Exit clean instead of retry-spamming.
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
        console::style(format!("· Telegram → {} bridge", mux::BINARY)).dim(),
    );
}

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
