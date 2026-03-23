//! Cloud backup sync — push snapshots and LKG to encrypted cloud storage.
//!
//! Rust port of `scripts/cloud-sync.sh`. Runs as the `outerclaw` user via
//! the `oc-cloud-sync.timer` (every 2 hours). Uses rclone with the config
//! created by `cloud setup`.

use crate::config::Config;
use crate::platform::Platform;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

const RCLONE_CONFIG_PATH: &str = "/var/lib/outerclaw/config/rclone.conf";

/// Run the cloud sync operation.
pub fn run(cfg: Config, _platform: Box<dyn Platform>) -> i32 {
    // ── Check cloud enabled ─────────────────────────────────────
    if !cfg.cloud_enabled {
        log_sync(
            &cfg.vault_dir,
            "SKIP: Cloud sync disabled (CLOUD_ENABLED!=true)",
        );
        return 0;
    }

    // ── Check rclone installed ──────────────────────────────────
    if Command::new("which")
        .arg("rclone")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        log_sync(&cfg.vault_dir, "ERROR: rclone not found");
        crate::alert::send_alert("WARNING", "Cloud sync failed: rclone not installed", &cfg);
        return 1;
    }

    // ── Check rclone config exists ──────────────────────────────
    let rclone_config = Path::new(RCLONE_CONFIG_PATH);
    if !rclone_config.exists() {
        log_sync(
            &cfg.vault_dir,
            &format!("ERROR: rclone config not found at {RCLONE_CONFIG_PATH}"),
        );
        crate::alert::send_alert(
            "WARNING",
            "Cloud sync failed: rclone not configured (run cloud setup)",
            &cfg,
        );
        return 1;
    }

    // ── I/O pressure gate ───────────────────────────────────────
    if !check_io_pressure(cfg.io_pressure_threshold) {
        log_sync(
            &cfg.vault_dir,
            "SKIP: I/O pressure too high, cloud sync deferred",
        );
        return 0;
    }

    // ── Build rclone flags ──────────────────────────────────────
    let cloud_remote = &cfg.cloud_remote;
    let mut base_flags = vec![
        "--config".to_string(),
        RCLONE_CONFIG_PATH.to_string(),
        "--retries".to_string(),
        "3".to_string(),
        "--low-level-retries".to_string(),
        "5".to_string(),
        "--timeout".to_string(),
        "60s".to_string(),
        "--contimeout".to_string(),
        "30s".to_string(),
        "--log-level".to_string(),
        "WARNING".to_string(),
        "--stats".to_string(),
        "0".to_string(),
    ];

    if cfg.cloud_bandwidth > 0 {
        base_flags.push("--bwlimit".to_string());
        base_flags.push(format!("{}k", cfg.cloud_bandwidth));
    }

    log_sync(
        &cfg.vault_dir,
        &format!("START: Cloud sync to {cloud_remote}"),
    );

    let mut errors = 0u32;

    // ── Sync snapshots/ ─────────────────────────────────────────
    let snap_dir = cfg.vault_dir.join("snapshots");
    if snap_dir.is_dir() {
        let snap_count = count_entries(&snap_dir, false);
        if snap_count > 0 {
            log_sync(
                &cfg.vault_dir,
                &format!("Syncing {snap_count} snapshot files..."),
            );

            let status = Command::new("rclone")
                .arg("sync")
                .arg(&snap_dir)
                .arg(format!("{cloud_remote}:snapshots/"))
                .args(&base_flags)
                .status();

            match status {
                Ok(s) if s.success() => {
                    log_sync(
                        &cfg.vault_dir,
                        &format!("OK: snapshots synced ({snap_count} files)"),
                    );
                }
                Ok(s) => {
                    let rc = s.code().unwrap_or(-1);
                    log_sync(
                        &cfg.vault_dir,
                        &format!("ERROR: snapshots sync failed (rc={rc})"),
                    );
                    errors += 1;
                }
                Err(e) => {
                    log_sync(
                        &cfg.vault_dir,
                        &format!("ERROR: snapshots sync failed: {e}"),
                    );
                    errors += 1;
                }
            }
        } else {
            log_sync(&cfg.vault_dir, "SKIP: no snapshots to sync");
        }
    }

    // ── Sync lkg/ ───────────────────────────────────────────────
    let lkg_dir = cfg.vault_dir.join("lkg");
    if lkg_dir.is_dir() {
        let lkg_count = count_entries(&lkg_dir, true);
        if lkg_count > 0 {
            log_sync(
                &cfg.vault_dir,
                &format!("Syncing {lkg_count} LKG states..."),
            );

            let status = Command::new("rclone")
                .arg("sync")
                .arg(&lkg_dir)
                .arg(format!("{cloud_remote}:lkg/"))
                .args(&base_flags)
                .status();

            match status {
                Ok(s) if s.success() => {
                    log_sync(
                        &cfg.vault_dir,
                        &format!("OK: lkg synced ({lkg_count} states)"),
                    );
                }
                Ok(s) => {
                    let rc = s.code().unwrap_or(-1);
                    log_sync(&cfg.vault_dir, &format!("ERROR: lkg sync failed (rc={rc})"));
                    errors += 1;
                }
                Err(e) => {
                    log_sync(&cfg.vault_dir, &format!("ERROR: lkg sync failed: {e}"));
                    errors += 1;
                }
            }
        } else {
            log_sync(&cfg.vault_dir, "SKIP: no LKG states to sync");
        }
    }

    // ── Result ──────────────────────────────────────────────────
    if errors > 0 {
        log_sync(
            &cfg.vault_dir,
            &format!("DONE: Cloud sync completed with {errors} error(s)"),
        );
        crate::alert::send_alert(
            "WARNING",
            &format!("Cloud sync completed with {errors} error(s)"),
            &cfg,
        );
        1
    } else {
        log_sync(&cfg.vault_dir, "DONE: Cloud sync successful");
        0
    }
}

