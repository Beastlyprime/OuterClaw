//! macOS + launchd platform implementation for OuterClaw.
//!
//! This module provides the concrete [`MacOSLaunchd`] struct that implements
//! the [`Platform`](super::Platform) trait using:
//!
//! - `ps` and `/bin/ps` for process metrics
//! - `launchctl` for service lifecycle management (launchd)
//! - `chflags` for immutable-bit manipulation (UF_IMMUTABLE)
//! - `dscl` for user management (Directory Services)
//!
//! macOS has no PSI (I/O pressure) or sd_notify — those methods are no-ops.
//!
//! This module is gated by `#[cfg(target_os = "macos")]` in the parent `mod.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Platform, ProcessMetrics, ServiceActive};

/// macOS platform implementation backed by launchd.
pub struct MacOSLaunchd;

impl MacOSLaunchd {
    /// Create a new instance.
    pub fn new() -> Self {
        Self
    }

    // ── Internal helpers ──────────────────────────────────────────

    /// Run `launchctl list <label>` and return stdout.
    fn launchctl_list(&self, label: &str) -> Result<String, String> {
        let output = Command::new("launchctl")
            .args(["list", label])
            .output()
            .map_err(|e| format!("launchctl list {label}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "launchctl list {label} failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Run `launchctl print system/<label>` and return stdout.
    fn launchctl_print(&self, label: &str) -> Result<String, String> {
        let target = format!("system/{label}");
        let output = Command::new("launchctl")
            .args(["print", &target])
            .output()
            .map_err(|e| format!("launchctl print {target}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "launchctl print {target} failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Derive the launchd label from a service name.
    ///
    /// Converts systemd-style names like "oc-outerclaw.service" to launchd
    /// labels like "com.outerclaw.oc-outerclaw".
    fn service_to_label(service: &str) -> String {
        let base = service
            .strip_suffix(".service")
            .or_else(|| service.strip_suffix(".plist"))
            .unwrap_or(service);
        format!("com.outerclaw.{base}")
    }

    /// Path to the plist for a given label.
    fn plist_path(label: &str) -> PathBuf {
        PathBuf::from(format!("/Library/LaunchDaemons/{label}.plist"))
    }

    /// Walk a directory recursively, summing file sizes in bytes.
    fn dir_size_bytes(path: &Path) -> Result<u64, String> {
        let mut total: u64 = 0;
        if !path.exists() {
            return Ok(0);
        }
        let entries =
            fs::read_dir(path).map_err(|e| format!("read_dir {}: {e}", path.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
            let ft = entry
                .file_type()
                .map_err(|e| format!("file_type {}: {e}", entry.path().display()))?;
            if ft.is_dir() {
                total += Self::dir_size_bytes(&entry.path())?;
            } else if ft.is_file() {
                let meta = entry
                    .metadata()
                    .map_err(|e| format!("metadata {}: {e}", entry.path().display()))?;
                total += meta.len();
            }
        }
        Ok(total)
    }
}

impl Platform for MacOSLaunchd {
    // ── Process / Service discovery ────────────────────────────────

    fn find_service_pid(&self, service: &str) -> Result<Option<u32>, String> {
        let label = Self::service_to_label(service);

        // `launchctl list <label>` outputs a table with PID, status, label.
        // The first column of the first data line is the PID (or "-" if not running).
        let output = self.launchctl_list(&label)?;

        // Parse the output — look for a line containing the label.
        // Format from `launchctl list <label>`:
        //   {
        //       "PID" = 12345;
        //       ...
        //   }
        // Or in table form:  PID  Status  Label
        for line in output.lines() {
            let trimmed = line.trim();

            // Key-value format: "PID" = 12345;
            if trimmed.starts_with("\"PID\"") || trimmed.starts_with("PID") {
                if let Some(val) = trimmed.split('=').nth(1) {
                    let cleaned = val.trim().trim_end_matches(';').trim();
                    if let Ok(pid) = cleaned.parse::<u32>() {
                        if pid > 0 {
                            return Ok(Some(pid));
                        }
                    }
                }
            }
        }

        // Fallback: try parsing the table format from `launchctl list`
        // (when called without arguments, but some versions use it for single label too)
        for line in output.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 && parts[2] == label {
                if let Ok(pid) = parts[0].parse::<u32>() {
                    if pid > 0 {
                        return Ok(Some(pid));
                    }
                }
            }
        }

        Ok(None)
    }

    fn collect_proc_metrics(&self, pid: u32) -> Result<Option<ProcessMetrics>, String> {
        // Use `ps` to get process info: pid, state, rss, vsz, threads
        let output = Command::new("/bin/ps")
            .args(["-o", "pid,state,rss,vsz,wq", "-p", &pid.to_string()])
            .output()
            .map_err(|e| format!("ps -p {pid}: {e}"))?;

        if !output.status.success() {
            // Process likely doesn't exist
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();

        // First line is header, second line is data
        if lines.len() < 2 {
            return Ok(None);
        }

        let mut m = ProcessMetrics {
            pid,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            ..Default::default()
        };

        let parts: Vec<&str> = lines[1].split_whitespace().collect();
        if parts.len() >= 4 {
            // PID is parts[0] (already known)
            m.state = parts[1].to_string();
            // RSS is in KB on macOS ps
            m.rss_bytes = parts[2].parse::<u64>().unwrap_or(0) * 1024;
            // VSZ is in KB
            // We store RSS only; VSZ not in our struct
        }

        // Get thread count via separate ps call
        let thread_output = Command::new("/bin/ps")
            .args(["-M", "-p", &pid.to_string()])
            .output();
        if let Ok(ref out) = thread_output {
            if out.status.success() {
                let thread_stdout = String::from_utf8_lossy(&out.stdout);
                // Each line after the header is a thread
                let thread_count = thread_stdout.lines().count().saturating_sub(1);
                m.threads = thread_count as u32;
            }
        }

        // I/O bytes: not easily available on macOS without dtrace/root.
        // Leave as 0 (graceful degradation, same as /proc/io EACCES on Linux).
        m.read_bytes = 0;
        m.write_bytes = 0;

        // Context switches and fd_count not readily available via ps on macOS.
        // Leave as defaults (0).

        Ok(Some(m))
    }

    // ── Service lifecycle ──────────────────────────────────────────

    fn service_state(&self, service: &str) -> Result<ServiceActive, String> {
        let label = Self::service_to_label(service);

        // Try `launchctl print system/<label>` to get the state.
        match self.launchctl_print(&label) {
            Ok(output) => {
                // Look for "state = running" or "state = waiting" etc.
                for line in output.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("state =") || trimmed.starts_with("state=") {
                        let val = trimmed
                            .split('=')
                            .nth(1)
                            .unwrap_or("")
                            .trim()
                            .to_lowercase();
                        return Ok(match val.as_str() {
                            "running" => ServiceActive::Active,
                            "waiting" => ServiceActive::Inactive,
                            "not running" => ServiceActive::Inactive,
                            _ => ServiceActive::Unknown,
                        });
                    }
                }

                // If we got output but no state line, check for PID presence
                if output.contains("\"PID\"") {
                    Ok(ServiceActive::Active)
                } else {
                    Ok(ServiceActive::Unknown)
                }
            }
            Err(_) => {
                // Service not loaded or doesn't exist — check if plist exists
                let plist = Self::plist_path(&label);
                if plist.exists() {
                    // Plist exists but service not loaded
                    Ok(ServiceActive::Inactive)
                } else {
                    Ok(ServiceActive::Unknown)
                }
            }
        }
    }

    fn restart_service(&self, service: &str) -> Result<(), String> {
        let label = Self::service_to_label(service);
        let target = format!("system/{label}");
        log::info!("Restarting service {label} via launchctl kickstart -k");

        let output = Command::new("sudo")
            .args(["launchctl", "kickstart", "-k", &target])
            .output()
            .map_err(|e| format!("launchctl kickstart -k {target}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "launchctl kickstart -k {target} failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(())
    }

    fn stop_service(&self, service: &str) -> Result<(), String> {
        let label = Self::service_to_label(service);
        let target = format!("system/{label}");
        log::info!("Stopping service {label} via launchctl kill SIGTERM");

        let output = Command::new("sudo")
            .args(["launchctl", "kill", "SIGTERM", &target])
            .output()
            .map_err(|e| format!("launchctl kill SIGTERM {target}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "launchctl kill SIGTERM {target} failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(())
    }

    fn kill_service(&self, service: &str) -> Result<(), String> {
        let label = Self::service_to_label(service);
        let target = format!("system/{label}");
        log::info!("Sending SIGKILL to service {label}");

        let output = Command::new("sudo")
            .args(["launchctl", "kill", "SIGKILL", &target])
            .output()
            .map_err(|e| format!("launchctl kill SIGKILL {target}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "launchctl kill SIGKILL {target} failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(())
    }

    fn reset_failed_service(&self, _service: &str) -> Result<(), String> {
        // launchd doesn't have a "failed" state that needs resetting.
        // Services either run or they don't. No-op on macOS.
        Ok(())
    }

    // ── Identity / immutability ────────────────────────────────────

    fn set_immutable(&self, path: &Path, immutable: bool) -> Result<(), String> {
        let flag = if immutable { "uchg" } else { "nouchg" };
        log::debug!("set_immutable: chflags {} on {}", flag, path.display());

        let output = Command::new("chflags")
            .args([flag, &path.to_string_lossy()])
            .output()
            .map_err(|e| format!("chflags {flag} {}: {e}", path.display()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "chflags {flag} {} failed (exit {}): {stderr}",
                path.display(),
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(())
    }

    // ── I/O pressure (PSI) ─────────────────────────────────────────

    fn io_pressure_avg10(&self) -> Result<Option<f32>, String> {
        // macOS has no PSI subsystem.
        Ok(None)
    }

    // ── sd_notify (no-op on macOS) ─────────────────────────────────

    fn notify_ready(&self) -> Result<(), String> {
        Ok(())
    }

    fn notify_watchdog(&self) -> Result<(), String> {
        Ok(())
    }

    fn notify_stopping(&self) -> Result<(), String> {
        Ok(())
    }

    // ── Service unit management ────────────────────────────────────

    fn install_service(&self, name: &str, content: &str) -> Result<(), String> {
        let label = Self::service_to_label(name);
        let plist = Self::plist_path(&label);
        log::info!("Installing launchd plist: {}", plist.display());

        // Write the plist file via sudo tee
        let mut child = Command::new("sudo")
            .args(["tee", &plist.to_string_lossy()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("install_service: spawn tee: {e}"))?;

        if let Some(ref mut stdin) = child.stdin {
            use std::io::Write;
            stdin
                .write_all(content.as_bytes())
                .map_err(|e| format!("install_service: write to tee: {e}"))?;
        }
        drop(child.stdin.take());

        let status = child
            .wait()
            .map_err(|e| format!("install_service: wait tee: {e}"))?;
        if !status.success() {
            return Err(format!(
                "install_service: tee failed (exit {})",
                status.code().unwrap_or(-1)
            ));
        }

        // Load the plist
        let output = Command::new("sudo")
            .args(["launchctl", "load", "-w", &plist.to_string_lossy()])
            .output()
            .map_err(|e| format!("launchctl load {}: {e}", plist.display()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "launchctl load {} failed (exit {}): {stderr}",
                plist.display(),
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(())
    }

    fn uninstall_service(&self, name: &str) -> Result<(), String> {
        let label = Self::service_to_label(name);
        let plist = Self::plist_path(&label);
        log::info!("Uninstalling launchd plist: {}", plist.display());

        // Unload the plist (best-effort)
        if plist.exists() {
            let _ = Command::new("sudo")
                .args(["launchctl", "unload", "-w", &plist.to_string_lossy()])
                .output();
        }

        // Remove the plist file
        let output = Command::new("sudo")
            .args(["rm", "-f", &plist.to_string_lossy()])
            .output()
            .map_err(|e| format!("uninstall_service: rm: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("uninstall_service: rm failed: {stderr}"));
        }

        Ok(())
    }

    fn enable_service(&self, name: &str) -> Result<(), String> {
        let label = Self::service_to_label(name);
        let plist = Self::plist_path(&label);
        log::info!("Enabling service {label}");

        // On macOS, loading a plist with -w enables it.
        if plist.exists() {
            let output = Command::new("sudo")
                .args(["launchctl", "load", "-w", &plist.to_string_lossy()])
                .output()
                .map_err(|e| format!("launchctl load -w {}: {e}", plist.display()))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!(
                    "launchctl load -w {} failed (exit {}): {stderr}",
                    plist.display(),
                    output.status.code().unwrap_or(-1)
                ));
            }
        } else {
            return Err(format!(
                "Plist not found: {}. Install the service first.",
                plist.display()
            ));
        }

        Ok(())
    }

    // ── Service uptime ─────────────────────────────────────────────

    fn service_uptime_secs(&self, service: &str) -> Result<Option<u64>, String> {
        let label = Self::service_to_label(service);

        // Try to parse uptime from `launchctl print` output.
        // The output may contain a line like:
        //   last exit reason = ...
        //   runs = N
        //   pid = 12345
        //   active count = N
        match self.launchctl_print(&label) {
            Ok(output) => {
                // Look for "last spawn time" or similar timestamp info.
                // launchctl print doesn't provide a direct uptime field,
                // so we fall back to checking the process start time via ps.
                if let Ok(Some(pid)) = self.find_service_pid(service) {
                    let ps_output = Command::new("/bin/ps")
                        .args(["-o", "etime=", "-p", &pid.to_string()])
                        .output()
                        .map_err(|e| format!("ps etime for pid {pid}: {e}"))?;

                    if ps_output.status.success() {
                        let etime = String::from_utf8_lossy(&ps_output.stdout)
                            .trim()
                            .to_string();
                        return Ok(parse_etime(&etime));
                    }
                }
                // If we have output but couldn't determine uptime
                let _ = output;
                Ok(None)
            }
            Err(_) => Ok(None),
        }
    }

    // ── User management ────────────────────────────────────────────

    fn create_system_user(&self, name: &str, home: &Path) -> Result<(), String> {
        log::info!("Creating system user '{name}' with home {}", home.display());

        // Check if user already exists
        if self.user_exists(name)? {
            log::info!("User '{name}' already exists");
            return Ok(());
        }

        // Find next available UID in the system range (< 500 on macOS)
        let uid = find_next_system_uid().map_err(|e| format!("find_next_system_uid: {e}"))?;

        // Create user via dscl
        let user_path = format!("/Users/{name}");
        let uid_str = uid.to_string();
        let home_str = home.to_string_lossy().to_string();
        let real_name = format!("OuterClaw {name} service");

        // Create user record
        run_sudo(&["dscl", ".", "-create", &user_path])?;
        run_sudo(&[
            "dscl",
            ".",
            "-create",
            &user_path,
            "UserShell",
            "/usr/bin/false",
        ])?;
        run_sudo(&["dscl", ".", "-create", &user_path, "UniqueID", &uid_str])?;
        run_sudo(&["dscl", ".", "-create", &user_path, "PrimaryGroupID", "20"])?;
        run_sudo(&[
            "dscl",
            ".",
            "-create",
            &user_path,
            "NFSHomeDirectory",
            &home_str,
        ])?;
        run_sudo(&["dscl", ".", "-create", &user_path, "RealName", &real_name])?;

        // Create home directory
        let _ = Command::new("sudo")
            .args(["mkdir", "-p", &home_str])
            .status();
        let _ = Command::new("sudo")
            .args(["chown", &format!("{name}:staff"), &home_str])
            .status();

        Ok(())
    }

    fn user_exists(&self, name: &str) -> Result<bool, String> {
        let user_path = format!("/Users/{name}");
        let output = Command::new("dscl")
            .args([".", "-read", &user_path])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("dscl read {name}: {e}"))?;

        Ok(output.success())
    }

    fn delete_user(&self, name: &str) -> Result<(), String> {
        log::info!("Deleting user '{name}'");

        if !self.user_exists(name)? {
            log::info!("User '{name}' does not exist");
            return Ok(());
        }

        let user_path = format!("/Users/{name}");
        run_sudo(&["dscl", ".", "-delete", &user_path])?;

        Ok(())
    }

    // ── Disk ───────────────────────────────────────────────────────

    fn disk_usage_mb(&self, path: &Path) -> Result<u64, String> {
        let bytes = Self::dir_size_bytes(path)?;
        Ok(bytes / (1024 * 1024))
    }

    // ── Identification ─────────────────────────────────────────────

    fn platform_name(&self) -> &str {
        "macos-launchd"
    }
}

/// Parse ps `etime` format to seconds.
///
/// Format: `[[DD-]HH:]MM:SS`
/// Examples: "01:23", "1:01:23", "2-01:23:45"
fn parse_etime(etime: &str) -> Option<u64> {
    let etime = etime.trim();
    if etime.is_empty() || etime == "-" {
        return None;
    }

    let (days, rest) = if let Some(pos) = etime.find('-') {
        let d: u64 = etime[..pos].parse().ok()?;
        (d, &etime[pos + 1..])
    } else {
        (0, etime)
    };

    let parts: Vec<&str> = rest.split(':').collect();
    let secs = match parts.len() {
        2 => {
            let mins: u64 = parts[0].parse().ok()?;
            let secs: u64 = parts[1].parse().ok()?;
            mins * 60 + secs
        }
        3 => {
            let hours: u64 = parts[0].parse().ok()?;
            let mins: u64 = parts[1].parse().ok()?;
            let secs: u64 = parts[2].parse().ok()?;
            hours * 3600 + mins * 60 + secs
        }
        _ => return None,
    };

    Some(days * 86400 + secs)
}

/// Find the next available system UID (range 400-499 on macOS).
fn find_next_system_uid() -> Result<u32, String> {
    let output = Command::new("dscl")
        .args([".", "-list", "/Users", "UniqueID"])
        .output()
        .map_err(|e| format!("dscl list UIDs: {e}"))?;

    if !output.status.success() {
        return Err("dscl list UIDs failed".into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut used_uids: Vec<u32> = Vec::new();

    for line in stdout.lines() {
        if let Some(uid_str) = line.split_whitespace().last() {
            if let Ok(uid) = uid_str.parse::<u32>() {
                used_uids.push(uid);
            }
        }
    }

    // Find first free UID in 400..499 (macOS system daemon range)
    for uid in 400..500 {
        if !used_uids.contains(&uid) {
            return Ok(uid);
        }
    }

    Err("No available system UIDs in range 400-499".into())
}

/// Run a command with sudo.
fn run_sudo(args: &[&str]) -> Result<(), String> {
    let output = Command::new("sudo")
        .args(args)
        .output()
        .map_err(|e| format!("sudo {}: {e}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "sudo {} failed (exit {}): {stderr}",
            args.join(" "),
            output.status.code().unwrap_or(-1)
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_name() {
        let p = MacOSLaunchd::new();
        assert_eq!(p.platform_name(), "macos-launchd");
    }

    #[test]
    fn test_service_to_label() {
        assert_eq!(
            MacOSLaunchd::service_to_label("oc-outerclaw.service"),
            "com.outerclaw.oc-outerclaw"
        );
        assert_eq!(
            MacOSLaunchd::service_to_label("openclaw-gateway.service"),
            "com.outerclaw.openclaw-gateway"
        );
        assert_eq!(
            MacOSLaunchd::service_to_label("oc-snapshot"),
            "com.outerclaw.oc-snapshot"
        );
    }

    #[test]
    fn test_plist_path() {
        let path = MacOSLaunchd::plist_path("com.outerclaw.oc-outerclaw");
        assert_eq!(
            path,
            PathBuf::from("/Library/LaunchDaemons/com.outerclaw.oc-outerclaw.plist")
        );
    }

    #[test]
    fn test_parse_etime() {
        assert_eq!(parse_etime("01:23"), Some(83));
        assert_eq!(parse_etime("1:01:23"), Some(3683));
        assert_eq!(
            parse_etime("2-01:23:45"),
            Some(2 * 86400 + 3600 + 23 * 60 + 45)
        );
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("-"), None);
    }

    #[test]
    fn test_io_pressure_none() {
        let p = MacOSLaunchd::new();
        assert_eq!(p.io_pressure_avg10().unwrap(), None);
    }

    #[test]
    fn test_notify_noop() {
        let p = MacOSLaunchd::new();
        assert!(p.notify_ready().is_ok());
        assert!(p.notify_watchdog().is_ok());
        assert!(p.notify_stopping().is_ok());
    }

    #[test]
    fn test_disk_usage_nonexistent() {
        let bytes =
            MacOSLaunchd::dir_size_bytes(Path::new("/tmp/outerclaw_does_not_exist_macos")).unwrap();
        assert_eq!(bytes, 0);
    }
}
