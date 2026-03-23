//! Daemon orchestrator — the main watchdog loop.
//!
//! Contains the core `run()` function that ties together all daemon
//! subsystems: state machine, health checker, auto-healer, config watcher,
//! Telegram bot, identity manager, and scheduler.

pub mod auto_healer;
pub mod config_watcher;
pub mod health_checker;
pub mod identity;
pub mod scheduler;
pub mod state_machine;
pub mod status;
pub mod telegram;

use crate::alert::send_alert;
use crate::config::Config;
use crate::platform::{Platform, ProcessMetrics};
use crate::util::time_fmt::now_epoch;

use auto_healer::AutoHealer;
use config_watcher::ConfigFileWatcher;
use health_checker::check_health;
use identity::IdentityManager;
use scheduler::Scheduler;
use state_machine::{alert_on_transition, Classifier, State};
use telegram::{StatusSnapshot, TelegramBot, TelegramCommand};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

/// Path to the proc JSON file written each tick.
const PROC_JSON_PATH: &str = "/var/lib/outerclaw/audit/gateway-proc-latest.json";

/// Interval for re-checking deleted-then-recreated watched files (seconds).
const CONFIG_REWATCH_INTERVAL_SECS: u64 = 300;

/// Global running flag — set to `false` by signal handlers.
///
/// Signal handlers can only safely perform async-signal-safe operations;
/// writing an `AtomicBool` qualifies.  The main loop checks this flag on
/// every tick.
static GLOBAL_RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn signal_handler(_sig: libc::c_int) {
    GLOBAL_RUNNING.store(false, Ordering::Relaxed);
}

/// Install SIGTERM and SIGINT handlers.
fn install_signal_handlers() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            signal_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            signal_handler as *const () as libc::sighandler_t,
        );
    }
}

