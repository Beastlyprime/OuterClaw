//! Status report — port of Python `show_status()`.
//!
//! Reads the proc JSON, queries service state and uptime, and prints a
//! human-friendly guardian status report to stdout.

use crate::config::Config;
use crate::platform::Platform;
use crate::util::time_fmt::{fmt_ago, fmt_uptime};
use std::path::Path;

/// Print the guardian status report and return exit code.
pub fn show(cfg: Config, platform: Box<dyn Platform>) -> i32 {
    let vault = &cfg.vault_dir;
    let snap_dir = vault.join("snapshots");
    let lkg_dir = vault.join("lkg");
    let audit_dir = vault.join("audit");
    let proc_json = audit_dir.join("gateway-proc-latest.json");

    // ── Header ──
    let version = env!("CARGO_PKG_VERSION");
    println!("{}", "=".repeat(42));
    println!("     OuterClaw Guardian v{version}");
    println!("{}", "=".repeat(42));
    println!();

    // ── Gateway state from proc JSON ──
    let mut state = "UNKNOWN".to_string();
    let mut pid = "N/A".to_string();
    let mut proc_ts: f64 = 0.0;

    if proc_json.exists() {
        if let Ok(text) = std::fs::read_to_string(&proc_json) {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(s) = data.get("outerclaw_state").and_then(|v| v.as_str()) {
                    state = s.to_string();
                }
                if let Some(p) = data.get("pid") {
                    pid = p.to_string().trim_matches('"').to_string();
                }
                if let Some(ts_str) = data.get("timestamp").and_then(|v| v.as_str()) {
                    // ISO 8601 timestamp -> epoch
                    proc_ts = parse_iso_timestamp(ts_str);
                }
            }
        }
    }

    let state_icon = match state.as_str() {
        "HEALTHY" => "[OK]",
        "DOWN" | "CONFIRMED_HANG" | "ZOMBIE" => "[!!]",
        _ => "[??]",
    };

    // ── Gateway uptime ──
    let uptime_str = match platform.service_uptime_secs(&cfg.gateway_service) {
        Ok(Some(secs)) if secs > 0 => fmt_uptime(secs),
        _ => "unknown".into(),
    };

    println!("  System        {state_icon} {state}");
    println!("  Gateway       PID {pid}");
    println!("  Uptime        {uptime_str}");
    if proc_ts > 0.0 {
        println!("  Last check    {}", fmt_ago(proc_ts));
    }
    println!();

    // ── Data Protection ──
    println!("  Data Protection");

    // Latest snapshot
    let (snap_count, latest_snap_ts) = count_snapshots(&snap_dir);
    let snap_ago = if latest_snap_ts > 0.0 {
        fmt_ago(latest_snap_ts)
    } else {
        "never".into()
    };
    println!("  +-- Last backup     {snap_ago}");
    println!("  +-- Backup count    {snap_count}");

    // LKG
    let lkg_count = count_lkg(&lkg_dir);
    println!("  +-- LKG states      {lkg_count} available");

    // Integrity check on latest snapshot
    let integrity = check_latest_integrity(&snap_dir);
    let check = if integrity == "passed" { "OK" } else { "FAIL" };
    println!("  +-- Integrity       [{check}] {integrity}");
    println!();

    // ── Security ──
    println!("  Security");

    // Three-user isolation
    let users_ok = platform.user_exists(&cfg.watchdog_user).unwrap_or(false);
    let agent_ok = platform.user_exists(&cfg.agent_user).unwrap_or(false);
    let isolation_ok = users_ok && agent_ok;
    let isolation_label = if isolation_ok {
        "3-user isolation active"
    } else {
        "missing users"
    };
    println!(
        "  +-- User isolation  [{}] {isolation_label}",
        if isolation_ok { "OK" } else { "!!" }
    );

    // Identity files immutable
    let (immutable_count, total_identity) = check_identity_immutable(&cfg.openclaw_dir);
    let immutable_ok = immutable_count == total_identity && total_identity > 0;
    println!(
        "  +-- Identity files  [{}] {immutable_count}/{total_identity} immutable",
        if immutable_ok { "OK" } else { "!!" }
    );

    // Config monitoring (is the daemon running?)
    let watcher_active = matches!(
        platform.service_state("oc-outerclaw.service"),
        Ok(crate::platform::ServiceActive::Active)
    );
    let watcher_label = if watcher_active {
        "file watcher active"
    } else {
        "inactive"
    };
    println!(
        "  +-- Config monitor  [{}] {watcher_label}",
        if watcher_active { "OK" } else { "!!" }
    );

    // Telegram
    let tg_configured = !cfg.tg_token.is_empty() && !cfg.tg_chat.is_empty();
    let tg_label = if tg_configured {
        "connected"
    } else {
        "not configured"
    };
    println!(
        "  +-- Alert channel   [{}] Telegram {tg_label}",
        if tg_configured { "OK" } else { "!!" }
    );
    println!();

    0
}

