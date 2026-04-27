//! Gateway pre-start validation.
//!
//! Rust port of `scripts/pre-start-check.sh`. Validates that the SQLite
//! database exists, passes integrity checks, has data, and that the
//! workspace directory exists. Blocks gateway startup on failure.

use crate::alert::send_alert;
use crate::config::Config;
use crate::platform::Platform;
use std::path::Path;

/// Run pre-start validation. Returns 0 if all checks pass, 1 on any failure.
pub fn run(cfg: Config, _platform: Box<dyn Platform>) -> i32 {
    let mut failed = false;
    let workspace_path = cfg.openclaw_dir.join("workspace");
    let mut row_info = String::from("rows=?");

    // ── Check each known SQLite source ────────────────────────────
    // `main` is required; other sources are optional and skipped silently
    // when absent (older installs / freshly-bootstrapped state).
    for source in crate::vault::snapshot_sqlite::SQLITE_SOURCES {
        let sqlite_path = cfg.openclaw_dir.join(source.rel_path);

        match std::fs::metadata(&sqlite_path) {
            Ok(_) => {}
            Err(e) => match e.kind() {
                std::io::ErrorKind::NotFound => {
                    if source.label == "main" {
                        log_pre_start(&format!(
                            "FAIL: {} not found: {}",
                            source.label,
                            sqlite_path.display()
                        ));
                        failed = true;
                    } else {
                        log_pre_start(&format!(
                            "OK: {} not present (optional): {}",
                            source.label,
                            sqlite_path.display()
                        ));
                    }
                    continue;
                }
                std::io::ErrorKind::PermissionDenied => {
                    log_pre_start(&format!(
                        "FAIL: {} access denied (likely ACL mask drift; run: sudo outerclaw deploy): {}",
                        source.label,
                        sqlite_path.display()
                    ));
                    failed = true;
                    continue;
                }
                _ => {
                    log_pre_start(&format!("FAIL: Cannot stat {}: {e}", sqlite_path.display()));
                    failed = true;
                    continue;
                }
            },
        }

        // Integrity check
        match check_sqlite_integrity(&sqlite_path) {
            Ok(integrity) => {
                if integrity != "ok" {
                    log_pre_start(&format!(
                        "FAIL: {} integrity check failed: {integrity}",
                        source.label
                    ));
                    failed = true;
                }
            }
            Err(e) => {
                log_pre_start(&format!(
                    "FAIL: Cannot open {} for integrity check: {e}",
                    source.label
                ));
                failed = true;
            }
        }

        // Row-count sanity (only for sources with a count_table)
        if let Some(table) = source.count_table {
            match count_table_rows(&sqlite_path, table) {
                Ok(count) => {
                    if count == 0 {
                        log_pre_start(&format!(
                            "WARNING: {} has 0 rows in {table} (may be fresh install)",
                            source.label
                        ));
                    } else {
                        log_pre_start(&format!("OK: {} has {count} rows in {table}", source.label));
                    }
                    if source.label == "main" {
                        row_info = format!("rows={count}");
                    }
                }
                Err(e) => {
                    log_pre_start(&format!(
                        "FAIL: Cannot query {table} on {}: {e}",
                        source.label
                    ));
                    failed = true;
                }
            }
        }
    }

    // ── Workspace directory exists ────────────────────────────────
    if !workspace_path.is_dir() {
        log_pre_start(&format!(
            "FAIL: Workspace directory missing: {}",
            workspace_path.display()
        ));
        failed = true;
    }

    // ── Result ────────────────────────────────────────────────────
    if failed {
        log_pre_start("BLOCKED: Gateway start prevented due to validation failures");
        send_alert(
            "CRITICAL",
            "Gateway start BLOCKED: pre-start validation failed. Check: journalctl -u openclaw-gateway",
            &cfg,
        );
        1
    } else {
        log_pre_start(&format!("OK: Pre-start validation passed ({row_info})"));
        0
    }
}

/// Run `PRAGMA integrity_check` on a SQLite database.
fn check_sqlite_integrity(path: &Path) -> Result<String, String> {
    let conn = rusqlite::Connection::open(path)
        .map_err(|e| format!("Cannot open {}: {e}", path.display()))?;

    let result: String = conn
        .query_row("PRAGMA integrity_check;", [], |row| row.get(0))
        .map_err(|e| format!("integrity_check failed: {e}"))?;

    Ok(result)
}

/// Count rows in `table` of a SQLite database. Returns Err on any failure.
fn count_table_rows(path: &Path, table: &str) -> Result<i64, String> {
    if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("invalid table name: {table}"));
    }
    let conn = rusqlite::Connection::open(path)
        .map_err(|e| format!("Cannot open {}: {e}", path.display()))?;

    let count: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {table};"), [], |row| {
            row.get(0)
        })
        .map_err(|e| format!("SELECT COUNT(*) FROM {table} failed: {e}"))?;

    Ok(count)
}

/// Log a pre-start check message to stderr (journald) and the log module.
fn log_pre_start(message: &str) {
    eprintln!("PRE-START: {message}");
    log::info!("PRE-START: {message}");
}