/// Run the watchdog daemon.  Returns an exit code (0 on clean shutdown).
pub fn run(cfg: Config, platform: Box<dyn Platform>) -> i32 {
    log::info!(
        "OuterClaw started (gateway_port={}, collect={}s, hang_warn={}s, hang_crit={}s)",
        cfg.gateway_port,
        cfg.collect_interval,
        cfg.hang_warn_secs,
        cfg.hang_crit_secs,
    );

    // sd_notify READY=1
    if let Err(e) = platform.notify_ready() {
        log::warn!("sd_notify READY failed: {e}");
    }

    // ── Signal handling ──
    GLOBAL_RUNNING.store(true, Ordering::Relaxed);
    install_signal_handlers();

    // ── Config file watcher ──
    let watched_files = cfg.watched_files();
    let mut file_watcher = match ConfigFileWatcher::new(&watched_files) {
        Ok(w) => Some(w),
        Err(e) => {
            log::error!("ConfigFileWatcher init failed (will run without): {e}");
            None
        }
    };

    // ── Shared status state for Telegram /status command ──
    let status_state = Arc::new(std::sync::Mutex::new(StatusSnapshot {
        state: "UNKNOWN".into(),
        pid: "N/A".into(),
        uptime: "unknown".into(),
        rss_mb: "N/A".into(),
        last_check: "never".into(),
    }));

    // ── Telegram bot (two-way, only if dedicated) ──
    let (tg_tx, tg_rx) = mpsc::channel::<TelegramCommand>();
    let mut tg_bot: Option<TelegramBot> = None;

    if cfg.tg_is_dedicated {
        let bot = TelegramBot::new(&cfg.tg_token, &cfg.tg_chat, cfg.max_response_bytes);

        let status_clone = status_state.clone();
        let status_fn: Arc<dyn Fn() -> StatusSnapshot + Send + Sync> =
            Arc::new(move || status_clone.lock().unwrap().clone());

        let _handle = bot.start(tg_tx.clone(), status_fn);
        tg_bot = Some(bot);
    }

    // ── Internal state ──
    let mut classifier = Classifier::new();
    let mut healer = AutoHealer::new();
    let mut identity_mgr = IdentityManager::new(cfg.identity_unlock_timeout);
    let mut _scheduler = Scheduler::new(cfg.cloud_enabled);

    let mut current_state = State::Unknown;
    let mut prev_metrics: Option<ProcessMetrics> = None;
    // Ensure first tick fires immediately
    let mut last_collect =
        Instant::now() - std::time::Duration::from_secs(cfg.collect_interval + 1);
    let mut last_rewatch = Instant::now();

    let tick_duration = std::time::Duration::from_secs(cfg.tick_interval);

    // ── Main loop: 1s tick ──
    while GLOBAL_RUNNING.load(Ordering::Relaxed) {
        // sd_notify WATCHDOG=1
        if let Err(e) = platform.notify_watchdog() {
            log::debug!("sd_notify WATCHDOG failed: {e}");
        }

        let now_instant = Instant::now();

        // ── Collection tick (every collect_interval) ──
        if now_instant.duration_since(last_collect).as_secs() >= cfg.collect_interval {
            match do_tick(
                &cfg,
                platform.as_ref(),
                &mut classifier,
                &mut healer,
                current_state,
                prev_metrics.as_ref(),
            ) {
                Ok((new_state, new_metrics)) => {
                    current_state = new_state;
                    prev_metrics = new_metrics;

                    // Update shared status for Telegram /status
                    if let Ok(mut s) = status_state.lock() {
                        s.state = current_state.as_str().into();
                        s.pid = prev_metrics
                            .as_ref()
                            .map(|m| m.pid.to_string())
                            .unwrap_or_else(|| "N/A".into());
                        s.rss_mb = prev_metrics
                            .as_ref()
                            .map(|m| (m.rss_bytes / (1024 * 1024)).to_string())
                            .unwrap_or_else(|| "N/A".into());
                        s.last_check = {
                            let epoch = now_epoch();
                            let secs = epoch as u64 % 86400;
                            format!(
                                "{:02}:{:02}:{:02} UTC",
                                secs / 3600,
                                (secs % 3600) / 60,
                                secs % 60
                            )
                        };
                        s.uptime = match platform.service_uptime_secs(&cfg.gateway_service) {
                            Ok(Some(up)) => crate::util::time_fmt::fmt_uptime(up),
                            _ => "unknown".into(),
                        };
                    }
                }
                Err(e) => {
                    log::error!("Error in collection tick: {e}");
                }
            }
            last_collect = now_instant;
        }

        // ── Config file watcher (non-blocking) ──
        if let Some(ref mut watcher) = file_watcher {
            let cfg_ref = &cfg;
            watcher.process_events(&|level, msg| {
                send_alert(level, msg, cfg_ref);
            });

            if now_instant.duration_since(last_rewatch).as_secs() >= CONFIG_REWATCH_INTERVAL_SECS {
                watcher.re_watch_missing();
                last_rewatch = now_instant;
            }
        }

        // ── Telegram command processing ──
        while let Ok(cmd) = tg_rx.try_recv() {
            process_telegram_command(
                &cmd,
                &cfg,
                platform.as_ref(),
                &mut identity_mgr,
                tg_bot.as_ref(),
            );
        }

        // ── Identity auto-relock timeout ──
        if identity_mgr.check_timeout() {
            log::warn!("Identity unlock timeout -- auto-relocking");
            let msg = identity_mgr.lock(platform.as_ref(), &cfg.openclaw_dir, "auto-timeout");
            send_alert("INFO", &msg, &cfg);
            if let Some(ref bot) = tg_bot {
                bot.send_message("Identity files auto-relocked (10 min timeout).");
            }
        }

        // ── Sleep tick ──
        std::thread::sleep(tick_duration);
    }

    // ── Cleanup ──
    log::info!("Received shutdown signal, cleaning up");
    if let Err(e) = platform.notify_stopping() {
        log::debug!("sd_notify STOPPING failed: {e}");
    }
    if let Some(ref bot) = tg_bot {
        bot.stop();
    }
    log::info!("OuterClaw stopped");
    0
}

/// One collection + classification + auto-heal cycle.
///
/// Returns the new state and optional new metrics.
fn do_tick(
    cfg: &Config,
    platform: &dyn Platform,
    classifier: &mut Classifier,
    healer: &mut AutoHealer,
    current_state: State,
    prev_metrics: Option<&ProcessMetrics>,
) -> Result<(State, Option<ProcessMetrics>), String> {
    // Find PID
    let pid = platform.find_service_pid(&cfg.gateway_service)?;

    // Collect /proc metrics
    let metrics = match pid {
        Some(p) => platform.collect_proc_metrics(p)?,
        None => None,
    };

    // HTTP health check
    let http_ok = check_health(&cfg.health_url, cfg.health_timeout);

    // Classify
    let now = now_epoch();
    let new_state = classifier.classify(
        metrics.as_ref(),
        prev_metrics,
        http_ok,
        now,
        cfg.hang_warn_secs,
        cfg.hang_crit_secs,
        cfg.io_delta_threshold,
        cfg.ctx_switch_threshold,
    );

    // Alert on state transition
    if let Some((level, msg)) = alert_on_transition(
        current_state,
        new_state,
        cfg.hang_warn_secs,
        cfg.hang_crit_secs,
    ) {
        send_alert(level, &msg, cfg);
    }

    // Auto-heal
    healer.tick(new_state, cfg, platform, &|level, msg| {
        send_alert(level, msg, cfg);
    });

    // Write proc JSON
    write_proc_json(metrics.as_ref(), new_state);

    log::debug!(
        "tick: pid={:?} state={} http={} io_r={} io_w={}",
        pid,
        new_state.as_str(),
        http_ok,
        metrics.as_ref().map(|m| m.read_bytes).unwrap_or(0),
        metrics.as_ref().map(|m| m.write_bytes).unwrap_or(0),
    );

    Ok((new_state, metrics))
}

