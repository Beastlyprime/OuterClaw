//! Human-triggered rollback to a specific LKG state.
//!
//! Rust port of `scripts/rollback.sh`. This is a DESTRUCTIVE operation that
//! requires interactive confirmation. Shows a rollback plan, confirms with
//! the user, stops the gateway, takes an emergency snapshot, restores data
//! from the chosen LKG, and restarts the gateway.

use crate::alert::send_alert;
use crate::cli::RollbackArgs;
use crate::config::Config;
use crate::platform::{Platform, ServiceActive};
use std::fs;
use std::path::{Path, PathBuf};

/// Run interactive rollback. Returns 0 on success, 1 on failure.
pub fn run(args: RollbackArgs, cfg: Config, platform: Box<dyn Platform>) -> i32 {
    match run_inner(&args, &cfg, platform.as_ref()) {
        Ok(()) => 0,
        Err(e) => {
            log::error!("Rollback failed: {e}");
            1
        }
    }
}

fn run_inner(args: &RollbackArgs, cfg: &Config, platform: &dyn Platform) -> Result<(), String> {
    let lkg_dir = cfg.vault_dir.join("lkg");
    let audit_dir = cfg.vault_dir.join("audit");
    fs::create_dir_all(&audit_dir).map_err(|e| format!("Cannot create audit dir: {e}"))?;

    // ── Step 1: Resolve LKG path ──────────────────────────────────
    let lkg = resolve_lkg(args.path.as_deref(), &lkg_dir)?;

    if !lkg.is_dir() {
        eprintln!("ERROR: LKG not found at {}", lkg.display());
        list_available_lkgs(&lkg_dir);
        return Err(format!("LKG not found: {}", lkg.display()));
    }

    // ── Step 2: Show rollback plan ────────────────────────────────
    println!();
    println!("================================================================");
    println!("                     ROLLBACK PLAN");
    println!("================================================================");
    println!();
    println!("Source LKG: {}", lkg.display());
    println!();
    println!("Will restore:");

    // Every SQLite source the LKG might contain.
    let mut lkg_dbs: Vec<(&'static str, PathBuf)> = Vec::new();
    for source in super::snapshot_sqlite::SQLITE_SOURCES {
        let p = lkg.join(format!("{}.sqlite", source.label));
        if p.exists() {
            let size = fs::metadata(&p)
                .map(|m| format_size(m.len()))
                .unwrap_or_else(|_| "?".into());
            println!("  - SQLite[{}]:    {size}", source.label);
            lkg_dbs.push((source.label, p));
        }
    }

    let lkg_memory_md = lkg.join("MEMORY.md");
    if lkg_memory_md.exists() {
        let size = fs::metadata(&lkg_memory_md)
            .map(|m| format_size(m.len()))
            .unwrap_or_else(|_| "?".into());
        println!("  - MEMORY.md:        {size}");
    }

    let lkg_memory_dir = lkg.join("memory");
    if lkg_memory_dir.is_dir() {
        let count = count_files(&lkg_memory_dir);
        println!("  - memory/ dir:      {count} files");
    }

    let lkg_config_dir = lkg.join("config");
    let has_config = lkg_config_dir.join("openclaw.json").exists();
    if has_config {
        let count = count_files(&lkg_config_dir);
        println!("  - config/:          {count} files");
    }

    println!();
    println!("Will OVERWRITE:");
    for source in super::snapshot_sqlite::SQLITE_SOURCES {
        if lkg_dbs.iter().any(|(l, _)| *l == source.label) {
            println!("  -> {}/{}", cfg.openclaw_dir.display(), source.rel_path);
        }
    }
    println!("  -> {}/workspace/MEMORY.md", cfg.openclaw_dir.display());
    println!("  -> {}/workspace/memory/", cfg.openclaw_dir.display());
    println!();
    println!("WARNING: This will STOP the OpenClaw gateway during rollback.");
    println!();

    // ── Step 3: Interactive confirmation ───────────────────────────
    let confirmed = dialoguer::Confirm::new()
        .with_prompt("Type 'y' to confirm rollback")
        .default(false)
        .interact()
        .map_err(|e| format!("Confirmation failed: {e}"))?;

    if !confirmed {
        println!("Aborted.");
        return Ok(());
    }

    // ── Step 4: Stop gateway ──────────────────────────────────────
    println!();
    println!("Stopping gateway...");
    if let Err(e) = platform.stop_service(&cfg.gateway_service) {
        log::warn!("Failed to stop gateway (may already be stopped): {e}");
    }
    std::thread::sleep(std::time::Duration::from_secs(2));

    // ── Step 5: Emergency snapshot to postmortem ──────────────────
    let ts = timestamp_str();
    let emergency_dir = cfg
        .vault_dir
        .join("postmortem")
        .join(format!("{ts}-pre-rollback"));
    fs::create_dir_all(&emergency_dir).map_err(|e| format!("Cannot create emergency dir: {e}"))?;

    let src_memory_md = cfg.openclaw_dir.join("workspace/MEMORY.md");
    let src_memory_dir = cfg.openclaw_dir.join("workspace/memory");

    for source in super::snapshot_sqlite::SQLITE_SOURCES {
        let src = cfg.openclaw_dir.join(source.rel_path);
        if src.exists() {
            let _ = fs::copy(&src, emergency_dir.join(format!("{}.sqlite", source.label)));
        }
    }
    if src_memory_md.exists() {
        let _ = fs::copy(&src_memory_md, emergency_dir.join("MEMORY.md"));
    }
    if src_memory_dir.is_dir() {
        let _ = copy_dir_recursive(&src_memory_dir, &emergency_dir.join("memory"));
    }

    fix_ownership_outerclaw(&emergency_dir);
    println!("Pre-rollback snapshot saved: {}", emergency_dir.display());

    // ── Step 6: Restore every SQLite source present in LKG ────────
    for (label, lkg_db) in &lkg_dbs {
        let source = super::snapshot_sqlite::SQLITE_SOURCES
            .iter()
            .find(|s| s.label == *label)
            .ok_or_else(|| format!("Unknown sqlite label in LKG: {label}"))?;
        let dst = cfg.openclaw_dir.join(source.rel_path);
        if let Some(parent) = dst.parent() {
            let _ = fs::create_dir_all(parent);
            let _ = fix_ownership_user(parent, &cfg.agent_user);
        }
        fs::copy(lkg_db, &dst).map_err(|e| format!("Failed to restore {label}: {e}"))?;
        fix_ownership_user(&dst, &cfg.agent_user)?;
        set_permissions(&dst, 0o600);
        println!("  - {label} restored");
    }

    // ── Step 6b: Restore MEMORY.md ────────────────────────────────
    if lkg_memory_md.exists() {
        let dst = cfg.openclaw_dir.join("workspace/MEMORY.md");
        fs::copy(&lkg_memory_md, &dst).map_err(|e| format!("Failed to restore MEMORY.md: {e}"))?;
        fix_ownership_user(&dst, &cfg.agent_user)?;
        println!("  - MEMORY.md restored");
    }

    // ── Step 6c: Restore memory/ ──────────────────────────────────
    if lkg_memory_dir.is_dir() {
        let dst = cfg.openclaw_dir.join("workspace/memory");
        if dst.exists() {
            let _ = fs::remove_dir_all(&dst);
        }
        copy_dir_recursive(&lkg_memory_dir, &dst)
            .map_err(|e| format!("Failed to restore memory/: {e}"))?;
        fix_ownership_recursive(&dst, &cfg.agent_user)?;
        println!("  - memory/ restored");
    }

    // ── Step 7: Optionally restore config ─────────────────────────
    if has_config {
        let restore_config = dialoguer::Confirm::new()
            .with_prompt("Also restore openclaw.json?")
            .default(false)
            .interact()
            .unwrap_or(false);

        if restore_config {
            let src_json = lkg_config_dir.join("openclaw.json");
            let dst_json = cfg.openclaw_dir.join("openclaw.json");
            fs::copy(&src_json, &dst_json)
                .map_err(|e| format!("Failed to restore openclaw.json: {e}"))?;
            fix_ownership_user(&dst_json, &cfg.agent_user)?;
            set_permissions(&dst_json, 0o600);
            println!("  - openclaw.json restored");
        }
    }

    // ── Step 8: Restart gateway ───────────────────────────────────
    println!();
    println!("Starting gateway...");
    platform
        .restart_service(&cfg.gateway_service)
        .map_err(|e| format!("Failed to start gateway: {e}"))?;

    std::thread::sleep(std::time::Duration::from_secs(3));

    let state = platform
        .service_state(&cfg.gateway_service)
        .unwrap_or(ServiceActive::Unknown);

    // ── Step 9: Report results ────────────────────────────────────
    println!();
    println!("================================================================");
    println!("                   ROLLBACK COMPLETE");
    println!("================================================================");
    println!();
    println!("  Restored from: {}", lkg.display());
    println!("  Gateway status: {state}");
    println!("  Pre-rollback backup: {}", emergency_dir.display());
    println!();

    // ── Log and alert ─────────────────────────────────────────────
    log_lkg(
        &audit_dir,
        &format!("ROLLBACK from {} by user", lkg.display()),
    );
    send_alert(
        "WARNING",
        &format!(
            "ROLLBACK executed from {}. Gateway status: {state}",
            lkg.display()
        ),
        cfg,
    );

    Ok(())
}

/// Resolve the LKG path from args or the `lkg/current` symlink.
fn resolve_lkg(path: Option<&Path>, lkg_dir: &Path) -> Result<PathBuf, String> {
    let raw = match path {
        Some(p) => p.to_path_buf(),
        None => lkg_dir.join("current"),
    };

    // Follow symlinks
    match fs::canonicalize(&raw) {
        Ok(resolved) => Ok(resolved),
        Err(_) => {
            // If it doesn't resolve, try the raw path
            if raw.exists() {
                Ok(raw)
            } else {
                eprintln!("ERROR: LKG not found at {}", raw.display());
                list_available_lkgs(lkg_dir);
                Err(format!("LKG not found: {}", raw.display()))
            }
        }
    }
}

/// List available LKG directories for the user.
fn list_available_lkgs(lkg_dir: &Path) {
    eprintln!("Available LKGs:");
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

    entries.sort();
    entries.reverse();

    if entries.is_empty() {
        eprintln!("  (none)");
    } else {
        for e in &entries {
            eprintln!("  {}", e.display());
        }
    }
}

/// Count files in a directory (non-recursive).
fn count_files(dir: &Path) -> usize {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .count()
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

/// Append a line to the LKG audit log.
fn log_lkg(audit_dir: &Path, message: &str) {
    let ts = crate::alert::format_utc_now();
    let line = format!("[{ts}] {message}\n");
    log::info!("{message}");
    let log_path = audit_dir.join("lkg.log");
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
