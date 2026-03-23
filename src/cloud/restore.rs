//! Cloud backup restore — download and verify backups from encrypted cloud.
//!
//! Rust port of `scripts/cloud-restore.sh`. Supports listing available
//! backups, showing the recovery hint, and downloading specific LKG states
//! or snapshots with post-download integrity verification.

use crate::cli::CloudRestoreArgs;
use crate::config::Config;
use crate::platform::Platform;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

const RCLONE_CONFIG_PATH: &str = "/var/lib/outerclaw/config/rclone.conf";

/// Run the cloud restore operation based on CLI flags.
pub fn run(args: CloudRestoreArgs, cfg: Config, _platform: Box<dyn Platform>) -> i32 {
    // ── Root check ──────────────────────────────────────────────
    if !nix::unistd::geteuid().is_root() {
        eprintln!("ERROR: Must run as root (sudo outerclaw cloud restore ...)");
        return 1;
    }

    // ── Preflight: rclone installed ─────────────────────────────
    if Command::new("which")
        .arg("rclone")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("ERROR: rclone not found -- install it first");
        return 1;
    }

    // ── Preflight: rclone config exists ─────────────────────────
    let rclone_config = Path::new(RCLONE_CONFIG_PATH);
    if !rclone_config.exists() {
        eprintln!("ERROR: rclone config not found -- run 'outerclaw cloud setup' first");
        return 1;
    }

    let cloud_remote = &cfg.cloud_remote;

    // Dispatch based on flags
    if args.show_hint {
        return show_hint(rclone_config, cloud_remote);
    }

    if args.list {
        return list_cloud(rclone_config, cloud_remote);
    }

    if let Some(ref name) = args.restore_lkg {
        return restore_lkg(name, rclone_config, cloud_remote, &cfg.vault_dir);
    }

    if let Some(ref name) = args.restore_snapshot {
        return restore_snapshot(name, rclone_config, cloud_remote, &cfg.vault_dir);
    }

    // No flags specified: print usage
    print_usage();
    0
}

/// List available cloud backups (LKG states and snapshots).
fn list_cloud(rclone_config: &Path, cloud_remote: &str) -> i32 {
    let config_str = rclone_config.to_string_lossy();

    println!();
    println!("=== Cloud LKG States ===");

    let lkg_output = Command::new("rclone")
        .args([
            "lsd",
            &format!("{cloud_remote}:lkg/"),
            "--config",
            &config_str,
        ])
        .output();

    match lkg_output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if stdout.trim().is_empty() {
                println!("  (none)");
            } else {
                // lsd output format: "  -1 2026-03-20 10:00:00  -1 dirname"
                // Extract just the directory names
                for line in stdout.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if let Some(name) = parts.last() {
                        println!("  {name}");
                    }
                }
            }
        }
        _ => {
            println!("  (none or not accessible)");
        }
    }

    println!();
    println!("=== Cloud Snapshots ===");

    let snap_output = Command::new("rclone")
        .args([
            "ls",
            &format!("{cloud_remote}:snapshots/"),
            "--config",
            &config_str,
        ])
        .output();

    match snap_output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if stdout.trim().is_empty() {
                println!("  (none)");
            } else {
                // ls output format: "  size filename"
                for line in stdout.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let parts: Vec<&str> = trimmed.splitn(2, char::is_whitespace).collect();
                    if parts.len() == 2 {
                        let size: u64 = parts[0].trim().parse().unwrap_or(0);
                        let name = parts[1].trim();
                        let formatted = format_size(size);
                        println!("  {formatted:>10}  {name}");
                    } else {
                        println!("  {trimmed}");
                    }
                }
            }
        }
        _ => {
            println!("  (none or not accessible)");
        }
    }

    println!();
    0
}

