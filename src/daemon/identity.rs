//! Identity file immutability management.
//!
//! Provides both the CLI subcommand handler (`run`) and an in-daemon
//! `IdentityManager` that tracks unlock timeouts for auto-relock.

use crate::cli::{IdentityAction, IdentityArgs};
use crate::platform::Platform;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const IDENTITY_LOCK_UNIT: &str = "oc-identity-lock.service";
const IDENTITY_UNLOCK_UNIT: &str = "oc-identity-unlock.service";

/// Well-known identity files (relative to workspace).
const IDENTITY_FILES: &[&str] = &["SOUL.md", "AGENTS.md", "USER.md"];

/// CLI subcommand: lock or unlock identity files.
///
/// This is called from `main.rs` as `outerclaw identity lock|unlock`.
/// It directly uses the platform trait to set/clear the immutable flag.
pub fn run(args: IdentityArgs, platform: Box<dyn Platform>) -> i32 {
    let openclaw_dir =
        std::env::var("OPENCLAW_DIR").unwrap_or_else(|_| "/home/ocagent/.openclaw".to_string());
    let workspace = PathBuf::from(&openclaw_dir).join("workspace");

    let immutable = matches!(args.action, IdentityAction::Lock);
    let verb = if immutable { "Locking" } else { "Unlocking" };

    let mut errors = 0;
    for name in IDENTITY_FILES {
        let path = workspace.join(name);
        if !path.exists() {
            log::warn!("{name} does not exist, skipping");
            continue;
        }
        log::info!("{verb} {name}");
        if let Err(e) = platform.set_immutable(&path, immutable) {
            log::error!("Failed to set immutable flag on {name}: {e}");
            errors += 1;
        }
    }

    if errors > 0 {
        log::error!("{errors} file(s) failed");
        1
    } else {
        let done = if immutable { "locked" } else { "unlocked" };
        log::info!("Identity files {done} successfully");
        0
    }
}

/// In-daemon identity unlock/relock state tracker.
///
/// The daemon itself runs unprivileged and cannot clear the immutable flag
/// (`FS_IOC_SETFLAGS` requires `CAP_LINUX_IMMUTABLE`). Both `unlock()` and
/// `lock()` therefore trigger the dedicated root-managed oneshot units
/// (`oc-identity-{un}lock.service`) via the no-password sudoers entry,
/// which in turn invoke `outerclaw identity {un}lock` as root.
pub struct IdentityManager {
    /// When identity files were last unlocked, or `None` if locked.
    unlocked_at: Option<Instant>,
    /// How long to wait before auto-relocking.
    unlock_timeout: Duration,
}

impl IdentityManager {
    pub fn new(unlock_timeout_secs: u64) -> Self {
        Self {
            unlocked_at: None,
            unlock_timeout: Duration::from_secs(unlock_timeout_secs),
        }
    }

    /// Unlock identity files via the privileged systemd unit and start the timeout.
    ///
    /// Returns a human-readable result message.
    pub fn unlock(&mut self, source: &str) -> String {
        match run_identity_unit(IDENTITY_UNLOCK_UNIT) {
            Ok(()) => {
                self.unlocked_at = Some(Instant::now());
                let minutes = self.unlock_timeout.as_secs() / 60;
                let msg = format!(
                    "Identity files UNLOCKED ({source}). Auto-relock in {minutes} minutes."
                );
                log::warn!("{msg}");
                msg
            }
            Err(e) => {
                let msg = format!("Unlock failed: {e}");
                log::error!("{msg}");
                msg
            }
        }
    }

    /// Lock identity files via the privileged systemd unit and clear the timeout.
    ///
    /// Returns a human-readable result message.
    pub fn lock(&mut self, source: &str) -> String {
        match run_identity_unit(IDENTITY_LOCK_UNIT) {
            Ok(()) => {
                self.unlocked_at = None;
                let msg = format!("Identity files LOCKED ({source}).");
                log::info!("{msg}");
                msg
            }
            Err(e) => {
                let msg = format!("Lock failed: {e}");
                log::error!("{msg}");
                msg
            }
        }
    }

    /// Check whether the unlock timeout has expired.
    ///
    /// Returns `true` if auto-relock is needed (caller should call `lock()`).
    pub fn check_timeout(&self) -> bool {
        match self.unlocked_at {
            Some(at) => at.elapsed() >= self.unlock_timeout,
            None => false,
        }
    }

    /// Whether identity files are currently unlocked.
    pub fn is_unlocked(&self) -> bool {
        self.unlocked_at.is_some()
    }
}

/// Trigger `sudo systemctl start <unit>` and wait for the oneshot to finish.
fn run_identity_unit(unit: &str) -> Result<(), String> {
    let output = Command::new("sudo")
        .args(["systemctl", "start", unit])
        .output()
        .map_err(|e| format!("cannot invoke sudo systemctl: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let code = output.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        Err(format!("{unit} exited with code {code}"))
    } else {
        Err(format!("{unit} exited with code {code}: {detail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_manager_timeout() {
        let mut mgr = IdentityManager::new(0); // 0-second timeout for testing
        assert!(!mgr.check_timeout());
        assert!(!mgr.is_unlocked());

        // Simulate unlock
        mgr.unlocked_at = Some(Instant::now() - Duration::from_secs(1));
        assert!(mgr.is_unlocked());
        assert!(mgr.check_timeout());
    }

    #[test]
    fn test_identity_manager_not_timed_out() {
        let mgr = IdentityManager::new(600);
        assert!(!mgr.check_timeout());
        assert!(!mgr.is_unlocked());
    }
}
