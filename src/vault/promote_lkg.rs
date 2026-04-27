//! Promote latest snapshot to Last Known Good (LKG).
//!
//! Rust port of `scripts/promote-lkg.sh`. Validates gateway health and
//! uptime, copies the latest snapshot into a versioned LKG directory,
//! verifies SQLite integrity and row-count regression, validates the
//! file manifest, updates the `lkg/current` symlink, and prunes old LKGs.

use crate::alert::send_alert;
use crate::config::Config;
use crate::platform::{Platform, ServiceActive};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Maximum number of LKG entries to retain.
const MAX_LKGS: usize = 10;

/// Minimum gateway uptime (seconds) required before promotion.
const MIN_UPTIME_SECS: u64 = 1800;

/// Run the LKG promotion pipeline. Returns 0 on success, 1 on failure.
pub fn run(cfg: Config, platform: Box<dyn Platform>) -> i32 {
    match run_inner(&cfg, platform.as_ref()) {
        Ok(()) => 0,
        Err(e) => {
            log::error!("LKG promotion failed: {e}");
            1
        }
    }
}

fn run_inner(cfg: &Config, platform: &dyn Platform) -> Result<(), String> {
    let lkg_dir = cfg.vault_dir.join("lkg");
    let snap_dir = cfg.vault_dir.join("snapshots");
    let audit_dir = cfg.vault_dir.join("audit");
    fs::create_dir_all(&lkg_dir).map_err(|e| format!("Cannot create LKG dir: {e}"))?;
    fs::create_dir_all(&audit_dir).map_err(|e| format!("Cannot create audit dir: {e}"))?;

    // ── Pre-check 1: Gateway must be active ────────────────────────
    let state = platform.service_state(&cfg.gateway_service)?;
    if state != ServiceActive::Active {
        let msg = format!("Gateway not active (state: {state}), refusing LKG promotion");
        log_lkg(&audit_dir, &format!("FAIL: {msg}"));
        eprintln!("ERROR: {msg}");
        return Err(msg);
    }

    // ── Pre-check 2: Uptime >= MIN_UPTIME_SECS ────────────────────
    match platform.service_uptime_secs(&cfg.gateway_service)? {
        Some(uptime) if uptime < MIN_UPTIME_SECS => {
            let msg = format!("Gateway uptime {uptime}s < required {MIN_UPTIME_SECS}s");
            log_lkg(&audit_dir, &format!("FAIL: {msg}"));
            eprintln!("ERROR: {msg}");
            eprintln!(
                "       Wait {}s more, or reduce MIN_UPTIME_SECS.",
                MIN_UPTIME_SECS - uptime
            );
            return Err(msg);
        }
        Some(uptime) => {
            println!("Gateway uptime: {uptime}s (>= {MIN_UPTIME_SECS}s required)");
        }
        None => {
            log::warn!("Could not determine gateway uptime, proceeding anyway");
        }
    }

    // ── Pre-check 3: Health check must pass ────────────────────────
    if !crate::daemon::health_checker::check_health(&cfg.health_url, cfg.health_timeout) {
        let msg = "Health check failed, refusing LKG promotion";
        log_lkg(&audit_dir, &format!("FAIL: {msg}"));
        eprintln!("ERROR: {msg}");
        return Err(msg.into());
    }
    println!("Health check: passed");

    // ── Find latest file snapshot (independent of SQLite) ──────────
    let latest_files =
        super::snapshot_sqlite::find_latest_files_snapshot(&snap_dir).ok_or_else(|| {
            let msg = format!("No file snapshots found in {}", snap_dir.display());
            log_lkg(&audit_dir, &format!("FAIL: {msg}"));
            msg
        })?;

    println!("Promoting to LKG:");
    println!("  Files:  {}", latest_files.display());

    // ── Find + validate latest snapshot for each SQLite source ────
    // `main` is required; others are optional (a recent snapshot must exist
    // in the snapshots dir, otherwise we skip it for this LKG).
    let mut sources_to_promote: Vec<(&'static str, PathBuf)> = Vec::new();
    for source in super::snapshot_sqlite::SQLITE_SOURCES {
        match super::snapshot_sqlite::find_latest_snapshot_by_label(&snap_dir, source.label) {
            Some(p) => {
                println!("  SQLite[{}]: {}", source.label, p.display());
                sources_to_promote.push((source.label, p));
            }
            None => {
                if source.label == "main" {
                    let msg = format!(
                        "No {} snapshot found in {}",
                        source.label,
                        snap_dir.display()
                    );
                    log_lkg(&audit_dir, &format!("FAIL: {msg}"));
                    return Err(msg);
                }
                log::info!("No {} snapshot present, skipping (optional)", source.label);
            }
        }
    }

    // ── Create LKG directory ───────────────────────────────────────
    let ts = timestamp_str();
    let lkg_snap = lkg_dir.join(format!("lkg-{ts}"));
    fs::create_dir_all(&lkg_snap)
        .map_err(|e| format!("Cannot create {}: {e}", lkg_snap.display()))?;

    let current_link = lkg_dir.join("current");

    // ── Copy + verify each SQLite source ───────────────────────────
    for (label, latest_sql) in &sources_to_promote {
        let lkg_sqlite = lkg_snap.join(format!("{label}.sqlite"));
        fs::copy(latest_sql, &lkg_sqlite).map_err(|e| {
            let _ = fs::remove_dir_all(&lkg_snap);
            format!("Failed to copy {} snapshot: {e}", label)
        })?;

        // Integrity check
        {
            let conn = rusqlite::Connection::open(&lkg_sqlite).map_err(|e| {
                let _ = fs::remove_dir_all(&lkg_snap);
                format!("Cannot open LKG {label} for integrity check: {e}")
            })?;
            let integrity: String = conn
                .query_row("PRAGMA integrity_check;", [], |row| row.get(0))
                .map_err(|e| {
                    let _ = fs::remove_dir_all(&lkg_snap);
                    format!("integrity_check query failed for {label}: {e}")
                })?;
            if integrity != "ok" {
                let _ = fs::remove_dir_all(&lkg_snap);
                let msg = format!("{label} integrity check failed: {integrity}");
                log_lkg(&audit_dir, &format!("FAIL: {msg}"));
                send_alert("CRITICAL", &format!("LKG promotion FAILED: {msg}"), cfg);
                return Err(msg);
            }
        }

        // Row-count regression — only for sources with a count_table (main).
        let count_table = super::snapshot_sqlite::SQLITE_SOURCES
            .iter()
            .find(|s| s.label == *label)
            .and_then(|s| s.count_table);
        if let Some(table) = count_table {
            let new_rows = super::snapshot_sqlite::count_rows(&lkg_sqlite, table);
            let current_lkg_db = current_link.join(format!("{label}.sqlite"));
            if current_lkg_db.exists() {
                let old_rows = super::snapshot_sqlite::count_rows(&current_lkg_db, table);
                if old_rows > 0 && new_rows > 0 && new_rows < old_rows {
                    let drop_pct = ((old_rows - new_rows) * 100) / old_rows;
                    if drop_pct > 50 {
                        let _ = fs::remove_dir_all(&lkg_snap);
                        let msg = format!(
                            "{label} row count regression {drop_pct}% ({old_rows} -> {new_rows})"
                        );
                        log_lkg(&audit_dir, &format!("FAIL: {msg}"));
                        send_alert(
                            "CRITICAL",
                            &format!(
                                "LKG promotion REFUSED: {label} row count dropped {drop_pct}% ({old_rows} -> {new_rows})"
                            ),
                            cfg,
                        );
                        eprintln!(
                            "ERROR: {label} row count dropped {drop_pct}% ({old_rows} -> {new_rows}). \
                             Refusing to promote possibly corrupted state."
                        );
                        return Err(msg);
                    }
                }
                println!("  {label} rows: {new_rows} (previous LKG: {old_rows})");
            } else {
                println!("  {label} rows: {new_rows}");
            }
        }
    }

    // ── Copy files from latest files snapshot ──────────────────────
    copy_dir_contents(&latest_files, &lkg_snap)
        .map_err(|e| {
            log::warn!("Failed to copy some files from snapshot: {e}");
            e
        })
        .ok();

    // ── Verify MANIFEST.sha256 if present ──────────────────────────
    let manifest_path = lkg_snap.join("MANIFEST.sha256");
    if manifest_path.exists() {
        if let Err(e) = verify_manifest(&lkg_snap, &manifest_path) {
            let _ = fs::remove_dir_all(&lkg_snap);
            let msg = format!("Manifest verification failed: {e}");
            log_lkg(&audit_dir, &format!("FAIL: {msg}"));
            send_alert(
                "CRITICAL",
                "LKG promotion FAILED: manifest verification failed",
                cfg,
            );
            return Err(msg);
        }
    }

    // ── Update symlink: lkg/current -> lkg/lkg-{ts} ───────────────
    // Remove old symlink (or file/dir) if it exists
    if current_link.exists() || current_link.symlink_metadata().is_ok() {
        let _ = fs::remove_file(&current_link);
    }
    std::os::unix::fs::symlink(&lkg_snap, &current_link).map_err(|e| {
        format!(
            "Failed to create symlink {} -> {}: {e}",
            current_link.display(),
            lkg_snap.display()
        )
    })?;

    // ── Prune old LKGs (keep MAX_LKGS) ────────────────────────────
    prune_lkgs(&lkg_dir, MAX_LKGS);

    // ── Log (local only — no Telegram for routine success) ─────────
    log_lkg(&audit_dir, &format!("OK: {}", lkg_snap.display()));

    println!();
    println!("LKG promoted: {}", lkg_snap.display());
    println!(
        "   Symlink: {} -> {}",
        current_link.display(),
        lkg_snap.display()
    );

    Ok(())
}

/// Copy all files and subdirectories from `src` into `dst`.
fn copy_dir_contents(src: &Path, dst: &Path) -> Result<(), String> {
    let entries = fs::read_dir(src).map_err(|e| format!("Cannot read {}: {e}", src.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);

        let ft = entry
            .file_type()
            .map_err(|e| format!("file_type {}: {e}", src_path.display()))?;

        if ft.is_dir() {
            fs::create_dir_all(&dst_path)
                .map_err(|e| format!("mkdir {}: {e}", dst_path.display()))?;
            copy_dir_contents(&src_path, &dst_path)?;
        } else if ft.is_file() {
            fs::copy(&src_path, &dst_path).map_err(|e| {
                format!("copy {} -> {}: {e}", src_path.display(), dst_path.display())
            })?;
        }
    }

    Ok(())
}

/// Verify a SHA-256 manifest file. Each line: `<hash>  <relative_path>`
fn verify_manifest(base_dir: &Path, manifest_path: &Path) -> Result<(), String> {
    let content =
        fs::read_to_string(manifest_path).map_err(|e| format!("Cannot read manifest: {e}"))?;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Format: "<hash>  <path>" or "<hash> <path>"
        let parts: Vec<&str> = line.splitn(2, |c: char| c.is_whitespace()).collect();
        if parts.len() < 2 {
            continue;
        }
        let expected_hash = parts[0].trim();
        let rel_path = parts[1].trim();

        // Skip the manifest file itself
        if rel_path == "MANIFEST.sha256" {
            continue;
        }

        let file_path = base_dir.join(rel_path);
        if !file_path.exists() {
            return Err(format!("File from manifest missing: {rel_path}"));
        }

        let actual_hash = sha256_file(&file_path)?;
        if actual_hash != expected_hash {
            return Err(format!(
                "Hash mismatch for {rel_path}: expected {expected_hash}, got {actual_hash}"
            ));
        }
    }

    Ok(())
}

/// Compute SHA-256 of a file, returning the hex digest.
fn sha256_file(path: &Path) -> Result<String, String> {
    let data =
        fs::read(path).map_err(|e| format!("Cannot read {} for SHA-256: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Prune LKG directories, keeping only the newest `keep` entries.
fn prune_lkgs(lkg_dir: &Path, keep: usize) {
    let mut entries: Vec<PathBuf> = fs::read_dir(lkg_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("lkg-"))
                    .unwrap_or(false)
        })
        .collect();

    // Sort descending by name (newest first)
    entries.sort();
    entries.reverse();

    for old in entries.into_iter().skip(keep) {
        log::debug!("Pruning old LKG: {}", old.display());
        let _ = fs::remove_dir_all(&old);
    }
}

/// Append a line to the LKG audit log.
fn log_lkg(audit_dir: &Path, message: &str) {
    let ts = crate::alert::format_utc_now();
    let line = format!("[{ts}] {message}\n");
    log::info!("{message}");
    let _ = fs::create_dir_all(audit_dir);
    let log_path = audit_dir.join("lkg.log");
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

/// Generate a local-time timestamp in `YYYYMMDD-HHMMSS` format.
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