/// Show the recovery hint from the base (unencrypted) remote.
fn show_hint(rclone_config: &Path, cloud_remote: &str) -> i32 {
    let config_str = rclone_config.to_string_lossy();

    // Resolve base remote path from rclone config (crypt remote's "remote = ..." line)
    let base_remote = match resolve_base_remote(rclone_config, cloud_remote) {
        Some(r) => r,
        None => {
            eprintln!("ERROR: Could not determine base remote from rclone config");
            return 1;
        }
    };

    println!();
    println!("Reading recovery hint from cloud (base remote, unencrypted)...");
    println!();

    let output = Command::new("rclone")
        .args([
            "cat",
            &format!("{base_remote}/RECOVERY-HINT.txt"),
            "--config",
            &config_str,
        ])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let content = String::from_utf8_lossy(&o.stdout);
            println!("{content}");
        }
        _ => {
            println!("No recovery hint found.");
            println!("Hint is set during 'outerclaw cloud setup' (optional step).");
        }
    }

    println!();
    0
}

/// Restore an LKG state from cloud.
fn restore_lkg(name: &str, rclone_config: &Path, cloud_remote: &str, vault_dir: &Path) -> i32 {
    let config_str = rclone_config.to_string_lossy();
    let dest = vault_dir.join("lkg").join(name);

    println!();
    println!("Downloading LKG '{name}' from cloud...");
    println!("  Source: {cloud_remote}:lkg/{name}/");
    println!("  Dest:   {}/", dest.display());
    println!();

    if let Err(e) = fs::create_dir_all(&dest) {
        eprintln!("ERROR: Cannot create destination directory: {e}");
        return 1;
    }

    let status = Command::new("rclone")
        .args([
            "copy",
            &format!("{cloud_remote}:lkg/{name}/"),
            &dest.to_string_lossy(),
            "--config",
            &config_str,
            "--progress",
        ])
        .status();

    match status {
        Ok(s) if s.success() => {
            // Fix ownership
            set_outerclaw_ownership(&dest);

            log_restore(vault_dir, &format!("RESTORED: LKG '{name}' from cloud"));

            println!();
            println!("[OK] LKG restored to {}", dest.display());

            // Check SQLite integrity for any .sqlite files in the LKG
            check_sqlite_files_in_dir(&dest);

            println!();
            println!("To apply this LKG state, run:");
            println!("  sudo outerclaw rollback");
        }
        _ => {
            eprintln!("ERROR: Failed to download LKG -- check logs and credentials");
            log_restore(
                vault_dir,
                &format!("ERROR: Failed to restore LKG '{name}' from cloud"),
            );
            return 1;
        }
    }

    0
}

/// Restore a snapshot file from cloud.
fn restore_snapshot(name: &str, rclone_config: &Path, cloud_remote: &str, vault_dir: &Path) -> i32 {
    let config_str = rclone_config.to_string_lossy();
    let dest = vault_dir.join("snapshots").join(name);

    // Ensure snapshots directory exists
    if let Some(parent) = dest.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("ERROR: Cannot create snapshots directory: {e}");
            return 1;
        }
    }

    println!();
    println!("Downloading snapshot '{name}' from cloud...");
    println!("  Source: {cloud_remote}:snapshots/{name}");
    println!("  Dest:   {}", dest.display());
    println!();

    let status = Command::new("rclone")
        .args([
            "copyto",
            &format!("{cloud_remote}:snapshots/{name}"),
            &dest.to_string_lossy(),
            "--config",
            &config_str,
            "--progress",
        ])
        .status();

    match status {
        Ok(s) if s.success() => {
            // Fix ownership and permissions
            set_outerclaw_ownership_file(&dest);

            log_restore(
                vault_dir,
                &format!("RESTORED: snapshot '{name}' from cloud"),
            );

            println!();
            println!("[OK] Snapshot restored to {}", dest.display());

            // Integrity check for SQLite files
            if name.ends_with(".sqlite") {
                check_sqlite_integrity(&dest);
            }
        }
        _ => {
            eprintln!("ERROR: Failed to download snapshot -- check logs and credentials");
            log_restore(
                vault_dir,
                &format!("ERROR: Failed to restore snapshot '{name}' from cloud"),
            );
            return 1;
        }
    }

    0
}

