//! Runit run script templates for OuterClaw setup.
//!
//! Generates runit `run` files for systems using runit as the init system
//! (e.g., Void Linux, or as a secondary supervisor on other distributions).
//!
//! Runit service directories are typically at `/etc/sv/<service>/` with a
//! `run` executable script inside. Services are enabled by symlinking to
//! `/var/service/` (or `/service/`).

const BIN: &str = "/var/lib/outerclaw/bin/outerclaw";
const ENV_FILE: &str = "/var/lib/outerclaw/config/outerclaw.env";
const VAULT: &str = "/var/lib/outerclaw";

// ---------------------------------------------------------------------------
// oc-outerclaw — the main watchdog daemon
// ---------------------------------------------------------------------------

/// Generate the runit `run` script for the OuterClaw watchdog daemon.
///
/// Runs as the `outerclaw` user via `chpst`. Runit automatically restarts
/// the service if it exits.
pub fn outerclaw_run() -> String {
    format!(
        r#"#!/bin/sh
# OuterClaw watchdog daemon — runit run script
# Installed to /etc/sv/oc-outerclaw/run
exec 2>&1

# Load environment variables safely (no source/eval)
if [ -f "{ENV_FILE}" ]; then
    while IFS='=' read -r key value; do
        case "$key" in
            \#*|"") continue ;;
            *) export "$key=$value" ;;
        esac
    done < "{ENV_FILE}"
fi

cd {VAULT}
exec chpst -u outerclaw:outerclaw {BIN} daemon
"#
    )
}

/// Generate the runit `log/run` script for the OuterClaw daemon.
///
/// Uses `svlogd` to capture stdout/stderr to a dedicated log directory.
pub fn outerclaw_log_run() -> String {
    format!(
        r#"#!/bin/sh
# OuterClaw daemon log — runit log/run script
exec svlogd -tt {VAULT}/audit/runit-daemon/
"#
    )
}

// ---------------------------------------------------------------------------
// oc-snapshot — snapshot runner (periodic via cron or runit timer)
// ---------------------------------------------------------------------------

/// Generate the runit `run` script for the snapshot service.
///
/// Since runit doesn't have native timers, this script sleeps for 30 minutes
/// between runs. The `pause` approach (sleep + exec) provides a simple
/// periodic trigger without requiring cron.
pub fn snapshot_run() -> String {
    format!(
        r#"#!/bin/sh
# OuterClaw snapshot runner — runit run script
# Runs every 30 minutes (1800 seconds)
exec 2>&1

# Load environment variables safely
if [ -f "{ENV_FILE}" ]; then
    while IFS='=' read -r key value; do
        case "$key" in
            \#*|"") continue ;;
            *) export "$key=$value" ;;
        esac
    done < "{ENV_FILE}"
fi

cd {VAULT}

# Run snapshot, then sleep for 30 minutes
chpst -u outerclaw:outerclaw {BIN} snapshot --sqlite-only
chpst -u outerclaw:outerclaw {BIN} snapshot --files-only

# Sleep until next interval — runit will restart this script after it exits
exec sleep 1800
"#
    )
}

// ---------------------------------------------------------------------------
// oc-healthcheck — health check runner (periodic)
// ---------------------------------------------------------------------------

/// Generate the runit `run` script for the health check service.
///
/// Runs every 2 minutes (120 seconds).
pub fn healthcheck_run() -> String {
    format!(
        r#"#!/bin/sh
# OuterClaw health check runner — runit run script
# Runs every 2 minutes (120 seconds)
exec 2>&1

# Load environment variables safely
if [ -f "{ENV_FILE}" ]; then
    while IFS='=' read -r key value; do
        case "$key" in
            \#*|"") continue ;;
            *) export "$key=$value" ;;
        esac
    done < "{ENV_FILE}"
fi

cd {VAULT}
chpst -u outerclaw:outerclaw {BIN} healthcheck
exec sleep 120
"#
    )
}

// ---------------------------------------------------------------------------
// oc-lkg-promote — LKG promotion runner (periodic)
// ---------------------------------------------------------------------------

/// Generate the runit `run` script for the LKG promotion service.
///
/// Runs every 2 hours (7200 seconds).
pub fn lkg_promote_run() -> String {
    format!(
        r#"#!/bin/sh
# OuterClaw LKG promotion runner — runit run script
# Runs every 2 hours (7200 seconds)
exec 2>&1

# Load environment variables safely
if [ -f "{ENV_FILE}" ]; then
    while IFS='=' read -r key value; do
        case "$key" in
            \#*|"") continue ;;
            *) export "$key=$value" ;;
        esac
    done < "{ENV_FILE}"
fi

cd {VAULT}
chpst -u outerclaw:outerclaw {BIN} promote-lkg
exec sleep 7200
"#
    )
}

