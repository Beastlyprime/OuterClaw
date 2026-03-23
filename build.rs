use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // --- Git commit hash ---
    let git_hash = read_git_hash().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=OUTERCLAW_GIT_HASH={git_hash}");

    // --- Build timestamp (UTC ISO 8601) ---
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| {
            let secs = d.as_secs();
            // Manual UTC formatting to avoid pulling in chrono just for build.rs
            let days_since_epoch = secs / 86400;
            let time_of_day = secs % 86400;
            let hours = time_of_day / 3600;
            let minutes = (time_of_day % 3600) / 60;
            let seconds = time_of_day % 60;

            // Calculate year/month/day from days since epoch (1970-01-01)
            let (year, month, day) = days_to_ymd(days_since_epoch);
            format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
        })
        .unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=OUTERCLAW_BUILD_TIMESTAMP={timestamp}");

    // --- Target triple ---
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=OUTERCLAW_BUILD_TARGET={target}");

    // Rebuild if git HEAD changes
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Some(ref_path) = resolve_git_ref_path() {
        println!("cargo:rerun-if-changed={ref_path}");
    }
}

/// Read the short git commit hash by parsing .git/HEAD and following refs.
fn read_git_hash() -> Option<String> {
    let head = fs::read_to_string(".git/HEAD").ok()?;
    let head = head.trim();

    let full_hash = if let Some(ref_name) = head.strip_prefix("ref: ") {
        // HEAD points to a branch ref
        let ref_path = format!(".git/{}", ref_name);
        // Try the loose ref first
        if let Ok(hash) = fs::read_to_string(&ref_path) {
            hash.trim().to_string()
        } else {
            // Fall back to packed-refs
            read_packed_ref(ref_name)?
        }
    } else {
        // Detached HEAD — the hash is directly in HEAD
        head.to_string()
    };

    // Return the short hash (first 7 chars)
    if full_hash.len() >= 7 {
        Some(full_hash[..7].to_string())
    } else {
        Some(full_hash)
    }
}

/// Resolve the filesystem path to the current git ref (for rerun-if-changed).
fn resolve_git_ref_path() -> Option<String> {
    let head = fs::read_to_string(".git/HEAD").ok()?;
    let head = head.trim();
    if let Some(ref_name) = head.strip_prefix("ref: ") {
        let ref_path = format!(".git/{}", ref_name);
        if Path::new(&ref_path).exists() {
            return Some(ref_path);
        }
    }
    None
}

/// Search packed-refs for a given ref name.
fn read_packed_ref(ref_name: &str) -> Option<String> {
    let packed = fs::read_to_string(".git/packed-refs").ok()?;
    for line in packed.lines() {
        if line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() == 2 && parts[1] == ref_name {
            return Some(parts[0].to_string());
        }
    }
    None
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm adapted from Howard Hinnant's civil_from_days
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
