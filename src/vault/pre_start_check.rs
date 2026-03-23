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

    let sqlite_path = cfg.openclaw_dir.join("memory/main.sqlite");
    let workspace_path = cfg.openclaw_dir.join("workspace");

    // ── Check 1: SQLite exists and is readable ────────────────────
    if !sqlite_path.exists() {
        log_pre_start(&format!(
            "FAIL: SQLite not found: {}",
            sqlite_path.display()
        ));
        failed = true;
    } else {
        // ── Check 2: SQLite structural integrity ──────────────────
        match check_sqlite_integrity(&sqlite_path) {
            Ok(integrity) => {
                if integrity != "ok" {
                    log_pre_start(&format!("FAIL: SQLite integrity check failed: {integrity}"));
                    failed = true;
                }
            }
            Err(e) => {
                log_pre_start(&format!(
                    "FAIL: Cannot open SQLite for integrity check: {e}"
                ));
                failed = true;
            }
        }

        // ── Check 3: SQLite has data ──────────────────────────────
        match count_chunks(&sqlite_path) {
            Ok(count) => {
                if count == 0 {
                    log_pre_start(
                        "WARNING: SQLite has 0 rows in chunks table (may be fresh install)",
                    );
                    // Warning only — don't fail on 0 rows
                } else {
                    log_pre_start(&format!("OK: SQLite has {count} rows in chunks"));
                }
            }
            Err(e) => {
                log_pre_start(&format!("FAIL: Cannot query chunks table: {e}"));
                failed = true;
            }
        }
    }

    // ── Check 4: Workspace directory exists ───────────────────────
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
        let row_info = count_chunks(&sqlite_path)
            .map(|n| format!("rows={n}"))
            .unwrap_or_else(|_| "rows=?".into());
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

/// Count rows in the `chunks` table.
fn count_chunks(path: &Path) -> Result<i64, String> {
    let conn = rusqlite::Connection::open(path)
        .map_err(|e| format!("Cannot open {}: {e}", path.display()))?;

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks;", [], |row| row.get(0))
        .map_err(|e| format!("SELECT COUNT(*) FROM chunks failed: {e}"))?;

    Ok(count)
}

/// Log a pre-start check message to stderr (journald) and the log module.
fn log_pre_start(message: &str) {
    eprintln!("PRE-START: {message}");
    log::info!("PRE-START: {message}");
}