/// Atomically write the latest proc snapshot for postmortem-collect.
fn write_proc_json(metrics: Option<&ProcessMetrics>, state: State) {
    let mut data = serde_json::json!({
        "timestamp": crate::alert::format_utc_now(),
        "outerclaw_state": state.as_str(),
    });

    if let Some(m) = metrics {
        if let Ok(m_json) = serde_json::to_value(m) {
            if let Some(obj) = m_json.as_object() {
                for (k, v) in obj {
                    data[k] = v.clone();
                }
            }
        }
    }

    let content = match serde_json::to_string_pretty(&data) {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to serialize proc JSON: {e}");
            return;
        }
    };

    if let Err(e) =
        crate::util::atomic_write::write(std::path::Path::new(PROC_JSON_PATH), content.as_bytes())
    {
        log::error!("Failed to write proc JSON: {e}");
    }
}

/// Process a single Telegram command on the main thread.
fn process_telegram_command(
    cmd: &TelegramCommand,
    cfg: &Config,
    platform: &dyn Platform,
    identity_mgr: &mut IdentityManager,
    tg_bot: Option<&TelegramBot>,
) {
    let user = &cmd.user;

    match cmd.action.as_str() {
        "restart" => {
            log::info!("Telegram: restart requested by {user}");
            send_alert(
                "WARNING",
                &format!("Gateway restart via Telegram by {user}"),
                cfg,
            );
            let msg = match platform.restart_service(&cfg.gateway_service) {
                Ok(()) => "Gateway restarted successfully.".to_string(),
                Err(e) => format!("Restart failed: {e}"),
            };
            if let Some(bot) = tg_bot {
                bot.send_message(&msg);
            }
        }
        "kill" => {
            log::info!("Telegram: kill requested by {user}");
            send_alert(
                "WARNING",
                &format!("Gateway STOP via Telegram by {user}"),
                cfg,
            );
            let msg = match platform.stop_service(&cfg.gateway_service) {
                Ok(()) => "Gateway stopped successfully.".to_string(),
                Err(e) => {
                    // Escalate to SIGKILL
                    log::warn!("Graceful stop failed ({e}), escalating to SIGKILL");
                    match platform.kill_service(&cfg.gateway_service) {
                        Ok(()) => {
                            "Gateway force-killed (SIGKILL) after graceful stop failed.".to_string()
                        }
                        Err(e2) => {
                            format!("Force-kill failed: {e2}. Manual intervention required.")
                        }
                    }
                }
            };
            if let Some(bot) = tg_bot {
                bot.send_message(&msg);
            }
        }
        "unlock_identity" => {
            let msg = identity_mgr.unlock(platform, &cfg.openclaw_dir, &format!("Telegram/{user}"));
            send_alert("WARNING", &msg, cfg);
            if let Some(bot) = tg_bot {
                bot.send_message(&msg);
            }
        }
        "lock_identity" => {
            let msg = identity_mgr.lock(platform, &cfg.openclaw_dir, &format!("Telegram/{user}"));
            send_alert("INFO", &msg, cfg);
            if let Some(bot) = tg_bot {
                bot.send_message(&msg);
            }
        }
        "kill_session" => {
            let session_id = cmd.args.get("session_id").map(|s| s.as_str()).unwrap_or("");
            if !crate::util::is_valid_session_id(session_id) {
                let msg = "Invalid session_id: must be alphanumeric, dash, or underscore.";
                if let Some(bot) = tg_bot {
                    bot.send_message(msg);
                }
                return;
            }
            let url = format!("{}/{session_id}", cfg.sessions_url);
            let agent = ureq::builder()
                .timeout_connect(std::time::Duration::from_secs(10))
                .timeout_read(std::time::Duration::from_secs(10))
                .build();
            let msg = match agent.delete(&url).call() {
                Ok(_) => {
                    log::info!("Telegram: kill_session {session_id} by {user}");
                    send_alert(
                        "INFO",
                        &format!("Session {session_id} kill via Telegram by {user}"),
                        cfg,
                    );
                    format!("Session {session_id} terminated.")
                }
                Err(e) => {
                    log::error!("kill_session failed: {e}");
                    format!("Failed to kill session {session_id}: {e}")
                }
            };
            if let Some(bot) = tg_bot {
                bot.send_message(&msg);
            }
        }
        other => {
            log::warn!("Unknown Telegram command action: {other}");
        }
    }
}
