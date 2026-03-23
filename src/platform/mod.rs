//! Platform abstraction layer for OuterClaw.
//!
//! Provides a trait that encapsulates all OS-specific operations (systemd,
//! launchd, rc.d, /proc, ioctl, sd_notify, user management) so the rest of
//! the codebase stays platform-agnostic.

use std::path::Path;

/// Process metrics snapshot collected from /proc (Linux), `ps` (macOS/FreeBSD),
/// or equivalent.
///
/// Mirrors the Python `ProcessMetrics` class.  All fields default to zero
/// so that partial reads (e.g. /proc/<pid>/io requires elevated perms)
/// degrade gracefully.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProcessMetrics {
    pub pid: u32,
    pub state: String,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub voluntary_ctxt_switches: u64,
    pub nonvoluntary_ctxt_switches: u64,
    pub threads: u32,
    pub rss_bytes: u64,
    pub fd_count: u32,
    pub timestamp: f64,
}

/// Service active state — maps systemd `ActiveState`, launchd state, and
/// rc.d status to a common enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceActive {
    Active,
    Inactive,
    Failed,
    Activating,
    Unknown,
}

impl std::fmt::Display for ServiceActive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Inactive => write!(f, "inactive"),
            Self::Failed => write!(f, "failed"),
            Self::Activating => write!(f, "activating"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Platform trait — abstracts every OS-level operation that OuterClaw needs.
///
/// All methods return `Result<_, String>` using human-readable error messages
/// (these propagate to Telegram alerts and log files, so clarity matters).
pub trait Platform: Send + Sync {
    // ── Process / Service discovery ────────────────────────────────
    /// Return the main PID of a service, or `None` if it has no running
    /// main process.
    fn find_service_pid(&self, service: &str) -> Result<Option<u32>, String>;

    /// Collect process metrics for a running process.  Returns `None` if the
    /// process no longer exists (race between PID lookup and read).
    fn collect_proc_metrics(&self, pid: u32) -> Result<Option<ProcessMetrics>, String>;

    // ── Service lifecycle ──────────────────────────────────────────
    fn service_state(&self, service: &str) -> Result<ServiceActive, String>;
    fn restart_service(&self, service: &str) -> Result<(), String>;
    fn stop_service(&self, service: &str) -> Result<(), String>;
    fn kill_service(&self, service: &str) -> Result<(), String>;
    fn reset_failed_service(&self, service: &str) -> Result<(), String>;

    // ── Identity / immutability ────────────────────────────────────
    /// Set or clear the immutable flag on a file.
    fn set_immutable(&self, path: &Path, immutable: bool) -> Result<(), String>;

    // ── I/O pressure (PSI) ─────────────────────────────────────────
    /// Read the `avg10` I/O pressure.
    /// Returns `None` if PSI is unavailable on this platform/kernel.
    fn io_pressure_avg10(&self) -> Result<Option<f32>, String>;

    // ── sd_notify (systemd watchdog protocol) ──────────────────────
    fn notify_ready(&self) -> Result<(), String>;
    fn notify_watchdog(&self) -> Result<(), String>;
    fn notify_stopping(&self) -> Result<(), String>;

    // ── Service unit management ────────────────────────────────────
    /// Write a service definition (systemd unit, launchd plist, rc.d script)
    /// and reload the init system.
    fn install_service(&self, name: &str, content: &str) -> Result<(), String>;
    /// Stop + disable + remove a service definition and reload.
    fn uninstall_service(&self, name: &str) -> Result<(), String>;
    /// Enable (and optionally start) a service.
    fn enable_service(&self, name: &str) -> Result<(), String>;

    // ── Service uptime ─────────────────────────────────────────────
    /// Compute how long a service has been in the active/running state.
    fn service_uptime_secs(&self, service: &str) -> Result<Option<u64>, String>;

    // ── User management ────────────────────────────────────────────
    fn create_system_user(&self, name: &str, home: &Path) -> Result<(), String>;
    fn user_exists(&self, name: &str) -> Result<bool, String>;
    fn delete_user(&self, name: &str) -> Result<(), String>;

    // ── Disk ───────────────────────────────────────────────────────
    /// Walk a directory tree and return total size in MiB.
    fn disk_usage_mb(&self, path: &Path) -> Result<u64, String>;

    // ── Identification ─────────────────────────────────────────────
    fn platform_name(&self) -> &str;
}

// ── Module declarations ────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "freebsd")]
pub mod freebsd;

#[cfg(target_os = "linux")]
pub mod container;

// ── Platform auto-detection ────────────────────────────────────────────

/// Auto-detect the current platform and return a boxed implementation.
pub fn detect() -> Box<dyn Platform> {
    #[cfg(target_os = "linux")]
    {
        if Path::new("/run/systemd/system").exists() {
            log::info!("Platform: Linux with systemd");
            return Box::new(linux::LinuxSystemd::new());
        }
        if Path::new("/run/openrc").exists() || Path::new("/sbin/openrc").exists() {
            log::info!("Platform: Linux with OpenRC");
            // OpenRC uses a subset of LinuxSystemd functionality — service
            // lifecycle methods that shell out to systemctl will fail
            // gracefully at call-time.
            return Box::new(linux::LinuxSystemd::new());
        }
        if is_container() {
            log::info!("Platform: Linux container (no init)");
            return Box::new(container::LinuxContainer::new());
        }
        log::warn!("Platform: Linux (generic, limited functionality)");
        Box::new(linux::LinuxSystemd::new())
    }

    #[cfg(target_os = "macos")]
    {
        log::info!("Platform: macOS with launchd");
        return Box::new(macos::MacOSLaunchd::new());
    }

    #[cfg(target_os = "freebsd")]
    {
        log::info!("Platform: FreeBSD");
        return Box::new(freebsd::FreeBSDRc::new());
    }

    // Fallback for unsupported targets — compile-time gate.
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
    {
        compile_error!("Unsupported target OS. OuterClaw supports Linux, macOS, and FreeBSD.");
    }
}

/// Detect whether we are running inside a container.
///
/// Checks for Docker, Podman, LXC, and Kubernetes markers.
#[cfg(target_os = "linux")]
fn is_container() -> bool {
    Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
        || std::fs::read_to_string("/proc/1/cgroup")
            .map(|c| c.contains("docker") || c.contains("lxc") || c.contains("kubepods"))
            .unwrap_or(false)
}
