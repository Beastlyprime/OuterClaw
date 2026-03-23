//! Automated LKG recovery triggered by the daemon on persistent failures.
//!
//! Rust port of `scripts/auto-recover.sh`. Validates the LKG, stops the
//! gateway, takes an emergency snapshot of the (potentially corrupt) state,
//! restores SQLite + MEMORY.md + memory/ from LKG, restarts the gateway,
//! and verifies it comes back up.

use crate::alert::send_alert;
use crate::config::Config;
use crate::platform::{Platform, ServiceActive};
use std::fs;
use std::path::{Path, PathBuf};

/// Run automated LKG recovery. Returns 0 on success, 1 on failure.
pub fn run(cfg: Config, platform: Box<dyn Platform>) -> i32 {
    match run_inner(&cfg, platform.as_ref()) {
        Ok(()) => 0,
        Err(e) => {
            log::error!("Auto-recovery FAILED: {e}");
            send_alert(
                "CRITICAL",
                &format!("Auto-recovery FAILED: {e}. Manual intervention required."),
                &cfg,
            );
            1
        }
    }
}

fn run_inner(cfg: &Config, platform: &dyn Platform) -> Result<(), String> {
    let lkg_dir = cfg.vault_dir.join("lkg");
    let lkg_current = lkg_dir.join("current");
    let audit_dir = cfg.vault_dir.join("audit");
    fs::create_dir_all(&audit_dir).map_err(|e| format!("Cannot create audit dir: {e}"))?;

    // ── Step 1: Resolve LKG ───────────────────────────────────────
    let lkg = fs::read_link(&lkg_current)
        .or_else(|_| fs::canonicalize(&lkg_current))
        .map_err(|e| format!("No LKG available at {}: {e}", lkg_current.display()))?;

    if !lkg.is_dir() {
        return Err(format!("LKG path is not a directory: {}", lkg.display()));
    }

    let lkg_sqlite = lkg.join("main.sqlite");
    if !lkg_sqlite.exists() {
        return Err(format!("No SQLite in LKG: {}", lkg_sqlite.display()));
    }

    // ── Step 2: Validate LKG SQLite integrity ─────────────────────
    {
        let conn = rusqlite::Connection::open(&lkg_sqlite)
            .map_err(|e| format!("Cannot open LKG SQLite: {e}"))?;
        let integrity: String = conn
            .query_row("PRAGMA integrity_check;", [], |row| row.get(0))
            .map_err(|e| format!("LKG integrity check query failed: {e}"))?;
        if integrity != "ok" {
            return Err(format!("LKG SQLite integrity check failed: {integrity}"));
        }
    }

    let lkg_rows = super::snapshot_sqlite::count_chunks(&lkg_sqlite);
    log_alert(
        &audit_dir,
        &format!("LKG validated: {} (rows={lkg_rows})", lkg.display()),
    );

    // ── Step 3: Stop gateway ──────────────────────────────────────
    log::info!("Stopping gateway for auto-recovery...");
    if let Err(e) = platform.stop_service(&cfg.gateway_service) {
        log::warn!("Failed to stop gateway (may already be stopped): {e}");
    }
    std::thread::sleep(std::time::Duration::from_secs(1));

    // ── Step 4: Emergency snapshot to postmortem ──────────────────
    let ts = timestamp_str();
    let emergency_dir = cfg
        .vault_dir
        .join("postmortem")
        .join(format!("{ts}-pre-auto-recover"));
    fs::create_dir_all(&emergency_dir).map_err(|e| format!("Cannot create emergency dir: {e}"))?;

    let src_sqlite = cfg.openclaw_dir.join("memory/main.sqlite");
    let src_memory_md = cfg.openclaw_dir.join("workspace/MEMORY.md");
    let src_memory_dir = cfg.openclaw_dir.join("workspace/memory");

    // Best-effort copy of current state
    if src_sqlite.exists() {
        let _ = fs::copy(&src_sqlite, emergency_dir.join("main.sqlite"));
    }
    if src_memory_md.exists() {
        let _ = fs::copy(&src_memory_md, emergency_dir.join("MEMORY.md"));
    }
    if src_memory_dir.is_dir() {
        let _ = copy_dir_recursive(&src_memory_dir, &emergency_dir.join("memory"));
    }

    // Fix ownership on emergency dir to outerclaw
    fix_ownership_outerclaw(&emergency_dir);
    log_alert(
        &audit_dir,
        &format!("Emergency snapshot saved: {}", emergency_dir.display()),
    );

    // ── Step 5: Restore SQLite ────────────────────────────────────
    let dst_sqlite = cfg.openclaw_dir.join("memory/main.sqlite");
    if lkg_sqlite.exists() {
        // Ensure parent directory exists
        if let Some(parent) = dst_sqlite.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::copy(&lkg_sqlite, &dst_sqlite).map_err(|e| format!("Failed to restore SQLite: {e}"))?;
        fix_ownership_user(&dst_sqlite, &cfg.agent_user)?;
        set_permissions(&dst_sqlite, 0o600);
        log_alert(&audit_dir, "SQLite restored from LKG");
    }

    // ── Step 6: Restore MEMORY.md ─────────────────────────────────
    let lkg_memory_md = lkg.join("MEMORY.md");
    let dst_memory_md = cfg.openclaw_dir.join("workspace/MEMORY.md");
    if lkg_memory_md.exists() {
        fs::copy(&lkg_memory_md, &dst_memory_md)
            .map_err(|e| format!("Failed to restore MEMORY.md: {e}"))?;
        fix_ownership_user(&dst_memory_md, &cfg.agent_user)?;
        log_alert(&audit_dir, "MEMORY.md restored from LKG");
    }

    // ── Step 7: Restore memory/ directory ─────────────────────────
    let lkg_memory_dir = lkg.join("memory");
    let dst_memory_dir = cfg.openclaw_dir.join("workspace/memory");
    if lkg_memory_dir.is_dir() {
        // Delete target first, then copy
        if dst_memory_dir.exists() {
            let _ = fs::remove_dir_all(&dst_memory_dir);
        }
        copy_dir_recursive(&lkg_memory_dir, &dst_memory_dir)
            .map_err(|e| format!("Failed to restore memory/: {e}"))?;
        fix_ownership_recursive(&dst_memory_dir, &cfg.agent_user)?;
        log_alert(&audit_dir, "memory/ restored from LKG");
    }

    // ── Step 8: Restart gateway ───────────────────────────────────
    log_alert(&audit_dir, "Starting gateway...");
    platform
        .restart_service(&cfg.gateway_service)
        .map_err(|e| format!("Failed to restart gateway: {e}"))?;

    // ── Step 9: Wait 3s, verify active ────────────────────────────
    std::thread::sleep(std::time::Duration::from_secs(3));

    let state = platform
        .service_state(&cfg.gateway_service)
        .unwrap_or(ServiceActive::Unknown);

    if state == ServiceActive::Active {
        let msg = format!(
            "Auto-recovered from LKG. Data rolled back to: {}. Corrupt state saved: {}",
            lkg.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown"),
            emergency_dir.display()
        );
        log_alert(&audit_dir, &format!("OK: Gateway recovered. {msg}"));
        send_alert("WARNING", &msg, cfg);
        Ok(())
    } else {
        let msg = format!("Gateway still not active after LKG restore. Status: {state}");
        log_alert(&audit_dir, &format!("FAIL: {msg}"));
        Err(msg)
    }
}

