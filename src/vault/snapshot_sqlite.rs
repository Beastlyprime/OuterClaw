//! Atomic SQLite snapshot via VACUUM INTO.
//!
//! Port of `scripts/snapshot-sqlite.sh`. Performs an atomic `VACUUM INTO`
//! backup of the OpenClaw SQLite database, verifies integrity, detects
//! row-count regressions, computes SHA-256, prunes old snapshots, and logs
//! everything to the audit trail.

use crate::alert::send_alert;
use crate::config::Config;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Maximum number of SQLite snapshots to retain (96 = 48 hours at 30-min intervals).
const MAX_SNAPSHOTS: usize = 96;

/// A SQLite database OuterClaw watches.
///
/// `label` is used as the snapshot filename prefix (e.g. `main-<ts>.sqlite`).
/// `rel_path` is relative to `OPENCLAW_DIR`. `count_table` enables row-count
/// regression detection — set to `None` for databases without a stable
/// integrity-tracking table (their snapshots still get integrity_check + SHA).
pub struct SqliteSource {
    pub label: &'static str,
    pub rel_path: &'static str,
    pub count_table: Option<&'static str>,
}

/// Databases watched by OuterClaw. Order matters: `main` is required, all
/// others are optional (skipped without error if missing).
pub const SQLITE_SOURCES: &[SqliteSource] = &[
    SqliteSource {
        label: "main",
        rel_path: "memory/main.sqlite",
        count_table: Some("chunks"),
    },
    SqliteSource {
        label: "runs",
        rel_path: "tasks/runs.sqlite",
        count_table: None,
    },
];

/// Run the SQLite snapshot pipeline. Returns 0 on success, 1 on failure.
pub fn run_sqlite(cfg: &Config) -> i32 {
    match run_inner(cfg) {
        Ok(()) => 0,
        Err(e) => {
            log::error!("SQLite snapshot failed: {e}");
            send_alert("CRITICAL", &format!("SQLite snapshot failed: {e}"), cfg);
            1
        }
    }
}

fn run_inner(cfg: &Config) -> Result<(), String> {
    // ── Preflight: vault disk quota ────────────────────────────────
    if !super::quota::check(&cfg.vault_dir, cfg.max_vault_mb) {
        log_backup(&cfg.vault_dir, "SKIP: Vault over quota, snapshot deferred");
        return Ok(());
    }

    let dst_dir = cfg.vault_dir.join("snapshots");
    ensure_dir(&dst_dir)?;

    let audit_dir = cfg.vault_dir.join("audit");
    ensure_dir(&audit_dir)?;

    let ts = timestamp_str();
    let mut errors: Vec<String> = Vec::new();

    for source in SQLITE_SOURCES {
        let src = cfg.openclaw_dir.join(source.rel_path);
        match snapshot_one(&src, &dst_dir, &audit_dir, &ts, source, cfg) {
            Ok(true) => {}
            Ok(false) => {
                // Source is genuinely missing (ENOENT).
                if source.label == "main" {
                    let msg = format!("Source {} does not exist", src.display());
                    log_backup(&cfg.vault_dir, &format!("FAIL: {}: {msg}", source.label));
                    errors.push(format!("{}: {msg}", source.label));
                } else {
                    log_backup(
                        &cfg.vault_dir,
                        &format!("SKIP: {} (not present at {})", source.label, src.display()),
                    );
                }
            }
            Err(e) => {
                log_backup(&cfg.vault_dir, &format!("FAIL: {}: {e}", source.label));
                errors.push(format!("{}: {e}", source.label));
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(())
}

/// Snapshot a single SQLite database.
///
/// Returns `Ok(true)` on success, `Ok(false)` if the source is genuinely
/// missing (ENOENT), `Err(_)` for any other failure (including EACCES, which
/// is reported with an ACL-drift hint to ease diagnosis).
fn snapshot_one(
    src: &Path,
    dst_dir: &Path,
    audit_dir: &Path,
    ts: &str,
    source: &SqliteSource,
    cfg: &Config,
) -> Result<bool, String> {
    // Distinguish "really missing" from "permission denied" — the latter
    // typically indicates ACL mask drift (POSIX ACL mask collapses to ---
    // when group bits are cleared by chmod), not a missing data file.
    match fs::metadata(src) {
        Ok(_) => {}
        Err(e) => {
            return match e.kind() {
                std::io::ErrorKind::NotFound => Ok(false),
                std::io::ErrorKind::PermissionDenied => Err(format!(
                    "{} access denied (likely ACL mask drift; run: sudo outerclaw deploy)",
                    src.display()
                )),
                _ => Err(format!("Cannot stat {}: {e}", src.display())),
            };
        }
    }

    let dst = dst_dir.join(format!("{}-{ts}.sqlite", source.label));
    let dst_tmp = dst_dir.join(format!("{}-{ts}.sqlite.tmp", source.label));

    // ── VACUUM INTO ────────────────────────────────────────────────
    {
        let conn =
            rusqlite::Connection::open(src).map_err(|e| format!("Cannot open source DB: {e}"))?;
        let vacuum_sql = format!("VACUUM INTO '{}'", dst_tmp.display());
        conn.execute_batch(&vacuum_sql).map_err(|e| {
            let _ = fs::remove_file(&dst_tmp);
            format!("VACUUM INTO failed: {e}")
        })?;
    }

    // ── Integrity check ────────────────────────────────────────────
    {
        let conn = rusqlite::Connection::open(&dst_tmp)
            .map_err(|e| format!("Cannot open backup for integrity check: {e}"))?;
        let integrity: String = conn
            .query_row("PRAGMA integrity_check;", [], |row| row.get(0))
            .map_err(|e| format!("integrity_check query failed: {e}"))?;
        if integrity != "ok" {
            let _ = fs::remove_file(&dst_tmp);
            return Err(format!("Integrity check failed: {integrity}"));
        }
    }

    // ── Row-count regression (only for sources with count_table) ──
    let row_count = source.count_table.map(|tbl| count_rows(&dst_tmp, tbl));
    if let (Some(table), Some(count)) = (source.count_table, row_count) {
        if count == 0 {
            log_backup(
                &cfg.vault_dir,
                &format!(
                    "WARNING: {} backup has 0 rows in {table} (may be OK for fresh install)",
                    source.label
                ),
            );
        }
        if let Some(prev) = find_latest_snapshot_by_label(dst_dir, source.label) {
            let prev_count = count_rows(&prev, table);
            if prev_count > 0 && count > 0 && count < prev_count {
                let drop_pct = ((prev_count - count) * 100) / prev_count;
                if drop_pct > 50 {
                    let msg = format!(
                        "{} row count dropped {drop_pct}% ({prev_count} -> {count}). Possible data loss.",
                        source.label
                    );
                    log_backup(&cfg.vault_dir, &format!("WARNING: {msg}"));
                    send_alert("WARNING", &msg, cfg);
                }
            }
        }
    }

    // ── Atomic rename ──────────────────────────────────────────────
    fs::rename(&dst_tmp, &dst).map_err(|e| format!("Atomic rename failed: {e}"))?;

    // ── SHA-256 ────────────────────────────────────────────────────
    let sha = sha256_file(&dst)?;
    let hash_line = format!("{ts} {sha} {}\n", dst.display());
    append_file(&audit_dir.join("sqlite-hashes.log"), &hash_line);

    // ── Log success ────────────────────────────────────────────────
    let size = fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
    let size_human = format_size(size);
    let row_info = match row_count {
        Some(n) => format!(" rows={n}"),
        None => String::new(),
    };
    log_backup(
        &cfg.vault_dir,
        &format!(
            "OK: {} ({}) size={size_human}{row_info}",
            dst.display(),
            &sha[..16],
        ),
    );

    // ── Prune (per-label) ──────────────────────────────────────────
    prune_sqlite_snapshots_by_label(dst_dir, source.label);

    Ok(true)
}

/// Generate a local-time timestamp in `YYYYMMDD-HHMMSS` format using libc.
fn timestamp_str() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    #[allow(clippy::unnecessary_cast)]
    let t = secs as libc::time_t;
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

/// Count rows in `table` of a SQLite database. Returns 0 on any error
/// (including missing-table), so callers can use this as a soft sanity check.
pub fn count_rows(db_path: &Path, table: &str) -> u64 {
    // table names cannot be parameterized in SQLite — validate to keep this
    // safe to interpolate.
    if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return 0;
    }
    rusqlite::Connection::open(db_path)
        .ok()
        .and_then(|conn| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table};"), [], |row| {
                row.get::<_, i64>(0)
            })
            .ok()
        })
        .map(|n| n.max(0) as u64)
        .unwrap_or(0)
}

