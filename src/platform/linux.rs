//! Linux + systemd platform implementation for OuterClaw.
//!
//! This module provides the concrete [`LinuxSystemd`] struct that implements
//! the [`Platform`](super::Platform) trait using:
//!
//! - `/proc` for process metrics and I/O pressure (PSI)
//! - `systemctl` for service lifecycle management
//! - Unix datagram sockets for `sd_notify` (watchdog protocol)
//! - `ioctl(FS_IOC_GETFLAGS/SETFLAGS)` for immutable-bit manipulation
//! - Standard coreutils (`useradd`, `userdel`, `id`) for user management
//!
//! All subprocess calls go through [`std::process::Command`].  Errors are
//! returned as human-readable `String`s because they may surface in Telegram
//! alerts and audit logs.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Platform, ProcessMetrics, ServiceActive};

// ── ioctl constants for ext2/ext4 immutable flag ──────────────────────
// These match the kernel headers <linux/fs.h>:
//   FS_IOC_GETFLAGS = _IOR('f', 1, long)  = 0x80086601
//   FS_IOC_SETFLAGS = _IOW('f', 2, long)  = 0x40086602
//   FS_IMMUTABLE_FL                        = 0x00000010
const FS_IOC_GETFLAGS: libc::c_ulong = 0x80086601;
const FS_IOC_SETFLAGS: libc::c_ulong = 0x40086602;
const FS_IMMUTABLE_FL: libc::c_long = 0x00000010;

/// Linux platform implementation backed by systemd.
pub struct LinuxSystemd {
    /// The `NOTIFY_SOCKET` address captured (and removed from the
    /// environment) at construction time, so child processes don't inherit
    /// it and trigger "reception only permitted for main PID" warnings.
    notify_socket: Option<String>,
}

impl LinuxSystemd {
    /// Create a new instance, consuming `NOTIFY_SOCKET` from the
    /// environment.
    pub fn new() -> Self {
        // Pop NOTIFY_SOCKET from env so subprocesses don't inherit it.
        let notify_socket = std::env::var("NOTIFY_SOCKET").ok();
        if notify_socket.is_some() {
            std::env::remove_var("NOTIFY_SOCKET");
        }
        Self { notify_socket }
    }

    // ── Internal helpers ──────────────────────────────────────────