// ---------------------------------------------------------------------------
// Metadata and helpers
// ---------------------------------------------------------------------------

/// All runit service names managed by OuterClaw.
pub const SERVICE_NAMES: &[&str] = &[
    "oc-outerclaw",
    "oc-snapshot",
    "oc-healthcheck",
    "oc-lkg-promote",
];

/// A runit service definition with its directory structure.
pub struct RunitService {
    /// Service name (directory name under /etc/sv/).
    pub name: &'static str,
    /// Content of the `run` script.
    pub run_script: String,
    /// Optional content of the `log/run` script.
    pub log_run_script: Option<String>,
}

/// Return all OuterClaw runit service definitions.
pub fn all_services() -> Vec<RunitService> {
    vec![
        RunitService {
            name: "oc-outerclaw",
            run_script: outerclaw_run(),
            log_run_script: Some(outerclaw_log_run()),
        },
        RunitService {
            name: "oc-snapshot",
            run_script: snapshot_run(),
            log_run_script: None,
        },
        RunitService {
            name: "oc-healthcheck",
            run_script: healthcheck_run(),
            log_run_script: None,
        },
        RunitService {
            name: "oc-lkg-promote",
            run_script: lkg_promote_run(),
            log_run_script: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outerclaw_run_content() {
        let content = outerclaw_run();
        assert!(content.starts_with("#!/bin/sh"));
        assert!(content.contains("/var/lib/outerclaw/bin/outerclaw"));
        assert!(content.contains("exec chpst -u outerclaw:outerclaw"));
        assert!(content.contains("daemon"));
    }

    #[test]
    fn test_outerclaw_log_run_content() {
        let content = outerclaw_log_run();
        assert!(content.starts_with("#!/bin/sh"));
        assert!(content.contains("svlogd"));
        assert!(content.contains("runit-daemon"));
    }

    #[test]
    fn test_snapshot_run_content() {
        let content = snapshot_run();
        assert!(content.starts_with("#!/bin/sh"));
        assert!(content.contains("snapshot --sqlite-only"));
        assert!(content.contains("snapshot --files-only"));
        assert!(content.contains("sleep 1800"));
    }

    #[test]
    fn test_healthcheck_run_content() {
        let content = healthcheck_run();
        assert!(content.starts_with("#!/bin/sh"));
        assert!(content.contains("healthcheck"));
        assert!(content.contains("sleep 120"));
    }

    #[test]
    fn test_lkg_promote_run_content() {
        let content = lkg_promote_run();
        assert!(content.starts_with("#!/bin/sh"));
        assert!(content.contains("promote-lkg"));
        assert!(content.contains("sleep 7200"));
    }

    #[test]
    fn test_all_services_count() {
        let services = all_services();
        assert_eq!(services.len(), 4);
        assert_eq!(SERVICE_NAMES.len(), 4);
    }

    #[test]
    fn test_only_daemon_has_log_run() {
        let services = all_services();
        for svc in &services {
            if svc.name == "oc-outerclaw" {
                assert!(svc.log_run_script.is_some(), "Daemon should have log/run");
            } else {
                assert!(
                    svc.log_run_script.is_none(),
                    "{} should not have log/run",
                    svc.name
                );
            }
        }
    }

    #[test]
    fn test_no_source_or_eval() {
        // Security invariant: scripts must never use `source` or `eval`
        // on config files to prevent injection.
        for svc in all_services() {
            assert!(
                !svc.run_script.contains("source ")
                    && !svc.run_script.contains(". /var/lib")
                    && !svc.run_script.contains(". \""),
                "Found 'source' or '. /' in {} — use read loop instead",
                svc.name
            );
            assert!(
                !svc.run_script.contains("eval "),
                "Found 'eval' in {} — prohibited for security",
                svc.name
            );
        }
    }

    #[test]
    fn test_all_scripts_have_shebang() {
        for svc in all_services() {
            assert!(
                svc.run_script.starts_with("#!/bin/sh"),
                "Missing shebang in {}",
                svc.name
            );
            if let Some(ref log_run) = svc.log_run_script {
                assert!(
                    log_run.starts_with("#!/bin/sh"),
                    "Missing shebang in {}/log/run",
                    svc.name
                );
            }
        }
    }

    #[test]
    fn test_all_scripts_use_chpst() {
        // All run scripts (except log) should use chpst to drop privileges.
        for svc in all_services() {
            assert!(
                svc.run_script.contains("chpst -u outerclaw"),
                "{} should use chpst to run as outerclaw",
                svc.name
            );
        }
    }
}
