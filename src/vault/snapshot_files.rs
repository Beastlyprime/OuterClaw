//! File snapshot — Rust port of `scripts/snapshot-files.sh`.
//!
//! Backs up MEMORY.md, `memory/` directory, config files, and a git bundle
//! of the workspace. Generates a SHA-256 manifest and prunes old snapshots.

use crate::config::Config;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Maximum number of file snapshot directories to retain.
const MAX_SNAPSHOTS: usize = 96;

/// Run the file snapshot pipeline. Returns 0 on success, 1 on failure.
pub fn run_files(cfg: &Config) -> i32 {
    match run_inner(cfg) {
        Ok(()) => 0,
        Err(e) => {
            log::error!("File snapshot failed: {e}");
            crate::alert::send_alert("CRITICAL", &format!("File snapshot failed: {e}"), cfg);
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

    // ── Preflight: I/O pressure (skipped — no platform handle in Phase 2) ──

    let ts = timestamp_local();
    let dst_dir = cfg.vault_dir.join("snapshots");
    let snap = dst_dir.join(format!("files-{ts}"));

    ensure_dir(&snap.join("memory"))?;
    ensure_dir(&snap.join("config"))?;

    let workspace = cfg.openclaw_dir.join("workspace");
    let mut file_count: u64 = 0;

    // ── Copy MEMORY.md ─────────────────────────────────────────────
    let memory_md = workspace.join("MEMORY.md");
    if memory_md.exists() {
        wait_stable(&memory_md);
        match fs::copy(&memory_md, snap.join("MEMORY.md")) {
            Ok(_) => file_count += 1,
            Err(e) => log_backup(
                &cfg.vault_dir,
                &format!("WARNING: MEMORY.md copy failed: {e}"),
            ),
        }
    }

    // ── Copy memory/ directory ─────────────────────────────────────
    let memory_dir = workspace.join("memory");
    if memory_dir.is_dir() {
        match copy_dir_recursive(&memory_dir, &snap.join("memory")) {
            Ok(n) => file_count += n,
            Err(e) => log_backup(
                &cfg.vault_dir,
                &format!("WARNING: memory/ copy failed: {e}"),
            ),
        }
    }

    // ── Copy config files ──────────────────────────────────────────
    let openclaw_json = cfg.openclaw_dir.join("openclaw.json");
    if openclaw_json.exists() {
        match fs::copy(&openclaw_json, snap.join("config/openclaw.json")) {
            Ok(_) => file_count += 1,
            Err(e) => log_backup(
                &cfg.vault_dir,
                &format!("WARNING: openclaw.json copy failed: {e}"),
            ),
        }
    }

    let approvals = cfg.openclaw_dir.join("exec-approvals.json");
    if approvals.exists() {
        match fs::copy(&approvals, snap.join("config/exec-approvals.json")) {
            Ok(_) => file_count += 1,
            Err(e) => log_backup(
                &cfg.vault_dir,
                &format!("WARNING: exec-approvals.json copy failed: {e}"),
            ),
        }
    }

    // ── Git bundle ─────────────────────────────────────────────────
    let git_dir = workspace.join(".git");
    if git_dir.exists() {
        let bundle_path = snap.join("workspace.bundle");
        match Command::new("git")
            .arg("-C")
            .arg(&workspace)
            .arg("bundle")
            .arg("create")
            .arg(&bundle_path)
            .arg("--all")
            .output()
        {
            Ok(output) if output.status.success() => {
                file_count += 1;
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                log_backup(
                    &cfg.vault_dir,
                    &format!("WARNING: git bundle failed: {stderr}"),
                );
            }
            Err(e) => {
                log_backup(
                    &cfg.vault_dir,
                    &format!("WARNING: git not found or bundle failed: {e}"),
                );
            }
        }
    }

    // ── Generate MANIFEST.sha256 ───────────────────────────────────
    generate_manifest(&snap)?;

    // ── Log success ────────────────────────────────────────────────
    let actual_count = count_files_recursive(&snap);
    let size = dir_size(&snap);
    let size_human = format_size(size);
    log_backup(
        &cfg.vault_dir,
        &format!(
            "OK: {} ({actual_count} files, {size_human})",
            snap.display()
        ),
    );

    // ── Prune ──────────────────────────────────────────────────────
    prune_file_snapshots(&dst_dir);

    Ok(())
}

/// Wait for a file's mtime to stabilize (at least 2 seconds old).
///
/// Prevents copying half-written files during active OpenClaw writes.
/// Waits up to 10 seconds before giving up.
fn wait_stable(path: &Path) {
    for _ in 0..10 {
        if let Ok(meta) = fs::metadata(path) {
            if let Ok(modified) = meta.modified() {
                let age = modified.elapsed().unwrap_or_default();
                if age.as_secs() >= 2 {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    log::warn!(
        "{} still being written after 10s, copying anyway",
        path.display()
    );
}

/// Recursively copy a directory tree. Returns number of files copied.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<u64, String> {
    if !src.is_dir() {
        return Err(format!("{} is not a directory", src.display()));
    }
    ensure_dir(dst)?;

    let mut count: u64 = 0;
    let entries =
        fs::read_dir(src).map_err(|e| format!("Cannot read directory {}: {e}", src.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Directory entry error: {e}"))?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);

        let ft = entry
            .file_type()
            .map_err(|e| format!("Cannot read file type of {}: {e}", src_path.display()))?;

        if ft.is_dir() {
            count += copy_dir_recursive(&src_path, &dst_path)?;
        } else if ft.is_file() {
            fs::copy(&src_path, &dst_path).map_err(|e| {
                format!("Copy {} -> {}: {e}", src_path.display(), dst_path.display())
            })?;
            count += 1;
        }
        // Skip symlinks and other special types for safety
    }
    Ok(count)
}

/// Walk the snapshot directory and generate MANIFEST.sha256.
///
/// Each line: `<sha256_hex>  <relative_path>`
fn generate_manifest(snap_dir: &Path) -> Result<(), String> {
    let manifest_path = snap_dir.join("MANIFEST.sha256");
    let mut lines = Vec::new();

    walk_files(snap_dir, snap_dir, &mut |rel_path, abs_path| {
        // Skip the manifest itself
        if abs_path == manifest_path {
            return Ok(());
        }
        let data = fs::read(abs_path)
            .map_err(|e| format!("Cannot read {} for manifest: {e}", abs_path.display()))?;
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let hash = format!("{:x}", hasher.finalize());
        lines.push(format!("{hash}  {rel_path}"));
        Ok(())
    })?;

    lines.sort();
    let content = lines.join("\n") + "\n";
    fs::write(&manifest_path, content.as_bytes())
        .map_err(|e| format!("Cannot write manifest: {e}"))?;
    Ok(())
}

/// Recursively walk files under `base`, calling `f` with (relative_path_str, abs_path).
fn walk_files(
    dir: &Path,
    base: &Path,
    f: &mut dyn FnMut(&str, &Path) -> Result<(), String>,
) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("Cannot read directory {}: {e}", dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Directory entry error: {e}"))?;
        let path = entry.path();
        let ft = entry
            .file_type()
            .map_err(|e| format!("Cannot read file type: {e}"))?;

        if ft.is_dir() {
            walk_files(&path, base, f)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(base)
                .map_err(|e| format!("strip_prefix failed: {e}"))?
                .to_string_lossy()
                .to_string();
            f(&rel, &path)?;
        }
    }
    Ok(())
}

/// Count all regular files under a directory, recursively.
fn count_files_recursive(dir: &Path) -> u64 {
    let mut count: u64 = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                count += count_files_recursive(&path);
            } else if path.is_file() {
                count += 1;
            }
        }
    }
    count
}

/// Compute total size of all files under a directory, recursively.
fn dir_size(dir: &Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_size(&path);
            } else if path.is_file() {
                total += fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// Keep only the newest `MAX_SNAPSHOTS` file snapshot directories, remove the rest.
fn prune_file_snapshots(dst_dir: &Path) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dst_dir)
        .into_iter()
        .flatten()
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
    // Sort descending by name (newest first)
    entries.sort();
    entries.reverse();
    for old in entries.into_iter().skip(MAX_SNAPSHOTS) {
        log::debug!("Pruning old file snapshot: {}", old.display());
        let _ = fs::remove_dir_all(&old);
    }
}

/// Generate a local-time timestamp in `YYYYMMDD-HHMMSS` format using libc.
fn timestamp_local() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_copy_dir_recursive() {
        let base = std::env::temp_dir().join("outerclaw_copydir_test");
        let _ = fs::remove_dir_all(&base);
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"aaa").unwrap();
        fs::write(src.join("sub/b.txt"), b"bbb").unwrap();

        let count = copy_dir_recursive(&src, &dst).unwrap();
        assert_eq!(count, 2);
        assert!(dst.join("a.txt").exists());
        assert!(dst.join("sub/b.txt").exists());
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "aaa");
        assert_eq!(fs::read_to_string(dst.join("sub/b.txt")).unwrap(), "bbb");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_generate_manifest() {
        let base = std::env::temp_dir().join("outerclaw_manifest_test");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("file1.txt"), b"hello").unwrap();
        fs::write(base.join("file2.txt"), b"world").unwrap();

        generate_manifest(&base).unwrap();

        let manifest = fs::read_to_string(base.join("MANIFEST.sha256")).unwrap();
        assert!(manifest.contains("file1.txt"));
        assert!(manifest.contains("file2.txt"));
        // Should not contain MANIFEST.sha256 itself
        assert!(!manifest.contains("MANIFEST.sha256"));
        // Each line should have hash + two-space separator + filename
        for line in manifest.trim().lines() {
            let parts: Vec<&str> = line.splitn(2, "  ").collect();
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[0].len(), 64); // SHA-256 hex = 64 chars
        }

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_count_files_and_dir_size() {
        let base = std::env::temp_dir().join("outerclaw_count_test");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("sub")).unwrap();
        fs::write(base.join("a.txt"), b"12345").unwrap();
        fs::write(base.join("sub/b.txt"), b"67890abc").unwrap();

        assert_eq!(count_files_recursive(&base), 2);
        assert_eq!(dir_size(&base), 13); // 5 + 8

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_prune_file_snapshots() {
        let dir = std::env::temp_dir().join("outerclaw_prune_files_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Create 100 fake snapshot dirs
        for i in 0..100 {
            let name = format!("files-20260101-{i:06}");
            fs::create_dir_all(dir.join(&name)).unwrap();
        }

        prune_file_snapshots(&dir);

        let remaining: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with("files-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(remaining.len(), MAX_SNAPSHOTS);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_timestamp_local_format() {
        let ts = timestamp_local();
        assert_eq!(ts.len(), 15);
        assert_eq!(&ts[8..9], "-");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(1024), "1.0K");
        assert_eq!(format_size(2_097_152), "2.0M");
    }
}
