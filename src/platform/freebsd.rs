//! FreeBSD + rc.d platform implementation for OuterClaw.
//!
//! This module provides the concrete [`FreeBSDRc`] struct that implements
//! the [`Platform`](super::Platform) trait using:
//!
//! - `ps` and `procstat` for process metrics
//! - `service(8)` and `sysrc(8)` for service lifecycle management (rc.d)
//! - `chflags schg/noschg` for immutable-bit manipulation
//! - `pw(8)` for user management
//!
//! FreeBSD has no PSI (I/O pressure) or sd_notify — those methods are no-ops.
//!
//! This module is gated by `#[cfg(target_os = "freebsd")]` in the parent `mod.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Platform, ProcessMetrics, ServiceActive};

/// FreeBSD platform implementation backed by rc.d.
pub struct FreeBSDRc;

impl FreeBSDRc {
    /// Create a new instance.
    pub fn new() -> Self {
        Self
    }

    // ── Internal helpers ──────────────────────────────────────────

    /// Derive the rc.d service name from a systemd-style service name.
    ///
    /// Strips the `.service` suffix if present.
    fn service_name(service: &str) -> &str {
        service.strip_suffix(".service").unwrap_or(service)
    }

    /// Run `service <name> <action>` and return the output.
    fn run_service_cmd(&self, name: &str, action: &str) -> Result<std::process::Output, String> {
        Command::new("service")
            .args([name, action])
            .output()
            .map_err(|e| format!("service {name} {action}: {e}"))
    }

