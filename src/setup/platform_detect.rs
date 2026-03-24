//! Platform detection utilities for the setup module.
//!
//! These helpers locate the OpenClaw binary, detect the human user who
//! invoked `sudo`, and check for root privileges.

use std::path::PathBuf;
use std::process::Command;

/// Detect the OpenClaw binary by searching common installation paths.
///
/// Strategy (in order):
/// 1. Ask `agent_user`'s login shell via `sudo -u <user> bash -lc 'command -v openclaw'`
/// 2. Search common npm global directories under the agent's home
/// 3. System PATH via `command -v openclaw`
pub fn detect_openclaw_binary(agent_user: &str) -> Option<PathBuf> {
    // Method 1: Ask the agent user's login shell
    if let Ok(output) = Command::new("sudo")
        .args(["-u", agent_user, "bash", "-lc", "command -v openclaw"])
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                let p = PathBuf::from(&path);
                if p.exists() {
                    log::debug!("Found openclaw via agent user login shell: {}", p.display());
                    return Some(p);
                }
            }
        }
    }

    // Method 2: Check well-known direct paths under agent home
    let agent_home = format!("/home/{agent_user}");
    let direct_paths = [
        format!("{agent_home}/.npm-global/bin/openclaw"),
        format!("{agent_home}/.nvm/versions/node/*/bin/openclaw"),
    ];
    for pattern in &direct_paths {
        // Use glob-style expansion for paths with wildcards
        if pattern.contains('*') {
            if let Ok(output) = Command::new("sh")
                .args(["-c", &format!("ls -1 {pattern} 2>/dev/null | tail -1")])
                .output()
            {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    let p = PathBuf::from(&path);
                    if p.exists() {
                        log::debug!("Found openclaw via glob: {}", p.display());
                        return Some(p);
                    }
                }
            }
        } else {
            let p = PathBuf::from(pattern);
            if p.exists() {
                log::debug!("Found openclaw at direct path: {}", p.display());
                return Some(p);
            }
        }
    }

    // Method 2b: Search common lib paths with find (follows symlinks)
    let search_dirs = ["/usr/local/lib".to_string(), "/usr/lib".to_string()];

    for search_dir in &search_dirs {
        let dir = PathBuf::from(search_dir);
        if !dir.exists() {
            continue;
        }
        if let Ok(output) = Command::new("find")
            .args([
                "-L", // follow symlinks
                dir.to_string_lossy().as_ref(),
                "-name",
                "openclaw",
                "-path",
                "*/bin/openclaw",
            ])
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(last) = stdout.lines().last() {
                    let p = PathBuf::from(last.trim());
                    if p.exists() {
                        log::debug!("Found openclaw via search: {}", p.display());
                        return Some(p);
                    }
                }
            }
        }
    }

    // Method 3: System PATH
    if let Ok(output) = Command::new("command").args(["-v", "openclaw"]).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                let p = PathBuf::from(&path);
                if p.exists() {
                    log::debug!("Found openclaw in system PATH: {}", p.display());
                    return Some(p);
                }
            }
        }
    }

    // Method 3 fallback: which
    if let Ok(output) = Command::new("which").arg("openclaw").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                let p = PathBuf::from(&path);
                if p.exists() {
                    log::debug!("Found openclaw via which: {}", p.display());
                    return Some(p);
                }
            }
        }
    }

    log::warn!("Could not detect openclaw binary");
    None
}

/// Detect the human user who invoked sudo.
///
/// Strategy:
/// 1. Check `SUDO_USER` environment variable
/// 2. Scan `/home/*/` for directories containing `.openclaw/`
pub fn detect_human_user() -> Option<String> {
    // Method 1: SUDO_USER
    if let Ok(user) = std::env::var("SUDO_USER") {
        if !user.is_empty() && user != "root" {
            log::debug!("Detected human user from SUDO_USER: {user}");
            return Some(user);
        }
    }

    // Method 2: Scan /home for .openclaw installations
    let home_dir = PathBuf::from("/home");
    if let Ok(entries) = std::fs::read_dir(&home_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let openclaw_dir = path.join(".openclaw");
                if openclaw_dir.exists() {
                    if let Some(name) = path.file_name() {
                        let user = name.to_string_lossy().to_string();
                        log::debug!("Detected human user from /home scan: {user}");
                        return Some(user);
                    }
                }
            }
        }
    }

    log::warn!("Could not detect human user");
    None
}

/// Check if the current process is running as root.
pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_root_returns_bool() {
        // Just ensure it doesn't panic; actual value depends on test runner
        let _ = is_root();
    }

    #[test]
    fn test_detect_human_user_does_not_panic() {
        // May return None in CI; the important thing is it doesn't crash
        let _ = detect_human_user();
    }
}
