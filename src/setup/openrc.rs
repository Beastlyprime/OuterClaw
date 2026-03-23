//! OpenRC init script templates for OuterClaw setup.
//!
//! Generates OpenRC-compatible init scripts for systems using OpenRC instead
//! of systemd (e.g., Alpine Linux, Gentoo, Artix Linux).
//!
//! Scripts are installed to `/etc/init.d/` and managed via `rc-update` and
//! `rc-service`.

const BIN: &str = "/var/lib/outerclaw/bin/outerclaw";
const ENV_FILE: &str = "/var/lib/outerclaw/config/outerclaw.env";
const VAULT: &str = "/var/lib/outerclaw";

// ---------------------------------------------------------------------------
// oc-outerclaw — the main watchdog daemon
// ---------------------------------------------------------------------------

/// Generate the OpenRC init script for the OuterClaw watchdog daemon.
///
/// Runs as the `outerclaw` user with `start-stop-daemon`. The daemon
/// is supervised by OpenRC and automatically restarted on failure.
pub fn outerclaw_initd() -> String {
    format!(
        r#"#!/sbin/openrc-run
# OuterClaw watchdog daemon — OpenRC init script
# Installed to /etc/init.d/oc-outerclaw

name="OuterClaw Sentry"
description="OuterClaw watchdog daemon for OpenClaw"

command="{BIN}"
command_args="daemon"
command_user="outerclaw:outerclaw"
command_background="yes"
pidfile="/run/oc-outerclaw.pid"
directory="{VAULT}"

# Read environment variables
start_pre() {{
    if [ -f "{ENV_FILE}" ]; then
        while IFS='=' read -r key value; do
            case "$key" in
                \#*|"") continue ;;
                *) export "$key=$value" ;;
            esac
        done < "{ENV_FILE}"
    fi
    checkpath --directory --owner outerclaw:outerclaw --mode 0700 {VAULT}
    return 0
}}

# Restart policy
respawn="yes"
respawn_delay=10
respawn_max=10
respawn_period=600

# Resource limits
rc_ulimit="-n 1024"

depend() {{
    need net
    after openclaw-gateway
    use logger
}}
"#
    )
}

// ---------------------------------------------------------------------------
// oc-snapshot — snapshot runner (cron-like, 30-min)
// ---------------------------------------------------------------------------

/// Generate the OpenRC init script for snapshots.
///
/// Since OpenRC doesn't have a native timer mechanism, this creates a
/// "cron-like" wrapper. The recommended approach is to pair this with
/// a crontab entry or use `crond` to trigger the service.
pub fn snapshot_initd() -> String {
    format!(
        r#"#!/sbin/openrc-run
# OuterClaw snapshot runner — OpenRC init script
# Run periodically (every 30 min) via cron or manual trigger.

name="OuterClaw Snapshot"
description="OuterClaw SQLite and file snapshot runner"

command="{BIN}"
command_args="snapshot"
command_user="outerclaw:outerclaw"

# Oneshot — runs and exits
start() {{
    if [ -f "{ENV_FILE}" ]; then
        while IFS='=' read -r key value; do
            case "$key" in
                \#*|"") continue ;;
                *) export "$key=$value" ;;
            esac
        done < "{ENV_FILE}"
    fi

    ebegin "Running OuterClaw snapshot"
    start-stop-daemon --start \
        --user outerclaw --group outerclaw \
        --exec {BIN} -- snapshot --sqlite-only
    start-stop-daemon --start \
        --user outerclaw --group outerclaw \
        --exec {BIN} -- snapshot --files-only
    eend $?
}}

stop() {{
    # Oneshot — nothing to stop
    return 0
}}
"#
    )
}

// ---------------------------------------------------------------------------
// oc-healthcheck — health check runner (cron-like, 2-min)
// ---------------------------------------------------------------------------

/// Generate the OpenRC init script for health checks.
pub fn healthcheck_initd() -> String {
    format!(
        r#"#!/sbin/openrc-run
# OuterClaw health check runner — OpenRC init script
# Run periodically (every 2 min) via cron or manual trigger.

name="OuterClaw Health Check"
description="OpenClaw gateway health check"

command="{BIN}"
command_args="healthcheck"
command_user="outerclaw:outerclaw"

start() {{
    if [ -f "{ENV_FILE}" ]; then
        while IFS='=' read -r key value; do
            case "$key" in
                \#*|"") continue ;;
                *) export "$key=$value" ;;
            esac
        done < "{ENV_FILE}"
    fi

    ebegin "Running OpenClaw health check"
    start-stop-daemon --start \
        --user outerclaw --group outerclaw \
        --exec {BIN} -- healthcheck
    eend $?
}}

stop() {{
    return 0
}}
"#
    )
}

// ---------------------------------------------------------------------------
// oc-lkg-promote — LKG promotion runner (cron-like, 2-hour)
// ---------------------------------------------------------------------------

