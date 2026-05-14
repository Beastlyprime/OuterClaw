//! Systemd unit file templates and config generators for OuterClaw setup.
//!
//! All unit-generator functions accept `&Config` so paths/users derive from the
//! deployment's actual layout instead of compile-time constants. This is what
//! makes the templates mode-agnostic (sudo vs user-mode in later PRs).

use crate::config::Config;

// ---------------------------------------------------------------------------
// oc-outerclaw.service — the main watchdog daemon
// ---------------------------------------------------------------------------

pub fn outerclaw_service(cfg: &Config) -> String {
    let bin = cfg.bin_path().display().to_string();
    let env_file = cfg.env_file_path().display().to_string();
    let vault = cfg.vault_dir.display().to_string();
    let user = &cfg.watchdog_user;

    format!(
        r#"# /etc/systemd/system/oc-outerclaw.service
# OuterClaw daemon — runs as watchdog user
[Unit]
Description=OuterClaw Sentry
After=openclaw-gateway.service
Wants=openclaw-gateway.service
StartLimitIntervalSec=600
StartLimitBurst=10

[Service]
Type=notify
User={user}
Group={user}
WorkingDirectory={vault}

ExecStart={bin} daemon
Restart=always
RestartSec=10

# Watchdog: systemd kills OuterClaw if it stops reporting within 120s
WatchdogSec=120

# Self-protection: make OuterClaw hard to OOM-kill
OOMScoreAdjust=-500
OOMPolicy=continue

# Lightweight — strict limits
MemoryMax=256M
MemoryHigh=192M
CPUQuota=10%
TasksMax=32

# Security hardening
NoNewPrivileges=no
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=read-only
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
RestrictNamespaces=yes
LockPersonality=yes
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
ReadWritePaths={vault}

# Config
EnvironmentFile={env_file}

[Install]
WantedBy=multi-user.target
"#
    )
}

// ---------------------------------------------------------------------------
// openclaw-gateway.service — the OpenClaw gateway
// ---------------------------------------------------------------------------

pub fn gateway_service(cfg: &Config) -> String {
    let bin = cfg.bin_path().display().to_string();
    let vault = cfg.vault_dir.display().to_string();
    let agent_user = &cfg.agent_user;
    let agent_home = cfg.agent_home.display().to_string();
    let openclaw_dir = cfg.openclaw_dir.display().to_string();

    format!(
        r#"# /etc/systemd/system/openclaw-gateway.service
# SYSTEM-level service (root-managed). Agent cannot modify or stop this.
[Unit]
Description=OpenClaw Gateway
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=300
StartLimitBurst=5

[Service]
Type=simple
User={agent_user}
Group={agent_user}
WorkingDirectory={agent_home}

# Validate data integrity before starting
ExecStartPre={bin} pre-start-check

# Gateway start wrapper (auto-generated)
ExecStart={vault}/bin/start-gateway.sh

# Restart policy
Restart=always
RestartSec=5

# Resource containment
MemoryMax=8G
MemoryHigh=6G
CPUQuota=400%
TasksMax=512

# OOM handling
OOMScoreAdjust=100
OOMPolicy=stop

# Post-crash forensics (runs as root for /proc access)
ExecStopPost=+{bin} postmortem openclaw-gateway

# Security hardening
NoNewPrivileges=yes
PrivateTmp=yes

# Environment
Environment=HOME={agent_home}
Environment="PATH=/usr/local/bin:/usr/bin:/bin"
Environment=OPENCLAW_GATEWAY_PORT=18789
EnvironmentFile=-{openclaw_dir}/.env
Environment="OPENCLAW_SYSTEMD_UNIT=openclaw-gateway.service"
Environment=OPENCLAW_SERVICE_MARKER=openclaw
Environment=OPENCLAW_SERVICE_KIND=gateway

[Install]
WantedBy=multi-user.target
"#
    )
}

// ---------------------------------------------------------------------------
// oc-snapshot.timer / oc-snapshot.service
// ---------------------------------------------------------------------------

pub fn snapshot_timer() -> &'static str {
    r#"# /etc/systemd/system/oc-snapshot.timer
[Unit]
Description=OuterClaw Snapshot (every 30 min)

[Timer]
OnBootSec=120
OnUnitActiveSec=1800
AccuracySec=30
Persistent=true

[Install]
WantedBy=timers.target
"#
}

