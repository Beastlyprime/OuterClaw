//! Alert module — Telegram notifications + local audit log.
//!
//! Replaces `scripts/alert.sh`.  Two responsibilities:
//! 1. Append to the local audit log (`vault_dir/audit/alerts.log`)
//! 2. Send to Telegram if a bot token is configured

use crate::config::Config;
use std::io::Write;
use std::path::Path;

/// Send an alert: log locally and (optionally) to Telegram.
///
/// This is the primary alert entry point used throughout the daemon.
pub fn send_alert(level: &str, message: &str, cfg: &Config) {
    // Always log locally
    log_to_audit(level, message, &cfg.vault_dir);

    // Send to Telegram if token is available
    if !cfg.tg_token.is_empty() {
        send_telegram(level, message, &cfg.tg_token, &cfg.tg_chat);
    }
}

/// Append an alert line to the audit log file.
///
/// Format: `YYYY-MM-DDTHH:MM:SSZ [LEVEL] message`
/// Path:   `vault_dir/audit/alerts.log`
///
/// Failures are logged but never fatal — alerting must be best-effort.
pub fn log_to_audit(level: &str, message: &str, vault_dir: &Path) {
    let audit_dir = vault_dir.join("audit");
    let log_path = audit_dir.join("alerts.log");

    // Ensure audit directory exists
    if !audit_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&audit_dir) {
            log::error!("Cannot create audit directory {audit_dir:?}: {e}");
            return;
        }
    }

    let timestamp = format_utc_now();
    let line = format!("{timestamp} [{level}] {message}\n");

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                log::error!("Failed to write to audit log: {e}");
            }
        }
        Err(e) => {
            log::error!("Cannot open audit log {log_path:?}: {e}");
        }
    }
}

/// Send an alert message to Telegram.
///
/// Uses the `sendMessage` API with Markdown formatting.
fn send_telegram(level: &str, message: &str, token: &str, chat_id: &str) {
    let icon = match level {
        "CRITICAL" => "[CRITICAL]",
        "WARNING" => "[WARNING]",
        "INFO" => "[INFO]",
        _ => "[ALERT]",
    };

    let text = format!("{icon} {message}");

    // Delegate to the telegram module's public helper
    crate::daemon::telegram::send_telegram_message(token, chat_id, &text);
}

/// Format the current UTC time as ISO 8601 (no external deps).
///
/// Output: `YYYY-MM-DDTHH:MM:SSZ`
///
/// This is public so the daemon can use it for proc JSON timestamps.
pub fn format_utc_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    epoch_to_iso(secs)
}

/// Convert epoch seconds to ISO 8601 string.
fn epoch_to_iso(secs: u64) -> String {
    let mut remaining = secs;

    let mut year = 1970u64;
    loop {
        let days_in_year: u64 = if is_leap(year) { 366 } else { 365 };
        let secs_in_year = days_in_year * 86400;
        if remaining < secs_in_year {
            break;
        }
        remaining -= secs_in_year;
        year += 1;
    }

    let month_days: [u64; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1u64;
    for &md in &month_days {
        let secs_in_month = md * 86400;
        if remaining < secs_in_month {
            break;
        }
        remaining -= secs_in_month;
        month += 1;
    }

    let day = remaining / 86400 + 1;
    remaining %= 86400;
    let hour = remaining / 3600;
    remaining %= 3600;
    let minute = remaining / 60;
    let second = remaining % 60;

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn is_leap(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_utc_now_looks_like_iso() {
        let ts = format_utc_now();
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
        assert_eq!(ts.len(), 20); // YYYY-MM-DDTHH:MM:SSZ
    }

    #[test]
    fn test_epoch_to_iso_known() {
        // 2024-01-01T00:00:00Z = 1704067200
        assert_eq!(epoch_to_iso(1704067200), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn test_epoch_to_iso_epoch_zero() {
        assert_eq!(epoch_to_iso(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn test_log_to_audit_creates_file() {
        let dir = std::env::temp_dir().join("outerclaw_alert_test");
        let _ = std::fs::remove_dir_all(&dir);
        log_to_audit("INFO", "test message", &dir);

        let log_path = dir.join("audit").join("alerts.log");
        assert!(log_path.exists());
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("[INFO]"));
        assert!(content.contains("test message"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