/// Backwards-compatible alias for `count_rows(path, "chunks")`. The chunks
/// table is the row-count proxy for the main database.
pub fn count_chunks(db_path: &Path) -> u64 {
    count_rows(db_path, "chunks")
}

/// Find the most recent existing `<label>-*.sqlite` file in the snapshots dir.
pub fn find_latest_snapshot_by_label(dir: &Path, label: &str) -> Option<PathBuf> {
    let prefix = format!("{label}-");
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix) && n.ends_with(".sqlite") && !n.ends_with(".tmp"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    entries.pop()
}

/// Find the most recent `files-*` directory in the snapshots dir.
pub fn find_latest_files_snapshot(dir: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("files-"))
                    .unwrap_or(false)
        })
        .collect();
    entries.sort();
    entries.pop()
}

/// Compute SHA-256 of a file, returning the hex digest.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let data =
        fs::read(path).map_err(|e| format!("Cannot read {} for SHA-256: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Keep only the newest `MAX_SNAPSHOTS` sqlite files for `label`, remove the rest.
fn prune_sqlite_snapshots_by_label(dir: &Path, label: &str) {
    let prefix = format!("{label}-");
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix) && n.ends_with(".sqlite") && !n.ends_with(".tmp"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    entries.reverse();
    for old in entries.into_iter().skip(MAX_SNAPSHOTS) {
        log::debug!("Pruning old SQLite snapshot: {}", old.display());
        let _ = fs::remove_file(&old);
    }
}

/// Append a line to the backup audit log.
fn log_backup(vault_dir: &Path, message: &str) {
    let ts = crate::alert::format_utc_now();
    let line = format!("[{ts}] {message}\n");
    log::info!("{message}");
    let audit_dir = vault_dir.join("audit");
    let _ = fs::create_dir_all(&audit_dir);
    append_file(&audit_dir.join("backup.log"), &line);
}

/// Append text to a file, creating it if necessary. Best-effort.
fn append_file(path: &Path, content: &str) {
    match fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => {
            let _ = f.write_all(content.as_bytes());
        }
        Err(e) => {
            log::error!("Cannot append to {}: {e}", path.display());
        }
    }
}

/// Ensure a directory exists.
fn ensure_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("Cannot create directory {}: {e}", dir.display()))
}

/// Format byte size as a human-readable string.
fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1}G", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1}M", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}