pub fn snapshot_service(cfg: &Config) -> String {
    let bin = cfg.bin_path().display().to_string();
    let env_file = cfg.env_file_path().display().to_string();
    let user = &cfg.watchdog_user;
    let openclaw_dir = cfg.openclaw_dir.display().to_string();

    format!(
        r#"# /etc/systemd/system/oc-snapshot.service
[Unit]
Description=OuterClaw Snapshot Runner

[Service]
Type=oneshot
User={user}
Group={user}

# Defend against ACL mask drift before each snapshot. POSIX ACL masks
# get recomputed when group bits change (e.g. chmod g-rwx by the agent
# user or its package manager), which collapses the watchdog's effective
# read permission to ---. Re-applying with explicit m::rx is idempotent.
ExecStartPre=+/bin/bash -c 'D=$(grep "^OPENCLAW_DIR=" {env_file} 2>/dev/null | cut -d= -f2); D="${{D:-{openclaw_dir}}}"; for P in "$D" "$D/memory" "$D/tasks" "$D/workspace" "$D/logs"; do [ -d "$P" ] && setfacl -m u:{user}:rx,m::rx "$P" 2>/dev/null; done; shopt -s nullglob; for F in "$D/memory/main.sqlite"* "$D/tasks/runs.sqlite"* "$D/openclaw.json" "$D/exec-approvals.json"; do [ -f "$F" ] && setfacl -m u:{user}:r,m::r "$F" 2>/dev/null; done; true'

ExecStart={bin} snapshot --sqlite-only
ExecStart={bin} snapshot --files-only

MemoryMax=512M
IOWeight=50
Nice=15
"#
    )
}

// ---------------------------------------------------------------------------
// oc-healthcheck.timer / oc-healthcheck.service
// ---------------------------------------------------------------------------

pub fn healthcheck_timer() -> &'static str {
    r#"# /etc/systemd/system/oc-healthcheck.timer
[Unit]
Description=OpenClaw Health Check (every 2 min)

[Timer]
OnBootSec=60
OnUnitActiveSec=120
AccuracySec=10

[Install]
WantedBy=timers.target
"#
}

pub fn healthcheck_service(cfg: &Config) -> String {
    let bin = cfg.bin_path().display().to_string();
    let user = &cfg.watchdog_user;
    format!(
        r#"# /etc/systemd/system/oc-healthcheck.service
[Unit]
Description=OpenClaw Health Check Runner

[Service]
Type=oneshot
User={user}
Group={user}
ExecStart={bin} healthcheck
"#
    )
}

// ---------------------------------------------------------------------------
// oc-lkg-promote.timer / oc-lkg-promote.service
// ---------------------------------------------------------------------------

pub fn lkg_promote_timer() -> &'static str {
    r#"# /etc/systemd/system/oc-lkg-promote.timer
[Unit]
Description=OpenClaw LKG Promotion Timer

[Timer]
OnBootSec=3600
OnUnitActiveSec=7200
AccuracySec=60
Persistent=true

[Install]
WantedBy=timers.target
"#
}

pub fn lkg_promote_service(cfg: &Config) -> String {
    let bin = cfg.bin_path().display().to_string();
    let user = &cfg.watchdog_user;
    format!(
        r#"# /etc/systemd/system/oc-lkg-promote.service
[Unit]
Description=OpenClaw LKG Promotion
After=oc-snapshot.service

[Service]
Type=oneshot
User={user}
Group={user}
ExecStart={bin} promote-lkg
MemoryMax=256M
Nice=15
"#
    )
}

// ---------------------------------------------------------------------------
// oc-cloud-sync.timer / oc-cloud-sync.service
// ---------------------------------------------------------------------------

pub fn cloud_sync_timer() -> &'static str {
    r#"# /etc/systemd/system/oc-cloud-sync.timer
[Unit]
Description=OuterClaw cloud backup sync timer

[Timer]
OnBootSec=300
OnUnitActiveSec=7200
AccuracySec=60
Persistent=true

[Install]
WantedBy=timers.target
"#
}

pub fn cloud_sync_service(cfg: &Config) -> String {
    let bin = cfg.bin_path().display().to_string();
    let user = &cfg.watchdog_user;
    let rclone_conf = cfg.rclone_config_path().display().to_string();
    format!(
        r#"# /etc/systemd/system/oc-cloud-sync.service
[Unit]
Description=OuterClaw cloud backup sync
After=oc-snapshot.service oc-lkg-promote.service

[Service]
Type=oneshot
User={user}
Group={user}
ExecStart={bin} cloud sync
Environment=RCLONE_CONFIG={rclone_conf}

Nice=15
IOWeight=50
MemoryMax=256M
"#
    )
}

// ---------------------------------------------------------------------------
// oc-identity-lock.service / oc-identity-unlock.service
// ---------------------------------------------------------------------------

