//! Auto-healing logic — three-step escalation for gateway recovery.
//!
//! Direct port of Python `OuterClaw._auto_heal()`.
//!
//! Escalation steps:
//!   1a. CONFIRMED_HANG -> restart gateway service
//!   1b. DOWN + systemd failed -> reset-failed + restart
//!       (DOWN + inactive = user stopped it, skip;
//!        DOWN + activating = systemd retrying, wait)
//!   2.  If still down after settle period -> auto-recover from LKG
//!
//! One restart attempt per incident. 30-minute cooldown after recovery.

use crate::config::Config;
use crate::daemon::state_machine::State;
use crate::platform::{Platform, ServiceActive};
use crate::util::time_fmt::now_epoch;

/// Tracks auto-heal state across ticks.
pub struct AutoHealer {
    /// When a service restart was last attempted (epoch seconds), or `None`.
    restart_attempted_at: Option<f64>,
    /// When an LKG recovery was last attempted (epoch seconds), or `None`.
    recovery_attempted_at: Option<f64>,
}

impl AutoHealer {
    pub fn new() -> Self {
        Self {
            restart_attempted_at: None,
            recovery_attempted_at: None,
        }
    }

    /// Run one auto-heal tick.  Called from the main daemon loop after
    /// every classification cycle.
    ///
    /// `alert_fn(level, message)` is used instead of importing alert
    /// directly, to keep this module testable without Telegram deps.
    pub fn tick(
        &mut self,
        state: State,
        cfg: &Config,
        platform: &dyn Platform,
        alert_fn: &dyn Fn(&str, &str),
    ) {
        let now = now_epoch();

        // On recovery to HEALTHY, reset everything
        if state == State::Healthy {
            if self.restart_attempted_at.is_some() || self.recovery_attempted_at.is_some() {
                log::info!("Gateway recovered, resetting auto-heal state");
            }
            self.restart_attempted_at = None;
            self.recovery_attempted_at = None;
            return;
        }

        // Don't retry during cooldown after recovery attempt
        if let Some(recovery_at) = self.recovery_attempted_at {
            if now - recovery_at < cfg.recovery_cooldown as f64 {
                return;
            }
        }

        // Step 1a: On CONFIRMED_HANG, attempt restart (once per incident)
        if state == State::ConfirmedHang && self.restart_attempted_at.is_none() {
            self.restart_attempted_at = Some(now);
            log::warn!("CONFIRMED_HANG: attempting gateway restart");
            alert_fn("WARNING", "Auto-restarting gateway (CONFIRMED_HANG)");
            if let Err(e) = platform.restart_service(&cfg.gateway_service) {
                log::error!("Gateway restart failed: {e}");
            }
            return;
        }

        // Step 1b: On DOWN, query systemd state before deciding
        if state == State::Down && self.restart_attempted_at.is_none() {
            let active_state = match platform.service_state(&cfg.gateway_service) {
                Ok(s) => s,
                Err(e) => {
                    log::debug!("Cannot query service state: {e}");
                    return;
                }
            };

            match active_state {
                ServiceActive::Inactive => {
                    log::info!("Gateway inactive (intentional stop), skipping auto-heal");
                    return;
                }
                ServiceActive::Activating => {
                    log::debug!("Gateway activating (systemd retrying), waiting");
                    return;
                }
                ServiceActive::Failed => {
                    self.restart_attempted_at = Some(now);
                    log::warn!(
                        "Gateway FAILED (systemd gave up): attempting reset-failed + restart"
                    );
                    alert_fn(
                        "WARNING",
                        "Gateway service failed -- auto-restarting (reset-failed + restart)",
                    );
                    if let Err(e) = platform.reset_failed_service(&cfg.gateway_service) {
                        log::error!("reset-failed failed: {e}");
                    }
                    if let Err(e) = platform.restart_service(&cfg.gateway_service) {
                        log::error!("Gateway restart after reset-failed failed: {e}");
                    }
                    return;
                }
                other => {
                    log::debug!("Gateway service state={other}, waiting");
                    return;
                }
            }
        }

        // Step 2: If restart didn't help after settle period, try LKG recovery
        if matches!(state, State::Down | State::ConfirmedHang)
            && self.recovery_attempted_at.is_none()
        {
            let Some(restart_at) = self.restart_attempted_at else {
                return;
            };
            if now - restart_at > cfg.restart_settle_wait as f64 {
                self.recovery_attempted_at = Some(now);
                log::warn!("Restart didn't resolve issue, attempting LKG recovery");
                alert_fn("WARNING", "Auto-recovering from LKG (restart failed)");

                // Call ourselves with the auto-recover subcommand
                let exe = match std::env::current_exe() {
                    Ok(p) => p,
                    Err(e) => {
                        log::error!("Cannot determine own executable path: {e}");
                        alert_fn(
                            "CRITICAL",
                            &format!("Auto-recovery FAILED: cannot find executable: {e}"),
                        );
                        return;
                    }
                };

                match std::process::Command::new("sudo")
                    .arg(exe)
                    .arg("auto-recover")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .output()
                {
                    Ok(output) => {
                        if output.status.success() {
                            alert_fn(
                                "WARNING",
                                "Auto-recovered from LKG. Data rolled back to last known good state.",
                            );
                        } else {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            let tail: String = stderr
                                .chars()
                                .rev()
                                .take(200)
                                .collect::<String>()
                                .chars()
                                .rev()
                                .collect();
                            alert_fn(
                                "CRITICAL",
                                &format!(
                                    "Auto-recovery FAILED: {tail}. Manual intervention required."
                                ),
                            );
                        }
                    }
                    Err(e) => {
                        log::error!("Auto-recovery failed: {e}");
                        alert_fn(
                            "CRITICAL",
                            &format!(
                                "Auto-recovery script failed: {e}. Manual intervention required."
                            ),
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_healthy_resets_state() {
        let mut healer = AutoHealer::new();
        healer.restart_attempted_at = Some(1000.0);
        healer.recovery_attempted_at = Some(900.0);

        let cfg = make_test_config();
        let platform = MockPlatform;
        let alerts = std::cell::RefCell::new(Vec::new());
        let alert_fn = |level: &str, msg: &str| {
            alerts
                .borrow_mut()
                .push((level.to_string(), msg.to_string()));
        };

        healer.tick(State::Healthy, &cfg, &platform, &alert_fn);
        assert!(healer.restart_attempted_at.is_none());
        assert!(healer.recovery_attempted_at.is_none());
    }

    fn make_test_config() -> Config {
        Config {
            gateway_port: 18789,
            gateway_service: "openclaw-gateway.service".into(),
            health_timeout: 5,
            health_url: "http://127.0.0.1:18789/health".into(),
            sessions_url: "http://127.0.0.1:18789/sessions".into(),
            tick_interval: 1,
            collect_interval: 30,
            hang_warn_secs: 120,
            hang_crit_secs: 300,
            io_delta_threshold: 1_048_576,
            ctx_switch_threshold: 10,
            restart_settle_wait: 90,
            recovery_cooldown: 1800,
            kill_graceful_timeout: 15,
            identity_unlock_timeout: 600,
            mode: crate::config::Mode::Sudo,
            openclaw_dir: "/tmp/test-openclaw".into(),
            vault_dir: "/tmp/test-vault".into(),
            agent_user: "ocagent".into(),
            agent_home: "/tmp/test-home".into(),
            watchdog_user: "outerclaw".into(),
            tg_token: String::new(),
            tg_chat: String::new(),
            tg_is_dedicated: false,
            max_vault_mb: 2048,
            io_pressure_threshold: 25.0,
            cloud_enabled: false,
            cloud_remote: "outerclaw-crypt".into(),
            cloud_bandwidth: 0,
            max_response_bytes: 1_048_576,
        }
    }

    struct MockPlatform;

    impl Platform for MockPlatform {
        fn find_service_pid(&self, _: &str) -> Result<Option<u32>, String> {
            Ok(None)
        }
        fn collect_proc_metrics(
            &self,
            _: u32,
        ) -> Result<Option<crate::platform::ProcessMetrics>, String> {
            Ok(None)
        }
        fn service_state(&self, _: &str) -> Result<ServiceActive, String> {
            Ok(ServiceActive::Unknown)
        }
        fn restart_service(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn stop_service(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn kill_service(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn reset_failed_service(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn set_immutable(&self, _: &std::path::Path, _: bool) -> Result<(), String> {
            Ok(())
        }
        fn io_pressure_avg10(&self) -> Result<Option<f32>, String> {
            Ok(None)
        }
        fn notify_ready(&self) -> Result<(), String> {
            Ok(())
        }
        fn notify_watchdog(&self) -> Result<(), String> {
            Ok(())
        }
        fn notify_stopping(&self) -> Result<(), String> {
            Ok(())
        }
        fn install_service(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn uninstall_service(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn enable_service(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn service_uptime_secs(&self, _: &str) -> Result<Option<u64>, String> {
            Ok(None)
        }
        fn create_system_user(&self, _: &str, _: &std::path::Path) -> Result<(), String> {
            Ok(())
        }
        fn user_exists(&self, _: &str) -> Result<bool, String> {
            Ok(false)
        }
        fn delete_user(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn disk_usage_mb(&self, _: &std::path::Path) -> Result<u64, String> {
            Ok(0)
        }
        fn platform_name(&self) -> &str {
            "mock"
        }
    }
}
