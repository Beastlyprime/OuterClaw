//! Four-factor health check for the OpenClaw gateway.
//!
//! Rust port of `scripts/healthcheck.sh`. Checks service state, HTTP
//! responsiveness, memory pressure, and restart count. Saves state to
//! a JSON file for cross-run comparison and alerting.

use crate::alert::send_alert;
use crate::config::Config;
use crate::platform::{Platform, ServiceActive};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

/// Health state persisted between runs.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct HealthState {
    timestamp: String,
    http_code: String,
    mem_current: String,
    restarts: u64,
}

/// Run the 4-factor health check. Returns 0 on success, 1 on critical failure.
pub fn run(cfg: Config, platform: Box<dyn Platform>) -> i32 {
    let audit_dir = cfg.vault_dir.join("audit");
    let _ = fs::create_dir_all(&audit_dir);
    let state_file = audit_dir.join("health-state.json");

    // Load previous state for comparison
    let prev_state = load_state(&state_file);

    let mut new_state = HealthState {
        timestamp: crate::alert::format_utc_now(),
        ..Default::default()
    };

    // ── Check 1: Service active ───────────────────────────────────
    let service_state = platform
        .service_state(&cfg.gateway_service)
        .unwrap_or(ServiceActive::Unknown);

    if service_state != ServiceActive::Active {
        send_alert(
            "CRITICAL",
            &format!("Gateway service not active ({})", service_state),
            &cfg,
        );
        save_state(&state_file, &new_state);
        return 1;
    }

    // ── Check 2: HTTP responsive ──────────────────────────────────
    let http_ok = crate::daemon::health_checker::check_health(&cfg.health_url, cfg.health_timeout);

    if http_ok {
        new_state.http_code = "200".into();
    } else {
        new_state.http_code = "000".into();
        send_alert(
            "WARNING",
            "Gateway not responding to HTTP health check",
            &cfg,
        );
    }

    // ── Check 3: Memory pressure ──────────────────────────────────
    let mem_current = read_memory_current(&cfg.gateway_service);
    match &mem_current {
        Some(bytes) => {
            let mb = bytes / (1024 * 1024);
            new_state.mem_current = bytes.to_string();
            if mb > 7168 {
                // >7GB
                send_alert(
                    "WARNING",
                    &format!("Gateway memory at {mb}MB (limit: 8192MB)"),
                    &cfg,
                );
            }
        }
        None => {
            new_state.mem_current = "0".into();
        }
    }

    // ── Check 4: Restart count (detect crash loops) ───────────────
    let n_restarts = read_restart_count(&cfg.gateway_service);
    new_state.restarts = n_restarts;

    if n_restarts > 0 && n_restarts > prev_state.restarts {
        send_alert(
            "WARNING",
            &format!(
                "Gateway restarted (count: {n_restarts}, was: {})",
                prev_state.restarts
            ),
            &cfg,
        );
    }

    // ── Save state ────────────────────────────────────────────────
    save_state(&state_file, &new_state);

    0
}

/// Load the previous health state from JSON file.
fn load_state(path: &Path) -> HealthState {
    if !path.exists() {
        return HealthState::default();
    }

    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => HealthState::default(),
    }
}

/// Save the health state to a JSON file.
fn save_state(path: &Path, state: &HealthState) {
    match serde_json::to_string_pretty(state) {
        Ok(json) => {
            match fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(path)
            {
                Ok(mut f) => {
                    let _ = f.write_all(json.as_bytes());
                }
                Err(e) => {
                    log::error!("Cannot write health state to {}: {e}", path.display());
                }
            }
        }
        Err(e) => {
            log::error!("Cannot serialize health state: {e}");
        }
    }
}

/// Read MemoryCurrent from systemd service properties.
fn read_memory_current(service: &str) -> Option<u64> {
    let output = Command::new("systemctl")
        .args(["show", service, "--property=MemoryCurrent", "--value"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let trimmed = raw.trim();

    // systemd returns "[not set]" when cgroup accounting is disabled
    if trimmed.is_empty() || trimmed.contains("not set") {
        return None;
    }

    trimmed.parse::<u64>().ok()
}

/// Read NRestarts from systemd service properties.
fn read_restart_count(service: &str) -> u64 {
    let output = Command::new("systemctl")
        .args(["show", service, "--property=NRestarts", "--value"])
        .output()
        .ok();

    match output {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<u64>()
            .unwrap_or(0),
        _ => 0,
    }
}
