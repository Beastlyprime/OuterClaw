//! User management helpers for OuterClaw setup.
//!
//! Wraps standard system commands (`useradd`, `userdel`, `id`) for
//! creating and removing service users.

use std::path::Path;
use std::process::Command;

/// Create a system user (nologin shell, with home directory).
///
/// Used for the `outerclaw` service user.  If the user already exists,
/// this is a no-op (idempotent).
pub fn ensure_system_user(name: &str, home: &Path, shell: &str) -> Result<(), String> {
    if user_exists(name) {
        log::info!("System user '{name}' already exists");
        return Ok(());
    }

    log::info!(
        "Creating system user '{name}' (home={}, shell={shell})",
        home.display()
    );

    let output = Command::new("useradd")
        .args([
            "--system",
            "--shell",
            shell,
            "--home-dir",
            &home.to_string_lossy(),
            "--create-home",
            "--comment",
            "OuterClaw Service",
            name,
        ])
        .output()
        .map_err(|e| format!("Failed to run useradd: {e}"))?;

    if !output.status.success() {
        // Exit code 9 = user already exists
        if output.status.code() == Some(9) {
            log::info!("User '{name}' already exists (race condition)");
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "useradd '{name}' failed (exit {}): {stderr}",
            output.status.code().unwrap_or(-1)
        ));
    }

    log::info!("System user '{name}' created");
    Ok(())
}

/// Create a login user (with bash shell, home directory).
///
/// Used for the `ocagent` application user in full mode.  If the user
/// already exists, this is a no-op (idempotent).
pub fn ensure_login_user(name: &str) -> Result<(), String> {
    if user_exists(name) {
        log::info!("Login user '{name}' already exists");
        return Ok(());
    }

    log::info!("Creating login user '{name}'");

    let output = Command::new("useradd")
        .args([
            "-m",
            "-s",
            "/bin/bash",
            "--comment",
            "OpenClaw Agent (no sudo)",
            name,
        ])
        .output()
        .map_err(|e| format!("Failed to run useradd: {e}"))?;

    if !output.status.success() {
        // Exit code 9 = user already exists
        if output.status.code() == Some(9) {
            log::info!("User '{name}' already exists (race condition)");
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "useradd '{name}' failed (exit {}): {stderr}",
            output.status.code().unwrap_or(-1)
        ));
    }

    log::info!("Login user '{name}' created");
    Ok(())
}

/// Check whether a system user exists via the `id` command.
pub fn user_exists(name: &str) -> bool {
    Command::new("id")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Remove a system user via `userdel`.
///
/// If the user does not exist, this is a no-op (idempotent).
pub fn remove_user(name: &str) -> Result<(), String> {
    if !user_exists(name) {
        log::info!("User '{name}' does not exist (nothing to remove)");
        return Ok(());
    }

    log::info!("Removing user '{name}'");

    let output = Command::new("userdel")
        .arg(name)
        .output()
        .map_err(|e| format!("Failed to run userdel: {e}"))?;

    if !output.status.success() {
        // Exit code 6 = user doesn't exist (race condition)
        if output.status.code() == Some(6) {
            log::info!("User '{name}' already gone");
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "userdel '{name}' failed (exit {}): {stderr}",
            output.status.code().unwrap_or(-1)
        ));
    }

    // Also try to remove the group (may or may not exist)
    let _ = Command::new("groupdel")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    log::info!("User '{name}' removed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_exists_root() {
        assert!(user_exists("root"));
    }

    #[test]
    fn test_user_exists_bogus() {
        assert!(!user_exists("outerclaw_test_bogus_user_xyzzy_42"));
    }
}