pub fn identity_lock_service(cfg: &Config) -> String {
    let bin = cfg.bin_path().display().to_string();
    format!(
        r#"# /etc/systemd/system/oc-identity-lock.service
[Unit]
Description=OuterClaw Identity Lock

[Service]
Type=oneshot
ExecStart={bin} identity lock
RemainAfterExit=no
"#
    )
}

pub fn identity_unlock_service(cfg: &Config) -> String {
    let bin = cfg.bin_path().display().to_string();
    format!(
        r#"# /etc/systemd/system/oc-identity-unlock.service
[Unit]
Description=OuterClaw Identity Unlock

[Service]
Type=oneshot
ExecStart={bin} identity unlock
RemainAfterExit=no
"#
    )
}

// ---------------------------------------------------------------------------
// logrotate config
// ---------------------------------------------------------------------------

pub fn logrotate_config(cfg: &Config) -> String {
    let audit = cfg.audit_dir().display().to_string();
    let user = &cfg.watchdog_user;
    format!(
        r#"{audit}/*.log {{
    daily
    rotate 30
    compress
    delaycompress
    missingok
    notifempty
    create 0600 {user} {user}
}}
"#
    )
}

// ---------------------------------------------------------------------------
// sudoers config
// ---------------------------------------------------------------------------

pub fn sudoers_config(cfg: &Config) -> String {
    let bin = cfg.bin_path().display().to_string();
    let user = &cfg.watchdog_user;
    format!(
        r#"# OuterClaw: narrowly scoped privileged operations
{user} ALL=(root) NOPASSWD: /usr/bin/systemctl restart openclaw-gateway.service
{user} ALL=(root) NOPASSWD: /usr/bin/systemctl stop openclaw-gateway.service
{user} ALL=(root) NOPASSWD: /usr/bin/systemctl reset-failed openclaw-gateway.service
{user} ALL=(root) NOPASSWD: /usr/bin/systemctl kill --signal=SIGKILL openclaw-gateway.service
{user} ALL=(root) NOPASSWD: /usr/bin/systemctl start oc-identity-lock.service
{user} ALL=(root) NOPASSWD: /usr/bin/systemctl start oc-identity-unlock.service
{user} ALL=(root) NOPASSWD: {bin} auto-recover
"#
    )
}

// ---------------------------------------------------------------------------
// All unit names — used by install/uninstall to iterate
// ---------------------------------------------------------------------------

/// All systemd service unit names managed by OuterClaw.
pub const SERVICE_UNITS: &[&str] = &[
    "oc-outerclaw.service",
    "oc-snapshot.service",
    "oc-healthcheck.service",
    "oc-lkg-promote.service",
    "oc-cloud-sync.service",
    "oc-identity-lock.service",
    "oc-identity-unlock.service",
];

/// All systemd timer unit names managed by OuterClaw.
pub const TIMER_UNITS: &[&str] = &[
    "oc-snapshot.timer",
    "oc-healthcheck.timer",
    "oc-lkg-promote.timer",
    "oc-cloud-sync.timer",
];