/// Resolve the base remote path from the crypt remote's config.
/// Reads the rclone config file and extracts the `remote = ...` line from
/// the crypt remote section.
fn resolve_base_remote(config_path: &Path, crypt_remote: &str) -> Option<String> {
    let content = fs::read_to_string(config_path).ok()?;
    let section_header = format!("[{crypt_remote}]");

    let mut in_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == section_header {
            in_section = true;
            continue;
        }
        if in_section {
            // New section starts
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                break;
            }
            if let Some(val) = trimmed.strip_prefix("remote") {
                let val = val.trim_start_matches([' ', '=']);
                let val = val.trim();
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }

    None
}

/// Check SQLite integrity using rusqlite.
fn check_sqlite_integrity(path: &Path) {
    println!();
    println!("Running integrity check...");

    match rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => {
            match conn.query_row("PRAGMA integrity_check;", [], |row| row.get::<_, String>(0)) {
                Ok(ref result) if result == "ok" => {
                    println!("[OK] SQLite integrity check passed");
                }
                Ok(result) => {
                    eprintln!("[WARN] SQLite integrity check: {result}");
                }
                Err(e) => {
                    eprintln!("[WARN] SQLite integrity check failed: {e}");
                }
            }
        }
        Err(e) => {
            eprintln!("[WARN] Cannot open SQLite file for integrity check: {e}");
        }
    }
}

/// Check all .sqlite files in a directory for integrity.
fn check_sqlite_files_in_dir(dir: &Path) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("sqlite") {
            println!();
            println!(
                "Checking {}...",
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            );
            check_sqlite_integrity(&path);
        }
    }
}

/// Set ownership to outerclaw:outerclaw on a directory (recursively via chown -R).
fn set_outerclaw_ownership(path: &Path) {
    let _ = Command::new("chown")
        .args(["-R", "outerclaw:outerclaw", &path.to_string_lossy()])
        .status();
    let _ = Command::new("chmod")
        .args(["-R", "700", &path.to_string_lossy()])
        .status();
}

/// Set ownership to outerclaw:outerclaw on a single file.
fn set_outerclaw_ownership_file(path: &Path) {
    let _ = Command::new("chown")
        .args(["outerclaw:outerclaw", &path.to_string_lossy()])
        .status();
    let _ = Command::new("chmod")
        .args(["600", &path.to_string_lossy()])
        .status();
}

/// Format a byte size for human-readable display.
fn format_size(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.1} KB", bytes as f64 / 1_024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Append a timestamped line to the cloud sync audit log.
fn log_restore(vault_dir: &Path, message: &str) {
    let ts = crate::alert::format_utc_now();
    let line = format!("[{ts}] {message}\n");
    log::info!("cloud-restore: {message}");

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

/// Print usage information.
fn print_usage() {
    println!();
    println!("Usage: sudo outerclaw cloud restore [OPTION]");
    println!();
    println!("Options:");
    println!("  --list                         List all cloud backups");
    println!("  --show-hint                    Show password recovery hint from cloud");
    println!("  --restore-lkg <name>           Download an LKG state from cloud");
    println!("  --restore-snapshot <name>      Download a snapshot file from cloud");
    println!();
    println!("Examples:");
    println!("  sudo outerclaw cloud restore --list");
    println!("  sudo outerclaw cloud restore --restore-lkg lkg-2026-03-20T10:00:00");
    println!("  sudo outerclaw cloud restore --restore-snapshot main-2026-03-20T12:00:00.sqlite");
    println!();
    println!("After restoring, use rollback to apply the data:");
    println!("  sudo outerclaw rollback");
    println!();
}