/// Count snapshot SQLite files and return (count, latest_mtime_epoch).
fn count_snapshots(snap_dir: &Path) -> (usize, f64) {
    let mut count = 0;
    let mut latest: f64 = 0.0;

    if let Ok(entries) = std::fs::read_dir(snap_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("main-") && name.ends_with(".sqlite") {
                count += 1;
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        let ts = mtime
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs_f64();
                        if ts > latest {
                            latest = ts;
                        }
                    }
                }
            }
        }
    }

    (count, latest)
}

/// Count LKG directories.
fn count_lkg(lkg_dir: &Path) -> usize {
    std::fs::read_dir(lkg_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("lkg-"))
                .count()
        })
        .unwrap_or(0)
}

/// Run `sqlite3 PRAGMA integrity_check` on the latest snapshot.
fn check_latest_integrity(snap_dir: &Path) -> String {
    let mut files: Vec<_> = std::fs::read_dir(snap_dir)
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("main-") && name.ends_with(".sqlite")
        })
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), mtime))
        })
        .collect();

    files.sort_by(|a, b| b.1.cmp(&a.1));

    let latest = match files.first() {
        Some((path, _)) => path,
        None => return "unknown".into(),
    };

    match std::process::Command::new("sqlite3")
        .arg(latest)
        .arg("PRAGMA integrity_check;")
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.trim() == "ok" {
                "passed".into()
            } else {
                "FAILED".into()
            }
        }
        Err(_) => "unknown".into(),
    }
}

/// Check how many identity files have the immutable flag set.
fn check_identity_immutable(openclaw_dir: &Path) -> (usize, usize) {
    let workspace = openclaw_dir.join("workspace");
    let identity_files = ["SOUL.md", "AGENTS.md", "USER.md"];
    let mut immutable = 0;
    let mut total = 0;

    for name in &identity_files {
        let path = workspace.join(name);
        if !path.exists() {
            continue;
        }
        total += 1;
        // Use lsattr to check immutable flag
        if let Ok(output) = std::process::Command::new("lsattr").arg(&path).output() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(flags) = stdout.split_whitespace().next() {
                if flags.contains('i') {
                    immutable += 1;
                }
            }
        }
    }

    (immutable, total)
}

/// Parse an ISO 8601 timestamp string to epoch seconds.
///
/// Handles the common format "2024-01-15T12:34:56.789Z" or with +00:00 offset.
/// Returns 0.0 on parse failure (non-critical, just means "unknown").
fn parse_iso_timestamp(ts: &str) -> f64 {
    // Simple parser: split at T, parse date and time manually
    // This avoids pulling in chrono for one call.
    let ts = ts.trim_end_matches('Z');
    let ts = if let Some(pos) = ts.rfind('+') {
        &ts[..pos]
    } else if let Some(pos) = ts.rfind('-') {
        // But careful: don't strip the date dash
        if pos > 10 {
            &ts[..pos]
        } else {
            ts
        }
    } else {
        ts
    };

    let parts: Vec<&str> = ts.splitn(2, 'T').collect();
    if parts.len() != 2 {
        return 0.0;
    }

    let date_parts: Vec<u64> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    if date_parts.len() != 3 {
        return 0.0;
    }

    let time_str = parts[1].split('.').next().unwrap_or(parts[1]);
    let time_parts: Vec<u64> = time_str.split(':').filter_map(|p| p.parse().ok()).collect();
    if time_parts.len() != 3 {
        return 0.0;
    }

    // Approximate: days since epoch (not accounting for leap seconds, but good enough)
    let year = date_parts[0];
    let month = date_parts[1];
    let day = date_parts[2];
    let hour = time_parts[0];
    let min = time_parts[1];
    let sec = time_parts[2];

    // Simple days-since-epoch calculation
    let mut days: i64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize] as i64;
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day as i64 - 1;

    (days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64) as f64
}

fn is_leap(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_iso_timestamp() {
        // 2024-01-01T00:00:00Z should be some known epoch
        let ts = parse_iso_timestamp("2024-01-01T00:00:00Z");
        assert!(ts > 1_700_000_000.0); // sanity: after ~2023-11
        assert!(ts < 1_710_000_000.0); // sanity: before ~2024-03
    }

    #[test]
    fn test_parse_iso_timestamp_invalid() {
        assert_eq!(parse_iso_timestamp("not-a-date"), 0.0);
        assert_eq!(parse_iso_timestamp(""), 0.0);
    }

    #[test]
    fn test_is_leap() {
        assert!(is_leap(2000));
        assert!(is_leap(2024));
        assert!(!is_leap(1900));
        assert!(!is_leap(2023));
    }
}
