//! Container / generic Linux platform implementation for OuterClaw.
//!
//! This module provides the concrete [`LinuxContainer`] struct that implements
//! the [`Platform`](super::Platform) trait for environments without a proper
//! init system — Docker, Podman, LXC, Kubernetes pods, etc.
//!
//! Key differences from `LinuxSystemd`:
//! - Service management (install/uninstall/enable) returns errors — containers
//!   don't have init-managed services.
//! - User management returns errors — container images typically manage users
//!   at build time.
//! - `find_service_pid` walks `/proc/*/cmdline` to find processes by name.
//! - `restart/stop/kill` send signals directly via `nix::sys::signal::kill()`.
//! - `sd_notify` is a no-op — no systemd socket.
//! - Process metrics and I/O pressure use the same `/proc` interfaces as Linux.
//!
//! This module is gated by `#[cfg(target_os = "linux")]` in the parent `mod.rs`.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Platform, ProcessMetrics, ServiceActive};

// ── ioctl constants (same as linux.rs) ──────────────────────────────
const FS_IOC_GETFLAGS: libc::c_ulong = 0x80086601;
const FS_IOC_SETFLAGS: libc::c_ulong = 0x40086602;
const FS_IMMUTABLE_FL: libc::c_long = 0x00000010;

/// Linux container platform implementation (no init system).
pub struct LinuxContainer;

impl LinuxContainer {
    /// Create a new instance.
    pub fn new() -> Self {
        Self
    }

    // ── Internal helpers ──────────────────────────────────────────

    /// Walk `/proc/*/cmdline` looking for a process whose command line
    /// contains the given name pattern.
    ///
    /// Returns the PID of the first match, or `None`.
    fn find_pid_by_cmdline(name: &str) -> Result<Option<u32>, String> {
        let proc_dir = Path::new("/proc");
        let entries = fs::read_dir(proc_dir).map_err(|e| format!("read_dir /proc: {e}"))?;

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            // Only look at numeric directory names (PIDs)
            let fname = entry.file_name();
            let pid_str = fname.to_string_lossy();
            let pid: u32 = match pid_str.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };

            // Skip PID 1 (init/entrypoint) and our own PID
            if pid <= 1 || pid == std::process::id() {
                continue;
            }