    /// Run `service <name> <action>` via sudo.
    fn sudo_service(&self, name: &str, action: &str) -> Result<(), String> {
        let output = Command::new("sudo")
            .args(["service", name, action])
            .output()
            .map_err(|e| format!("sudo service {name} {action}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "sudo service {name} {action} failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(())
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

impl Platform for FreeBSDRc {
    // ── Process / Service discovery ────────────────────────────────

    fn find_service_pid(&self, service: &str) -> Result<Option<u32>, String> {
        let name = Self::service_name(service);

        // `service <name> status` typically prints the PID if running.
        // e.g., "<name> is running as pid 12345."
        let output = self.run_service_cmd(name, "status")?;

        if !output.status.success() {
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Parse PID from status output. Common formats:
        //   "oc_outerclaw is running as pid 12345."
        //   "<name> is running as pid 12345."
        for line in stdout.lines() {
            if line.contains("pid") {
                // Find the numeric PID after "pid"
                for word in line.split_whitespace() {
                    let cleaned = word.trim_end_matches('.');
                    if let Ok(pid) = cleaned.parse::<u32>() {
                        if pid > 0 {
                            return Ok(Some(pid));
                        }
                    }
                }
            }
        }

        // Fallback: check PID file
        let pid_file = PathBuf::from(format!("/var/run/{name}.pid"));
        if pid_file.exists() {
            if let Ok(content) = fs::read_to_string(&pid_file) {
                if let Ok(pid) = content.trim().parse::<u32>() {
                    // Verify process exists
                    let proc_path = PathBuf::from(format!("/proc/{pid}"));
                    if proc_path.exists() {
                        return Ok(Some(pid));
                    }
                    // Also check via kill(0)
                    let check = Command::new("kill")
                        .args(["-0", &pid.to_string()])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    if let Ok(status) = check {
                        if status.success() {
                            return Ok(Some(pid));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    fn collect_proc_metrics(&self, pid: u32) -> Result<Option<ProcessMetrics>, String> {
        let mut m = ProcessMetrics {
            pid,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            ..Default::default()
        };

        // Try procstat first (FreeBSD-native)
        let procstat_output = Command::new("procstat")
            .args(["-r", &pid.to_string()])
            .output();

        match procstat_output {
            Ok(ref out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                // procstat -r output format:
                //   PID  COMM  ... RSS  ... (varies by version)
                for line in stdout.lines().skip(1) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    // Try to find RSS value (typically the 4th or later column)
                    if parts.len() >= 4 {
                        // Parse RSS (in pages on FreeBSD, page size typically 4096)
                        if let Ok(rss_pages) = parts.get(3).unwrap_or(&"0").parse::<u64>() {
                            m.rss_bytes = rss_pages * 4096;
                        }
                    }
                }
            }
            _ => {
                // procstat failed — fall back to /proc if mounted
                let proc_status = PathBuf::from(format!("/proc/{pid}/status"));
                if proc_status.exists() {
                    if let Ok(content) = fs::read_to_string(&proc_status) {
                        // FreeBSD /proc/<pid>/status is a single line with space-separated fields
                        let fields: Vec<&str> = content.split_whitespace().collect();
                        if fields.len() >= 8 {
                            // Field 0: command, Field 1: PID, Field 2: PPID, ...
                            // Field 7: RSS (in pages)
                            if let Ok(rss_pages) = fields.get(7).unwrap_or(&"0").parse::<u64>() {
                                m.rss_bytes = rss_pages * 4096;
                            }
                        }
                    }
                }
            }
        }

        // Use ps as a reliable fallback / supplement for state, RSS, VSZ
        let ps_output = Command::new("ps")
            .args(["-o", "pid,state,rss,vsz", "-p", &pid.to_string()])
            .output();

        match ps_output {
            Ok(ref out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let lines: Vec<&str> = stdout.lines().collect();
                if lines.len() >= 2 {
                    let parts: Vec<&str> = lines[1].split_whitespace().collect();
                    if parts.len() >= 4 {
                        m.state = parts[1].to_string();
                        // ps RSS is in KB
                        if m.rss_bytes == 0 {
                            m.rss_bytes = parts[2].parse::<u64>().unwrap_or(0) * 1024;
                        }
                    }
                }
            }
            Ok(_) => {
                // Process doesn't exist
                return Ok(None);
            }
            Err(e) => {
                log::debug!("ps failed for pid {pid}: {e}");
                return Ok(None);
            }
        }

        // Thread count via procstat -t or ps -H
        let thread_output = Command::new("procstat")
            .args(["-t", &pid.to_string()])
            .output();

        match thread_output {
            Ok(ref out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                // Each line after the header is a thread
                let thread_count = stdout.lines().count().saturating_sub(1);
                m.threads = thread_count as u32;
            }
            _ => {
                // Fallback: ps -H -p <pid> counts threads
                let ps_thread = Command::new("ps")
                    .args(["-H", "-p", &pid.to_string()])
                    .output();
                if let Ok(ref out) = ps_thread {
                    if out.status.success() {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let count = stdout.lines().count().saturating_sub(1);
                        m.threads = std::cmp::max(count as u32, 1);
                    }
                }
            }
        }

        // I/O stats: try procstat -f (file descriptors) for fd_count
        let fd_output = Command::new("procstat")
            .args(["-f", &pid.to_string()])
            .output();

        if let Ok(ref out) = fd_output {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let fd_count = stdout.lines().count().saturating_sub(1);
                m.fd_count = fd_count as u32;
            }
        }

        // I/O bytes not easily available without DTrace on FreeBSD — leave as 0.
        m.read_bytes = 0;
        m.write_bytes = 0;

        Ok(Some(m))
    }

    // ── Service lifecycle ──────────────────────────────────────────

    fn service_state(&self, service: &str) -> Result<ServiceActive, String> {
        let name = Self::service_name(service);

        let output = self.run_service_cmd(name, "status")?;

        // `service <name> status` return codes:
        //   0 = running
        //   1 = not running (but enabled)
        //   other = error / not found
        match output.status.code() {
            Some(0) => Ok(ServiceActive::Active),
            Some(1) => {
                // Check if the service is enabled but just not running
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains("not running") {
                    Ok(ServiceActive::Inactive)
                } else {
                    Ok(ServiceActive::Failed)
                }
            }
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stderr.contains("does not exist") || stderr.contains("unknown") {
                    Ok(ServiceActive::Unknown)
                } else {
                    Ok(ServiceActive::Inactive)
                }
            }
        }
    }

    fn restart_service(&self, service: &str) -> Result<(), String> {
        let name = Self::service_name(service);
        log::info!("Restarting service {name}");
        self.sudo_service(name, "restart")
    }

    fn stop_service(&self, service: &str) -> Result<(), String> {
        let name = Self::service_name(service);
        log::info!("Stopping service {name}");
        self.sudo_service(name, "stop")
    }

    fn kill_service(&self, service: &str) -> Result<(), String> {
        let name = Self::service_name(service);
        log::info!("Sending SIGKILL to service {name}");

        // Find PID first, then kill -9
        if let Ok(Some(pid)) = self.find_service_pid(service) {
            let output = Command::new("sudo")
                .args(["kill", "-9", &pid.to_string()])
                .output()
                .map_err(|e| format!("kill -9 {pid}: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!(
                    "kill -9 {pid} failed (exit {}): {stderr}",
                    output.status.code().unwrap_or(-1)
                ));
            }
            Ok(())
        } else {
            Err(format!("Cannot find PID for service {name} to kill"))
        }
    }

    fn reset_failed_service(&self, _service: &str) -> Result<(), String> {
        // rc.d doesn't have a "failed" state concept. No-op.
        Ok(())
    }

    // ── Identity / immutability ────────────────────────────────────

    fn set_immutable(&self, path: &Path, immutable: bool) -> Result<(), String> {
        // FreeBSD uses system immutable flag (schg) — requires securelevel <= 0
        // or root. This is stronger than the user immutable flag (uchg).
        let flag = if immutable { "schg" } else { "noschg" };
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
        // FreeBSD has no PSI subsystem.
        Ok(None)
    }

    // ── sd_notify (no-op on FreeBSD) ───────────────────────────────

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
        let rc_name = Self::service_name(name);
        let rc_path = PathBuf::from(format!("/usr/local/etc/rc.d/{rc_name}"));
        log::info!("Installing rc.d script: {}", rc_path.display());

        // Write the rc.d script via sudo tee
        let mut child = Command::new("sudo")
            .args(["tee", &rc_path.to_string_lossy()])
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

        // Make executable
        let _ = Command::new("sudo")
            .args(["chmod", "755", &rc_path.to_string_lossy()])
            .status();

        Ok(())
    }

    fn uninstall_service(&self, name: &str) -> Result<(), String> {
        let rc_name = Self::service_name(name);
        let rc_path = PathBuf::from(format!("/usr/local/etc/rc.d/{rc_name}"));
        log::info!("Uninstalling rc.d script: {}", rc_path.display());

        // Stop the service first (best-effort)
        let _ = self.sudo_service(rc_name, "stop");

        // Disable via sysrc (best-effort)
        let _ = Command::new("sudo")
            .args(["sysrc", &format!("{rc_name}_enable=NO")])
            .output();

        // Remove the rc.d script
        let output = Command::new("sudo")
            .args(["rm", "-f", &rc_path.to_string_lossy()])
            .output()
            .map_err(|e| format!("uninstall_service: rm: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("uninstall_service: rm failed: {stderr}"));
        }

        Ok(())
    }

    fn enable_service(&self, name: &str) -> Result<(), String> {
        let rc_name = Self::service_name(name);
        log::info!("Enabling service {rc_name} via sysrc");

        let output = Command::new("sudo")
            .args(["sysrc", &format!("{rc_name}_enable=YES")])
            .output()
            .map_err(|e| format!("sysrc {rc_name}_enable=YES: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "sysrc {rc_name}_enable=YES failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(())
    }

    // ── Service uptime ─────────────────────────────────────────────

    fn service_uptime_secs(&self, service: &str) -> Result<Option<u64>, String> {
        // Get the PID, then use ps to determine elapsed time.
        if let Some(pid) = self.find_service_pid(service)? {
            let output = Command::new("ps")
                .args(["-o", "etime=", "-p", &pid.to_string()])
                .output()
                .map_err(|e| format!("ps etime for pid {pid}: {e}"))?;

            if output.status.success() {
                let etime = String::from_utf8_lossy(&output.stdout).trim().to_string();
                return Ok(parse_etime(&etime));
            }
        }

        Ok(None)
    }

    // ── User management ────────────────────────────────────────────

    fn create_system_user(&self, name: &str, home: &Path) -> Result<(), String> {
        log::info!("Creating system user '{name}' with home {}", home.display());

        let output = Command::new("sudo")
            .args([
                "pw",
                "useradd",
                name,
                "-d",
                &home.to_string_lossy(),
                "-s",
                "/usr/sbin/nologin",
                "-m",
                "-c",
                &format!("OuterClaw {name} service"),
            ])
            .output()
            .map_err(|e| format!("pw useradd {name}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "user already exists" is not an error
            if stderr.contains("already exists") || stderr.contains("name already exists") {
                log::info!("User '{name}' already exists");
                return Ok(());
            }
            return Err(format!(
                "pw useradd {name} failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(())
    }

    fn user_exists(&self, name: &str) -> Result<bool, String> {
        let output = Command::new("pw")
            .args(["usershow", name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("pw usershow {name}: {e}"))?;

        Ok(output.success())
    }

    fn delete_user(&self, name: &str) -> Result<(), String> {
        log::info!("Deleting user '{name}'");

        let output = Command::new("sudo")
            .args(["pw", "userdel", name])
            .output()
            .map_err(|e| format!("pw userdel {name}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "no such user" is not an error
            if stderr.contains("no such user") || stderr.contains("does not exist") {
                log::info!("User '{name}' does not exist");
                return Ok(());
            }
            return Err(format!(
                "pw userdel {name} failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(())
    }

    // ── Disk ───────────────────────────────────────────────────────

    fn disk_usage_mb(&self, path: &Path) -> Result<u64, String> {
        let bytes = Self::dir_size_bytes(path)?;
        Ok(bytes / (1024 * 1024))
    }

    // ── Identification ─────────────────────────────────────────────

    fn platform_name(&self) -> &str {
        "freebsd-rc"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_name() {
        let p = FreeBSDRc::new();
        assert_eq!(p.platform_name(), "freebsd-rc");
    }

    #[test]
    fn test_service_name_stripping() {
        assert_eq!(
            FreeBSDRc::service_name("oc-outerclaw.service"),
            "oc-outerclaw"
        );
        assert_eq!(FreeBSDRc::service_name("oc-outerclaw"), "oc-outerclaw");
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
        let p = FreeBSDRc::new();
        assert_eq!(p.io_pressure_avg10().unwrap(), None);
    }

    #[test]
    fn test_notify_noop() {
        let p = FreeBSDRc::new();
        assert!(p.notify_ready().is_ok());
        assert!(p.notify_watchdog().is_ok());
        assert!(p.notify_stopping().is_ok());
    }

    #[test]
    fn test_disk_usage_nonexistent() {
        let bytes =
            FreeBSDRc::dir_size_bytes(Path::new("/tmp/outerclaw_does_not_exist_freebsd")).unwrap();
        assert_eq!(bytes, 0);
    }
}