/// Fix file ownership to the agent user via nix::unistd::chown.
fn fix_ownership_user(path: &Path, user: &str) -> Result<(), String> {
    let usr = nix::unistd::User::from_name(user)
        .map_err(|e| format!("Cannot look up user {user}: {e}"))?
        .ok_or_else(|| format!("User {user} not found"))?;

    nix::unistd::chown(path, Some(usr.uid), Some(usr.gid))
        .map_err(|e| format!("chown {} to {user}: {e}", path.display()))?;

    Ok(())
}

/// Recursively fix ownership on a directory tree.
fn fix_ownership_recursive(path: &Path, user: &str) -> Result<(), String> {
    fix_ownership_user(path, user)?;

    if path.is_dir() {
        let entries =
            fs::read_dir(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
            let p = entry.path();
            if p.is_dir() {
                fix_ownership_recursive(&p, user)?;
            } else {
                fix_ownership_user(&p, user)?;
            }
        }
    }

    Ok(())
}

/// Fix ownership on a path to the outerclaw user (best-effort).
fn fix_ownership_outerclaw(path: &Path) {
    if let Ok(Some(usr)) = nix::unistd::User::from_name("outerclaw") {
        let _ = chown_recursive(path, usr.uid, usr.gid);
    }
}

/// Recursively chown a directory tree.
fn chown_recursive(
    path: &Path,
    uid: nix::unistd::Uid,
    gid: nix::unistd::Gid,
) -> Result<(), String> {
    let _ = nix::unistd::chown(path, Some(uid), Some(gid));

    if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    chown_recursive(&p, uid, gid)?;
                } else {
                    let _ = nix::unistd::chown(&p, Some(uid), Some(gid));
                }
            }
        }
    }

    Ok(())
}

/// Set file permissions (mode).
fn set_permissions(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(mode);
        let _ = fs::set_permissions(path, perms);
    }
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("Cannot create {}: {e}", dst.display()))?;

    let entries = fs::read_dir(src).map_err(|e| format!("Cannot read {}: {e}", src.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        let ft = entry.file_type().map_err(|e| format!("file_type: {e}"))?;

        if ft.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if ft.is_file() {
            fs::copy(&src_path, &dst_path).map_err(|e| {
                format!("copy {} -> {}: {e}", src_path.display(), dst_path.display())
            })?;
        }
    }

    Ok(())
}

/// Append a line to the alerts audit log.
fn log_alert(audit_dir: &Path, message: &str) {
    let ts = crate::alert::format_utc_now();
    let line = format!("[{ts}] AUTO-RECOVER: {message}\n");
    log::info!("AUTO-RECOVER: {message}");
    let log_path = audit_dir.join("alerts.log");
    match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(mut f) => {
            use std::io::Write;
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