            let cmdline_path = PathBuf::from(format!("/proc/{pid}/cmdline"));
            match fs::read(&cmdline_path) {
                Ok(data) => {
                    // cmdline is NUL-separated
                    let cmdline = String::from_utf8_lossy(&data).replace('\0', " ");
                    if cmdline.contains(name) {
                        return Ok(Some(pid));
                    }
                }
                Err(_) => continue, // Process may have exited
            }
        }

        Ok(None)
    }

    /// Check if a PID is alive by checking `/proc/<pid>` existence.
    fn pid_exists(pid: u32) -> bool {
        PathBuf::from(format!("/proc/{pid}")).exists()
    }

    /// Send a signal to a process using nix.
    fn send_signal(pid: u32, signal: nix::sys::signal::Signal) -> Result<(), String> {
        let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
        nix::sys::signal::kill(nix_pid, signal).map_err(|e| format!("kill({pid}, {signal}): {e}"))
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

impl Platform for LinuxContainer {
    // ── Process / Service discovery ────────────────────────────────

    fn find_service_pid(&self, service: &str) -> Result<Option<u32>, String> {
        // In container mode, there's no systemd. We search for the process
        // by name in /proc/*/cmdline.
        //
        // Strip .service suffix and look for the base name.
        let name = service.strip_suffix(".service").unwrap_or(service);

        // First try to find by the service name pattern
        if let Some(pid) = Self::find_pid_by_cmdline(name)? {
            return Ok(Some(pid));
        }

        // For "openclaw-gateway" specifically, also search for "openclaw gateway"
        if name.contains('-') {
            let alt_name = name.replace('-', " ");
            if let Some(pid) = Self::find_pid_by_cmdline(&alt_name)? {
                return Ok(Some(pid));
            }
        }

        Ok(None)
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
                log::debug!("Failed to read {}: {e}", status_path.display());
                return Ok(None);
            }
        }

        // ── /proc/<pid>/io ────────────────────────────────────────
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
        // In container mode, check if the process is running.
        match self.find_service_pid(service)? {
            Some(pid) => {
                if Self::pid_exists(pid) {
                    Ok(ServiceActive::Active)
                } else {
                    Ok(ServiceActive::Inactive)
                }
            }
            None => Ok(ServiceActive::Inactive),
        }
    }

    fn restart_service(&self, service: &str) -> Result<(), String> {
        log::info!("Container restart: sending SIGTERM to {service}, then starting new");

        // Find and stop the existing process
        if let Some(pid) = self.find_service_pid(service)? {
            Self::send_signal(pid, nix::sys::signal::Signal::SIGTERM)?;

            // Wait briefly for process to exit
            for _ in 0..30 {
                if !Self::pid_exists(pid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            // Force kill if still alive
            if Self::pid_exists(pid) {
                let _ = Self::send_signal(pid, nix::sys::signal::Signal::SIGKILL);
            }
        }

        // In container mode, we can't easily "start" a service since there's
        // no init. The process should be restarted by the container runtime
        // or a supervisor.
        log::warn!(
            "Container mode: process stopped. Restart must be handled by \
             container runtime or external supervisor."
        );

        Ok(())
    }

    fn stop_service(&self, service: &str) -> Result<(), String> {
        log::info!("Container stop: sending SIGTERM to {service}");

        if let Some(pid) = self.find_service_pid(service)? {
            Self::send_signal(pid, nix::sys::signal::Signal::SIGTERM)?;
        } else {
            log::debug!("Service {service} not running (no PID found)");
        }

        Ok(())
    }

    fn kill_service(&self, service: &str) -> Result<(), String> {
        log::info!("Container kill: sending SIGKILL to {service}");

        if let Some(pid) = self.find_service_pid(service)? {
            Self::send_signal(pid, nix::sys::signal::Signal::SIGKILL)?;
        } else {
            log::debug!("Service {service} not running (no PID found)");
        }

        Ok(())
    }

    fn reset_failed_service(&self, _service: &str) -> Result<(), String> {
        // No-op — containers don't have a "failed" state to reset.
        Ok(())
    }

    // ── Identity / immutability ────────────────────────────────────

    fn set_immutable(&self, path: &Path, immutable: bool) -> Result<(), String> {
        // Same ioctl approach as LinuxSystemd.
        let file = fs::File::open(path)
            .map_err(|e| format!("set_immutable: open {}: {e}", path.display()))?;
        let fd = file.as_raw_fd();

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
        // Same as LinuxSystemd — PSI may or may not be available in containers.
        let psi_path = Path::new("/proc/pressure/io");
        if !psi_path.exists() {
            return Ok(None);
        }

        let content =
            fs::read_to_string(psi_path).map_err(|e| format!("read /proc/pressure/io: {e}"))?;

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

    // ── sd_notify (no-op in containers) ────────────────────────────

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

    fn install_service(&self, name: &str, _content: &str) -> Result<(), String> {
        Err(format!(
            "install_service({name}): not supported in container mode — \
             use container runtime (Docker, Podman) to manage services"
        ))
    }

    fn uninstall_service(&self, name: &str) -> Result<(), String> {
        Err(format!(
            "uninstall_service({name}): not supported in container mode"
        ))
    }

    fn enable_service(&self, name: &str) -> Result<(), String> {
        Err(format!(
            "enable_service({name}): not supported in container mode"
        ))
    }

    // ── Service uptime ─────────────────────────────────────────────

    fn service_uptime_secs(&self, service: &str) -> Result<Option<u64>, String> {
        // Find the PID, then compute uptime from /proc/<pid>/stat starttime.
        if let Some(pid) = self.find_service_pid(service)? {
            let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
            match fs::read_to_string(&stat_path) {
                Ok(text) => {
                    // /proc/<pid>/stat format: pid (comm) state ppid ... starttime ...
                    // starttime is field 22 (1-indexed), in clock ticks since boot.
                    // Find the closing ')' to skip the comm field (may contain spaces).
                    if let Some(close_paren) = text.rfind(')') {
                        let after_comm = &text[close_paren + 2..]; // skip ") "
                        let fields: Vec<&str> = after_comm.split_whitespace().collect();
                        // starttime is the 20th field after comm (0-indexed: 19)
                        if fields.len() > 19 {
                            if let Ok(start_ticks) = fields[19].parse::<u64>() {
                                // Get clock ticks per second
                                let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
                                if ticks_per_sec > 0 {
                                    // Get system uptime
                                    let uptime_text = fs::read_to_string("/proc/uptime")
                                        .map_err(|e| format!("read /proc/uptime: {e}"))?;
                                    let sys_uptime_secs: f64 = uptime_text
                                        .split_whitespace()
                                        .next()
                                        .and_then(|s| s.parse().ok())
                                        .ok_or_else(|| "parse /proc/uptime".to_string())?;

                                    let start_secs = start_ticks as f64 / ticks_per_sec as f64;
                                    let uptime = sys_uptime_secs - start_secs;
                                    if uptime > 0.0 {
                                        return Ok(Some(uptime as u64));
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    log::debug!("Failed to read {}: {e}", stat_path.display());
                }
            }
        }

        Ok(None)
    }

    // ── User management ────────────────────────────────────────────

    fn create_system_user(&self, name: &str, _home: &Path) -> Result<(), String> {
        Err(format!(
            "create_system_user({name}): not supported in container mode — \
             add users in the Dockerfile/Containerfile instead"
        ))
    }

    fn user_exists(&self, name: &str) -> Result<bool, String> {
        // We can still check /etc/passwd in containers.
        let output = Command::new("id")
            .arg(name)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("id {name}: {e}"))?;

        Ok(output.success())
    }

    fn delete_user(&self, name: &str) -> Result<(), String> {
        Err(format!(
            "delete_user({name}): not supported in container mode"
        ))
    }

    // ── Disk ───────────────────────────────────────────────────────

    fn disk_usage_mb(&self, path: &Path) -> Result<u64, String> {
        let bytes = Self::dir_size_bytes(path)?;
        Ok(bytes / (1024 * 1024))
    }

    // ── Identification ─────────────────────────────────────────────

    fn platform_name(&self) -> &str {
        "linux-container"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_name() {
        let p = LinuxContainer::new();
        assert_eq!(p.platform_name(), "linux-container");
    }

    #[test]
    fn test_io_pressure_parsing() {
        let p = LinuxContainer::new();
        match p.io_pressure_avg10() {
            Ok(Some(v)) => assert!(v >= 0.0, "avg10 should be non-negative"),
            Ok(None) => {} // PSI not available — acceptable.
            Err(e) => {
                assert!(
                    e.contains("Permission denied") || e.contains("Operation not permitted"),
                    "Unexpected error: {e}"
                );
            }
        }
    }

    #[test]
    fn test_pid_exists() {
        // Our own PID should exist.
        assert!(LinuxContainer::pid_exists(std::process::id()));
        // Bogus PID should not exist.
        assert!(!LinuxContainer::pid_exists(999_999_999));
    }

    #[test]
    fn test_collect_proc_metrics_self() {
        let p = LinuxContainer::new();
        let pid = std::process::id();
        let m = p.collect_proc_metrics(pid).unwrap();
        assert!(m.is_some());
        let m = m.unwrap();
        assert_eq!(m.pid, pid);
        assert!(!m.state.is_empty());
        assert!(m.threads >= 1);
    }

    #[test]
    fn test_collect_proc_metrics_gone() {
        let p = LinuxContainer::new();
        let m = p.collect_proc_metrics(999_999_999).unwrap();
        assert!(m.is_none());
    }

    #[test]
    fn test_service_management_errors() {
        let p = LinuxContainer::new();
        assert!(p.install_service("test", "content").is_err());
        assert!(p.uninstall_service("test").is_err());
        assert!(p.enable_service("test").is_err());
        assert!(p.create_system_user("test", Path::new("/tmp")).is_err());
        assert!(p.delete_user("test").is_err());
    }

    #[test]
    fn test_user_exists_root() {
        let p = LinuxContainer::new();
        assert!(p.user_exists("root").unwrap());
    }

    #[test]
    fn test_disk_usage_nonexistent() {
        let bytes =
            LinuxContainer::dir_size_bytes(Path::new("/tmp/outerclaw_does_not_exist_container"))
                .unwrap();
        assert_eq!(bytes, 0);
    }

    #[test]
    fn test_notify_noop() {
        let p = LinuxContainer::new();
        assert!(p.notify_ready().is_ok());
        assert!(p.notify_watchdog().is_ok());
        assert!(p.notify_stopping().is_ok());
    }

    #[test]
    fn test_reset_failed_noop() {
        let p = LinuxContainer::new();
        assert!(p.reset_failed_service("anything").is_ok());
    }
}