/// Generate the OpenRC init script for LKG promotion.
pub fn lkg_promote_initd() -> String {
    format!(
        r#"#!/sbin/openrc-run
# OuterClaw LKG promotion runner — OpenRC init script
# Run periodically (every 2 hours) via cron or manual trigger.

name="OuterClaw LKG Promotion"
description="Promote latest snapshot to Last Known Good state"

command="{BIN}"
command_args="promote-lkg"
command_user="outerclaw:outerclaw"

start() {{
    if [ -f "{ENV_FILE}" ]; then
        while IFS='=' read -r key value; do
            case "$key" in
                \#*|"") continue ;;
                *) export "$key=$value" ;;
            esac
        done < "{ENV_FILE}"
    fi

    ebegin "Running LKG promotion"
    start-stop-daemon --start \
        --user outerclaw --group outerclaw \
        --exec {BIN} -- promote-lkg
    eend $?
}}

stop() {{
    return 0
}}

depend() {{
    after oc-snapshot
}}
"#
    )
}

// ---------------------------------------------------------------------------
// Crontab entries for periodic tasks
// ---------------------------------------------------------------------------

/// Generate crontab entries for periodic OuterClaw tasks.
///
/// OpenRC doesn't have native timers, so we use cron for scheduling.
/// This should be installed via `crontab -u outerclaw -` or written to
/// `/etc/cron.d/outerclaw`.
pub fn crontab_entries() -> String {
    format!(
        r#"# OuterClaw periodic tasks — install to /etc/cron.d/outerclaw
# Snapshot every 30 minutes
*/30 * * * * outerclaw {BIN} snapshot --sqlite-only && {BIN} snapshot --files-only
# Health check every 2 minutes
*/2 * * * * outerclaw {BIN} healthcheck
# LKG promotion every 2 hours
0 */2 * * * outerclaw {BIN} promote-lkg
"#
    )
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// All OpenRC service names managed by OuterClaw.
pub const SERVICE_NAMES: &[&str] = &[
    "oc-outerclaw",
    "oc-snapshot",
    "oc-healthcheck",
    "oc-lkg-promote",
];

/// Return (service_name, script_content) pairs for all OuterClaw init scripts.
pub fn all_initd_scripts() -> Vec<(&'static str, String)> {
    vec![
        ("oc-outerclaw", outerclaw_initd()),
        ("oc-snapshot", snapshot_initd()),
        ("oc-healthcheck", healthcheck_initd()),
        ("oc-lkg-promote", lkg_promote_initd()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outerclaw_initd_content() {
        let content = outerclaw_initd();
        assert!(content.contains("#!/sbin/openrc-run"));
        assert!(content.contains("/var/lib/outerclaw/bin/outerclaw"));
        assert!(content.contains("command_args=\"daemon\""));
        assert!(content.contains("command_user=\"outerclaw:outerclaw\""));
        assert!(content.contains("command_background=\"yes\""));
        assert!(content.contains("respawn=\"yes\""));
        assert!(content.contains("respawn_delay=10"));
    }

    #[test]
    fn test_snapshot_initd_content() {
        let content = snapshot_initd();
        assert!(content.contains("#!/sbin/openrc-run"));
        assert!(content.contains("snapshot --sqlite-only"));
        assert!(content.contains("snapshot --files-only"));
    }

    #[test]
    fn test_healthcheck_initd_content() {
        let content = healthcheck_initd();
        assert!(content.contains("#!/sbin/openrc-run"));
        assert!(content.contains("healthcheck"));
    }

    #[test]
    fn test_lkg_promote_initd_content() {
        let content = lkg_promote_initd();
        assert!(content.contains("#!/sbin/openrc-run"));
        assert!(content.contains("promote-lkg"));
        assert!(content.contains("after oc-snapshot"));
    }

    #[test]
    fn test_crontab_entries() {
        let content = crontab_entries();
        assert!(content.contains("*/30 * * * *"));
        assert!(content.contains("*/2 * * * *"));
        assert!(content.contains("0 */2 * * *"));
        assert!(content.contains("outerclaw"));
    }

    #[test]
    fn test_all_initd_scripts_count() {
        let scripts = all_initd_scripts();
        assert_eq!(scripts.len(), 4);
        assert_eq!(SERVICE_NAMES.len(), 4);
    }

    #[test]
    fn test_all_scripts_have_shebang() {
        for (name, content) in all_initd_scripts() {
            assert!(
                content.starts_with("#!/sbin/openrc-run"),
                "Missing shebang in {name}"
            );
        }
    }

    #[test]
    fn test_no_source_or_eval() {
        // Security invariant: scripts must never use shell `source` or `eval`
        // on config files to prevent injection.
        // Check each line individually to avoid false positives from words
        // like "Resource" which contain "source" as a substring.
        for (name, content) in all_initd_scripts() {
            for line in content.lines() {
                let trimmed = line.trim();
                assert!(
                    !trimmed.starts_with("source ") && !trimmed.starts_with(". /"),
                    "Found shell source in {name}: {trimmed}"
                );
                assert!(
                    !trimmed.starts_with("eval "),
                    "Found eval in {name}: {trimmed}"
                );
            }
        }
    }
}
