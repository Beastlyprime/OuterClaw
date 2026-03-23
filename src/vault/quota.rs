//! Vault disk quota check with emergency pruning.
//!
//! Rust port of `scripts/quota-check.sh`. Walks the vault directory to
//! compute total disk usage, and if over the limit, performs emergency
//! pruning (reducing snapshot/postmortem/LKG retention) before re-checking.

use std::fs;
use std::io::Write;
use std::path::Path;

/// Check vault disk quota. Returns `true` if under limit (OK to proceed).
///
/// If over quota, attempts emergency pruning and re-checks. Returns `false`
/// only if still over quota after pruning.
pub fn check(vault_dir: &Path, max_mb: u64) -> bool {
    let current_mb = dir_size_mb(vault_dir);

    if current_mb <= max_mb {
        return true;
    }

    log_quota(
        vault_dir,
        &format!("WARNING: Vault at {current_mb}MB / {max_mb}MB -- attempting emergency prune"),
    );

    // ── Emergency prune: reduce retention ──────────────────────────
    // Snapshots: keep 48 SQLite files
    let snap_dir = vault_dir.join("snapshots");
    prune_sorted_entries(&snap_dir, "main-", 48, false);
    // Snapshots: keep 48 file snapshot dirs
    prune_sorted_entries(&snap_dir, "files-", 48, true);

    // Postmortems: keep 10
    let pm_dir = vault_dir.join("postmortem");
    prune_sorted_entries(&pm_dir, "", 10, true);

    // LKGs: keep 5
    let lkg_dir = vault_dir.join("lkg");
    prune_sorted_entries(&lkg_dir, "lkg-", 5, true);

    // ── Re-check ──────────────────────────────────────────────────
    let after_mb = dir_size_mb(vault_dir);
    if after_mb <= max_mb {
        log_quota(
            vault_dir,
            &format!("OK: Emergency prune freed space, now {after_mb}MB / {max_mb}MB"),
        );
        return true;
    }

    log_quota(
        vault_dir,
        &format!("FAIL: Vault still at {after_mb}MB after emergency prune (limit: {max_mb}MB)"),
    );
    false
}

/// List entries in `dir` matching `prefix`, sorted by name descending,
/// and remove all entries beyond `keep`.
///
/// If `is_dir` is true, entries are removed with `remove_dir_all`;
/// otherwise with `remove_file`.
fn prune_sorted_entries(dir: &Path, prefix: &str, keep: usize, is_dir: bool) {
    if !dir.exists() {
        return;
    }

    let mut entries: Vec<_> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name_str = name.to_string_lossy();
            if prefix.is_empty() {
                // Match all entries (for postmortem dirs)
                if is_dir {
                    e.path().is_dir()
                } else {
                    e.path().is_file()
                }
            } else {
                name_str.starts_with(prefix)
                    && if is_dir {
                        e.path().is_dir()
                    } else {
                        e.path().is_file()
                    }
            }
        })
        .collect();

    // Sort by name descending (newest first, since names contain timestamps)
    entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));

    for old in entries.into_iter().skip(keep) {
        let path = old.path();
        log::debug!("Quota prune: removing {}", path.display());
        if is_dir {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Walk a directory tree and compute total size in MiB.
fn dir_size_mb(path: &Path) -> u64 {
    let bytes = dir_size_bytes(path);
    bytes / (1024 * 1024)
}

/// Walk a directory tree and compute total size in bytes.
fn dir_size_bytes(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }

    let mut total: u64 = 0;
    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if ft.is_dir() {
            total += dir_size_bytes(&entry.path());
        } else if ft.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }

    total
}

/// Append a line to the backup audit log.
fn log_quota(vault_dir: &Path, message: &str) {
    let ts = crate::alert::format_utc_now();
    let line = format!("[{ts}] QUOTA: {message}\n");
    log::info!("QUOTA: {message}");
    let audit_dir = vault_dir.join("audit");
    let _ = fs::create_dir_all(&audit_dir);
    let log_path = audit_dir.join("backup.log");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dir_size_bytes() {
        let dir = std::env::temp_dir().join("outerclaw_quota_test_size");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), b"12345").unwrap();
        fs::write(dir.join("sub/b.txt"), b"67890").unwrap();

        assert_eq!(dir_size_bytes(&dir), 10);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dir_size_bytes_nonexistent() {
        assert_eq!(
            dir_size_bytes(Path::new("/tmp/outerclaw_does_not_exist_quota")),
            0
        );
    }

    #[test]
    fn test_check_under_limit() {
        let dir = std::env::temp_dir().join("outerclaw_quota_test_under");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tiny.txt"), b"hi").unwrap();

        // 2048 MB limit, tiny file should be well under
        assert!(check(&dir, 2048));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_prune_sorted_entries_files() {
        let dir = std::env::temp_dir().join("outerclaw_quota_prune_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Create 10 files
        for i in 0..10 {
            fs::write(dir.join(format!("main-{i:04}.sqlite")), b"data").unwrap();
        }

        prune_sorted_entries(&dir, "main-", 3, false);

        let remaining: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(remaining.len(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_prune_sorted_entries_dirs() {
        let dir = std::env::temp_dir().join("outerclaw_quota_prune_dirs_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        for i in 0..10 {
            fs::create_dir_all(dir.join(format!("lkg-{i:04}"))).unwrap();
        }

        prune_sorted_entries(&dir, "lkg-", 5, true);

        let remaining: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        assert_eq!(remaining.len(), 5);

        let _ = fs::remove_dir_all(&dir);
    }
}