    /// Send a message to the systemd notification socket.
    fn sd_notify(&self, state: &str) -> Result<(), String> {
        let addr = match &self.notify_socket {
            Some(a) => a.clone(),
            None => return Ok(()), // No socket → silently succeed.
        };

        // Abstract sockets: leading '@' → replace with NUL byte.
        let addr_bytes: Vec<u8> = if let Some(rest) = addr.strip_prefix('@') {
            let mut v = vec![0u8]; // leading NUL
            v.extend_from_slice(rest.as_bytes());
            v
        } else {
            addr.as_bytes().to_vec()
        };

        let sock = UnixDatagram::unbound()
            .map_err(|e| format!("sd_notify: socket creation failed: {e}"))?;

        // Use nix for abstract socket address, or send_to for filesystem path.
        // For abstract sockets we need to use libc directly since std
        // UnixDatagram doesn't support abstract namespace directly.
        if addr.starts_with('@') || addr.starts_with('\0') {
            // Build a sockaddr_un with abstract name.
            let fd = sock.as_raw_fd();
            let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            sa.sun_family = libc::AF_UNIX as libc::sa_family_t;

            let max_path = sa.sun_path.len();
            if addr_bytes.len() > max_path {
                return Err("sd_notify: abstract socket path too long".into());
            }
            for (i, &b) in addr_bytes.iter().enumerate() {
                sa.sun_path[i] = b as libc::c_char;
            }

            let sa_len = std::mem::size_of::<libc::sa_family_t>() + addr_bytes.len();
            let payload = state.as_bytes();

            let ret = unsafe {
                libc::sendto(
                    fd,
                    payload.as_ptr() as *const libc::c_void,
                    payload.len(),
                    libc::MSG_NOSIGNAL,
                    &sa as *const libc::sockaddr_un as *const libc::sockaddr,
                    sa_len as libc::socklen_t,
                )
            };

            if ret < 0 {
                return Err(format!(
                    "sd_notify: sendto failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
        } else {
            // Filesystem path socket.
            sock.send_to(state.as_bytes(), &addr)
                .map_err(|e| format!("sd_notify: send_to {addr} failed: {e}"))?;
        }

        Ok(())
    }

    /// Run a `systemctl show` command and return stdout, trimmed.
    fn systemctl_show(&self, service: &str, property: &str) -> Result<String, String> {
        let output = Command::new("systemctl")
            .args([
                "show",
                service,
                &format!("--property={property}"),
                "--value",
            ])
            .output()
            .map_err(|e| format!("systemctl show {property}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "systemctl show {property} failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Run `sudo systemctl <action> [service]`.
    ///
    /// If `service` is empty (e.g. for `daemon-reload`), it is omitted from
    /// the command line.
    fn sudo_systemctl(&self, action: &str, service: &str) -> Result<(), String> {
        let mut cmd = Command::new("sudo");
        cmd.arg("systemctl").arg(action);
        if !service.is_empty() {
            cmd.arg(service);
        }

        let output = cmd
            .output()
            .map_err(|e| format!("sudo systemctl {action} {service}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "sudo systemctl {action} {service} failed (exit {}): {stderr}",
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
            // Symlinks, sockets, etc. — skip silently.
        }
        Ok(total)
    }
}

impl Platform for LinuxSystemd {
    // ── Process / Service discovery ────────────────────────────────

    fn find_service_pid(&self, service: &str) -> Result<Option<u32>, String> {
        let raw = self.systemctl_show(service, "MainPID")?;
        let pid: u32 = match raw.parse() {
            Ok(p) => p,
            Err(_) => {
                log::debug!("MainPID parse failed for '{raw}'");
                return Ok(None);
            }
        };

        if pid == 0 {
            return Ok(None);
        }

        // Verify the process actually exists.
        let proc_path = PathBuf::from(format!("/proc/{pid}"));
        if proc_path.exists() {
            Ok(Some(pid))
        } else {
            log::debug!("MainPID {pid} reported but /proc/{pid} gone");
            Ok(None)
        }
    }

    fn collect_proc_metrics(&self, pid: u32) -> Result<Option<ProcessMetrics>, String> {
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        if !proc_dir.exists() {
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

        // ── /proc/<pid>/status ────────────────────────────────────
        let status_path = proc_dir.join("status");
        match fs::read_to_string(&status_path) {
            Ok(text) => {
                for line in text.lines() {
                    if let Some(rest) = line.strip_prefix("State:") {
                        m.state = rest.split_whitespace().next().unwrap_or("").to_string();
                    } else if let Some(rest) = line.strip_prefix("Threads:") {
                        m.threads = rest.trim().parse().unwrap_or(0);
                    } else if let Some(rest) = line.strip_prefix("VmRSS:") {
                        // Value is in kB.
                        if let Some(kb_str) = rest.split_whitespace().next() {
                            m.rss_bytes = kb_str.parse::<u64>().unwrap_or(0) * 1024;
                        }
                    } else if let Some(rest) = line.strip_prefix("voluntary_ctxt_switches:") {
                        m.voluntary_ctxt_switches = rest.trim().parse().unwrap_or(0);
                    } else if let Some(rest) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
                        m.nonvoluntary_ctxt_switches = rest.trim().parse().unwrap_or(0);
                    }
                }
            }
            Err(e) => {
                // Process likely exited between our exists() check and the read.
                log::debug!("Failed to read {}: {e}", status_path.display());
                return Ok(None);
            }
        }

        // ── /proc/<pid>/io ────────────────────────────────────────
        // May fail with EACCES if we lack permissions (non-root).
        let io_path = proc_dir.join("io");
        match fs::read_to_string(&io_path) {
            Ok(text) => {
                for line in text.lines() {
                    if let Some(rest) = line.strip_prefix("read_bytes:") {
                        m.read_bytes = rest.trim().parse().unwrap_or(0);
                    } else if let Some(rest) = line.strip_prefix("write_bytes:") {
                        m.write_bytes = rest.trim().parse().unwrap_or(0);
                    }
                }
            }
            Err(e) => {
                log::debug!(
                    "Failed to read {}: {e} (may need CAP_SYS_PTRACE)",
                    io_path.display()
                );
            }
        }

        // ── /proc/<pid>/fd count ──────────────────────────────────
        let fd_path = proc_dir.join("fd");
        match fs::read_dir(&fd_path) {
            Ok(entries) => {
                m.fd_count = entries.count() as u32;
            }
            Err(e) => {
                log::debug!("Failed to read {}: {e}", fd_path.display());
            }
        }

        Ok(Some(m))
    }

    // ── Service lifecycle ──────────────────────────────────────────

    fn service_state(&self, service: &str) -> Result<ServiceActive, String> {
        let raw = self.systemctl_show(service, "ActiveState")?;
        let state = match raw.as_str() {
            "active" => ServiceActive::Active,
            "inactive" | "deactivating" => ServiceActive::Inactive,
            "failed" => ServiceActive::Failed,
            "activating" | "reloading" => ServiceActive::Activating,
            _ => {
                log::debug!("Unknown ActiveState '{raw}' for {service}");
                ServiceActive::Unknown
            }
        };
        Ok(state)
    }

    fn restart_service(&self, service: &str) -> Result<(), String> {
        log::info!("Restarting service {service}");
        self.sudo_systemctl("restart", service)
    }

    fn stop_service(&self, service: &str) -> Result<(), String> {
        log::info!("Stopping service {service}");
        self.sudo_systemctl("stop", service)
    }

    fn kill_service(&self, service: &str) -> Result<(), String> {
        log::info!("Sending SIGKILL to service {service}");
        let output = Command::new("sudo")
            .args(["systemctl", "kill", "--signal=SIGKILL", service])
            .output()
            .map_err(|e| format!("sudo systemctl kill {service}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "sudo systemctl kill {service} failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }
        Ok(())
    }

    fn reset_failed_service(&self, service: &str) -> Result<(), String> {
        log::info!("Resetting failed state for {service}");
        self.sudo_systemctl("reset-failed", service)
    }

    // ── Identity / immutability ────────────────────────────────────

    fn set_immutable(&self, path: &Path, immutable: bool) -> Result<(), String> {
        let file = fs::File::open(path)
            .map_err(|e| format!("set_immutable: open {}: {e}", path.display()))?;
        let fd = file.as_raw_fd();

        // Read current flags.
        let mut flags: libc::c_long = 0;
        #[allow(clippy::useless_conversion)]
        let ret = unsafe { libc::ioctl(fd, FS_IOC_GETFLAGS as _, &mut flags) };
        if ret < 0 {
            return Err(format!(
                "set_immutable: FS_IOC_GETFLAGS on {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }

        // Set or clear the immutable bit.
        if immutable {
            flags |= FS_IMMUTABLE_FL;
        } else {
            flags &= !FS_IMMUTABLE_FL;
        }

        #[allow(clippy::useless_conversion)]
        let ret = unsafe { libc::ioctl(fd, FS_IOC_SETFLAGS as _, &flags) };
        if ret < 0 {
            return Err(format!(
                "set_immutable: FS_IOC_SETFLAGS on {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }

        log::debug!(
            "set_immutable: {} flag {} on {}",
            if immutable { "set" } else { "cleared" },
            if immutable { "+i" } else { "-i" },
            path.display()
        );
        Ok(())
    }

    // ── I/O pressure (PSI) ─────────────────────────────────────────

    fn io_pressure_avg10(&self) -> Result<Option<f32>, String> {
        let psi_path = Path::new("/proc/pressure/io");
        if !psi_path.exists() {
            return Ok(None);
        }

        let content =
            fs::read_to_string(psi_path).map_err(|e| format!("read /proc/pressure/io: {e}"))?;

        // Format: some avg10=0.00 avg60=0.00 avg300=0.00 total=123456
        for line in content.lines() {
            if line.starts_with("some") {
                for token in line.split_whitespace() {
                    if let Some(val_str) = token.strip_prefix("avg10=") {
                        match val_str.parse::<f32>() {
                            Ok(v) => return Ok(Some(v)),
                            Err(e) => {
                                log::warn!("Failed to parse PSI avg10 '{val_str}': {e}");
                                return Ok(None);
                            }
                        }
                    }
                }
            }
        }

        log::debug!("No 'some' line found in /proc/pressure/io");
        Ok(None)
    }

    // ── sd_notify ──────────────────────────────────────────────────

    fn notify_ready(&self) -> Result<(), String> {
        self.sd_notify("READY=1")
    }

    fn notify_watchdog(&self) -> Result<(), String> {
        self.sd_notify("WATCHDOG=1")
    }

    fn notify_stopping(&self) -> Result<(), String> {
        self.sd_notify("STOPPING=1")
    }

    // ── Service unit management ────────────────────────────────────

    fn install_service(&self, name: &str, content: &str) -> Result<(), String> {
        let unit_path = PathBuf::from(format!("/etc/systemd/system/{name}"));
        log::info!("Installing service unit: {}", unit_path.display());

        // Write the unit file via sudo tee (we may not have direct write
        // access to /etc/systemd/system).
        let mut child = Command::new("sudo")
            .args(["tee", &unit_path.to_string_lossy()])
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
        // Close stdin so tee can finish.
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

        // daemon-reload
        self.sudo_systemctl("daemon-reload", "")?;
        Ok(())
    }

    fn uninstall_service(&self, name: &str) -> Result<(), String> {
        let unit_path = PathBuf::from(format!("/etc/systemd/system/{name}"));
        log::info!("Uninstalling service unit: {}", unit_path.display());

        // Best-effort stop + disable.
        let _ = self.sudo_systemctl("stop", name);
        let _ = self.sudo_systemctl("disable", name);

        // Remove the unit file.
        let output = Command::new("sudo")
            .args(["rm", "-f", &unit_path.to_string_lossy()])
            .output()
            .map_err(|e| format!("uninstall_service: rm: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("uninstall_service: rm failed: {stderr}"));
        }

        // Also remove timer if it exists.
        let timer_name = name.replace(".service", ".timer");
        if timer_name != name {
            let timer_path = PathBuf::from(format!("/etc/systemd/system/{timer_name}"));
            if timer_path.exists() {
                let _ = self.sudo_systemctl("stop", &timer_name);
                let _ = self.sudo_systemctl("disable", &timer_name);
                let _ = Command::new("sudo")
                    .args(["rm", "-f", &timer_path.to_string_lossy()])
                    .output();
            }
        }

        self.sudo_systemctl("daemon-reload", "")?;
        Ok(())
    }

    fn enable_service(&self, name: &str) -> Result<(), String> {
        log::info!("Enabling service {name}");
        self.sudo_systemctl("enable", name)
    }

    // ── Service uptime ─────────────────────────────────────────────

    fn service_uptime_secs(&self, service: &str) -> Result<Option<u64>, String> {
        let raw = self.systemctl_show(service, "ActiveEnterTimestampMonotonic")?;
        let mono_us: u64 = match raw.parse() {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };

        if mono_us == 0 {
            return Ok(None);
        }

        // Current monotonic time from /proc/uptime (first field, in seconds).
        let uptime_text =
            fs::read_to_string("/proc/uptime").map_err(|e| format!("read /proc/uptime: {e}"))?;
        let sys_uptime_secs: f64 = uptime_text
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| "Failed to parse /proc/uptime".to_string())?;

        let enter_secs = mono_us as f64 / 1_000_000.0;
        let uptime = sys_uptime_secs - enter_secs;

        if uptime > 0.0 {
            Ok(Some(uptime as u64))
        } else {
            Ok(None)
        }
    }

    // ── User management ────────────────────────────────────────────

    fn create_system_user(&self, name: &str, home: &Path) -> Result<(), String> {
        log::info!("Creating system user '{name}' with home {}", home.display());

        let output = Command::new("sudo")
            .args([
                "useradd",
                "--system",
                "--shell",
                "/usr/sbin/nologin",
                "--home-dir",
                &home.to_string_lossy(),
                "--create-home",
                name,
            ])
            .output()
            .map_err(|e| format!("useradd {name}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Exit code 9 = user already exists — treat as success.
            if output.status.code() == Some(9) {
                log::info!("User '{name}' already exists");
                return Ok(());
            }
            return Err(format!(
                "useradd {name} failed (exit {}): {stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        Ok(())
    }

    fn user_exists(&self, name: &str) -> Result<bool, String> {
        let output = Command::new("id")
            .arg(name)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("id {name}: {e}"))?;

        Ok(output.success())
    }

    fn delete_user(&self, name: &str) -> Result<(), String> {
        log::info!("Deleting user '{name}'");

        let output = Command::new("sudo")
            .args(["userdel", name])
            .output()
            .map_err(|e| format!("userdel {name}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Exit code 6 = user doesn't exist — treat as success.
            if output.status.code() == Some(6) {
                log::info!("User '{name}' does not exist");
                return Ok(());
            }
            return Err(format!(
                "userdel {name} failed (exit {}): {stderr}",
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
        "linux-systemd"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_name() {
        let p = LinuxSystemd::new();
        assert_eq!(p.platform_name(), "linux-systemd");
    }

    #[test]
    fn test_io_pressure_parsing() {
        // This test only runs on Linux with PSI support; skip otherwise.
        let p = LinuxSystemd::new();
        match p.io_pressure_avg10() {
            Ok(Some(v)) => assert!(v >= 0.0, "avg10 should be non-negative"),
            Ok(None) => {} // PSI not available — acceptable.
            Err(e) => {
                // Permission errors are acceptable in CI.
                assert!(
                    e.contains("Permission denied") || e.contains("Operation not permitted"),
                    "Unexpected error: {e}"
                );
            }
        }
    }

    #[test]
    fn test_disk_usage_tmp() {
        let dir = std::env::temp_dir().join("outerclaw_test_disk_usage");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "hello").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/b.txt"), "world!").unwrap();

        let bytes = LinuxSystemd::dir_size_bytes(&dir).unwrap();
        assert_eq!(bytes, 11); // "hello" (5) + "world!" (6)

        let mb = LinuxSystemd::new().disk_usage_mb(&dir).unwrap();
        assert_eq!(mb, 0); // 11 bytes rounds to 0 MiB

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_disk_usage_nonexistent() {
        let bytes =
            LinuxSystemd::dir_size_bytes(Path::new("/tmp/outerclaw_does_not_exist_xyz")).unwrap();
        assert_eq!(bytes, 0);
    }

    #[test]
    fn test_user_exists_root() {
        let p = LinuxSystemd::new();
        assert!(p.user_exists("root").unwrap());
    }

    #[test]
    fn test_user_exists_bogus() {
        let p = LinuxSystemd::new();
        assert!(!p.user_exists("outerclaw_bogus_user_xyzzy").unwrap());
    }

    #[test]
    fn test_collect_proc_metrics_self() {
        let p = LinuxSystemd::new();
        let pid = std::process::id();
        let m = p.collect_proc_metrics(pid).unwrap();
        assert!(m.is_some(), "Should be able to read own /proc");
        let m = m.unwrap();
        assert_eq!(m.pid, pid);
        assert!(!m.state.is_empty(), "State should be non-empty");
        assert!(m.threads >= 1, "Should have at least one thread");
        assert!(m.timestamp > 0.0);
    }

    #[test]
    fn test_collect_proc_metrics_gone() {
        let p = LinuxSystemd::new();
        // PID 999999999 almost certainly doesn't exist.
        let m = p.collect_proc_metrics(999_999_999).unwrap();
        assert!(m.is_none());
    }

    #[test]
    fn test_service_active_display() {
        assert_eq!(ServiceActive::Active.to_string(), "active");
        assert_eq!(ServiceActive::Failed.to_string(), "failed");
        assert_eq!(ServiceActive::Unknown.to_string(), "unknown");
    }
}