/// Core timers that are always enabled (cloud-sync is conditional).
pub const CORE_TIMERS: &[&str] = &[
    "oc-snapshot.timer",
    "oc-healthcheck.timer",
    "oc-lkg-promote.timer",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;
    use std::path::PathBuf;

    fn test_cfg() -> Config {
        Config {
            gateway_port: 18789,
            gateway_service: "openclaw-gateway.service".into(),
            health_timeout: 5,
            health_url: "".into(),
            sessions_url: "".into(),
            tick_interval: 1,
            collect_interval: 30,
            hang_warn_secs: 120,
            hang_crit_secs: 300,
            io_delta_threshold: 0,
            ctx_switch_threshold: 0,
            restart_settle_wait: 90,
            recovery_cooldown: 1800,
            kill_graceful_timeout: 15,
            identity_unlock_timeout: 600,
            mode: Mode::Sudo,
            openclaw_dir: PathBuf::from("/home/ocagent/.openclaw"),
            vault_dir: PathBuf::from("/var/lib/outerclaw"),
            agent_user: "ocagent".into(),
            agent_home: PathBuf::from("/home/ocagent"),
            watchdog_user: "outerclaw".into(),
            tg_token: "".into(),
            tg_chat: "".into(),
            tg_is_dedicated: false,
            max_vault_mb: 2048,
            io_pressure_threshold: 25.0,
            cloud_enabled: false,
            cloud_remote: "".into(),
            cloud_bandwidth: 0,
            max_response_bytes: 0,
        }
    }

    #[test]
    fn test_outerclaw_service_contains_binary_path() {
        let content = outerclaw_service(&test_cfg());
        assert!(content.contains("/var/lib/outerclaw/bin/outerclaw daemon"));
        assert!(content.contains("Type=notify"));
        assert!(content.contains("WatchdogSec=120"));
        assert!(content.contains("OOMScoreAdjust=-500"));
        assert!(content.contains("MemoryMax=256M"));
        assert!(content.contains("ProtectSystem=strict"));
        assert!(content.contains("User=outerclaw"));
        assert!(content.contains("ReadWritePaths=/var/lib/outerclaw"));
    }

    #[test]
    fn test_gateway_service_uses_agent_user() {
        let mut cfg = test_cfg();
        cfg.agent_user = "testuser".into();
        cfg.agent_home = PathBuf::from("/home/testuser");
        cfg.openclaw_dir = PathBuf::from("/home/testuser/.openclaw");
        let content = gateway_service(&cfg);
        assert!(content.contains("User=testuser"));
        assert!(content.contains("Group=testuser"));
        assert!(content.contains("pre-start-check"));
        assert!(content.contains("/var/lib/outerclaw/bin/outerclaw"));
        assert!(content.contains("WorkingDirectory=/home/testuser"));
    }

    #[test]
    fn test_snapshot_service_uses_binary() {
        let content = snapshot_service(&test_cfg());
        assert!(content.contains("/var/lib/outerclaw/bin/outerclaw snapshot --sqlite-only"));
        assert!(content.contains("/var/lib/outerclaw/bin/outerclaw snapshot --files-only"));
        assert!(content.contains("u:outerclaw:rx,m::rx"));
    }

    #[test]
    fn test_sudoers_uses_binary_for_auto_recover() {
        let content = sudoers_config(&test_cfg());
        assert!(content.contains("/var/lib/outerclaw/bin/outerclaw auto-recover"));
        assert!(content.contains("outerclaw ALL=(root)"));
        assert!(!content.contains("auto-recover.sh"));
    }

    #[test]
    fn test_identity_services_use_binary() {
        let lock = identity_lock_service(&test_cfg());
        let unlock = identity_unlock_service(&test_cfg());
        assert!(lock.contains("/var/lib/outerclaw/bin/outerclaw identity lock"));
        assert!(unlock.contains("/var/lib/outerclaw/bin/outerclaw identity unlock"));
    }

    #[test]
    fn test_healthcheck_service_uses_binary() {
        let content = healthcheck_service(&test_cfg());
        assert!(content.contains("/var/lib/outerclaw/bin/outerclaw healthcheck"));
    }

    #[test]
    fn test_lkg_promote_service_uses_binary() {
        let content = lkg_promote_service(&test_cfg());
        assert!(content.contains("/var/lib/outerclaw/bin/outerclaw promote-lkg"));
    }

    #[test]
    fn test_cloud_sync_service_uses_binary() {
        let content = cloud_sync_service(&test_cfg());
        assert!(content.contains("/var/lib/outerclaw/bin/outerclaw cloud sync"));
        assert!(content.contains("RCLONE_CONFIG=/var/lib/outerclaw/config/rclone.conf"));
    }

    #[test]
    fn test_logrotate_config_content() {
        let content = logrotate_config(&test_cfg());
        assert!(content.contains("/var/lib/outerclaw/audit/*.log"));
        assert!(content.contains("daily"));
        assert!(content.contains("rotate 30"));
        assert!(content.contains("create 0600 outerclaw outerclaw"));
    }

    #[test]
    fn test_template_respects_custom_vault() {
        let mut cfg = test_cfg();
        cfg.vault_dir = PathBuf::from("/opt/elsewhere");
        let content = outerclaw_service(&cfg);
        assert!(content.contains("/opt/elsewhere/bin/outerclaw daemon"));
        assert!(content.contains("ReadWritePaths=/opt/elsewhere"));
        assert!(!content.contains("/var/lib/outerclaw"));
    }

    #[test]
    fn test_template_respects_custom_watchdog_user() {
        let mut cfg = test_cfg();
        cfg.watchdog_user = "guardian".into();
        let content = outerclaw_service(&cfg);
        assert!(content.contains("User=guardian"));
        let sudoers = sudoers_config(&cfg);
        assert!(sudoers.contains("guardian ALL=(root)"));
    }

    #[test]
    fn test_all_units_listed() {
        assert_eq!(TIMER_UNITS.len(), 4);
        assert_eq!(SERVICE_UNITS.len(), 7);
        assert_eq!(CORE_TIMERS.len(), 3);
    }
}