/// Count entries in a directory. If `dirs_only`, count only subdirectories;
/// otherwise count only files.
fn count_entries(dir: &Path, dirs_only: bool) -> usize {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            if dirs_only {
                e.path().is_dir()
            } else {
                e.path().is_file()
            }
        })
        .count()
}

/// Check I/O pressure by reading /proc/pressure/io directly.
/// Returns `true` if OK to proceed (pressure below threshold or unavailable).
fn check_io_pressure(threshold: f32) -> bool {
    let content = match fs::read_to_string("/proc/pressure/io") {
        Ok(c) => c,
        Err(_) => return true, // PSI unavailable, allow
    };

    // Parse the "some" line: "some avg10=X.XX avg60=X.XX avg300=X.XX total=NNN"
    for line in content.lines() {
        if line.starts_with("some ") {
            for part in line.split_whitespace() {
                if let Some(val_str) = part.strip_prefix("avg10=") {
                    if let Ok(pressure) = val_str.parse::<f32>() {
                        if pressure >= threshold {
                            log::warn!(
                                "I/O pressure high: {pressure:.1}% >= {threshold}%, deferring cloud sync"
                            );
                            return false;
                        }
                        return true;
                    }
                }
            }
        }
    }

    true // Could not parse, allow
}

/// Append a timestamped line to the cloud sync audit log.
fn log_sync(vault_dir: &Path, message: &str) {
    let ts = crate::alert::format_utc_now();
    let line = format!("[{ts}] {message}\n");
    log::info!("cloud-sync: {message}");

    let audit_dir = vault_dir.join("audit");
    let _ = fs::create_dir_all(&audit_dir);
    let log_path = audit_dir.join("cloud-sync.log");

    match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(mut f) => {
            let _ = f.write_all(line.as_bytes());
        }
        Err(e) => {
            log::error!("Cannot write to {}: {e}", log_path.display());
        }
    }
}
