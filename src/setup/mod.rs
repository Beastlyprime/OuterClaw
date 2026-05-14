//! Setup module — install, deploy, and uninstall OuterClaw.
//!
//! Ports `install.sh`, `deploy.sh`, and `uninstall.sh` to Rust.
//! The single binary replaces all scripts; `deploy()` copies itself
//! to `/var/lib/outerclaw/bin/outerclaw` and generates systemd units
//! that point to subcommands of that binary.

use crate::cli::{SetupArgs, UninstallArgs};
use crate::config::Config;
use crate::platform::Platform;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub mod launchd;
pub mod openrc;
pub mod platform_detect;
pub mod runit;
pub mod systemd;
pub mod users;

/// Systemd unit directory.
const SYSTEMD_DIR: &str = "/etc/systemd/system";

// ═══════════════════════════════════════════════════════════════════════════
// install()
// ═══════════════════════════════════════════════════════════════════════════

/// First-time installation — port of `install.sh`.
///
/// Returns 0 on success, non-zero on failure.
pub fn install(args: SetupArgs, cfg: Config, platform: Box<dyn Platform>) -> i32 {
    if !platform_detect::is_root() {
        eprintln!("ERROR: Must run as root (sudo outerclaw setup)");
        return 1;
    }

    // ── Detect human user ────────────────────────────────────────
    let human_user = match platform_detect::detect_human_user() {
        Some(u) => u,
        None => {
            eprintln!("ERROR: Cannot detect current user. Run with: sudo outerclaw setup");
            return 1;
        }
    };

    let human_home = format!("/home/{human_user}");
    let human_openclaw = format!("{human_home}/.openclaw");

    if !Path::new(&human_openclaw).exists() {
        eprintln!(
            "ERROR: OpenClaw installation not found at {human_openclaw}\n\
             Install OpenClaw first: npm install -g openclaw && openclaw setup"
        );
        return 1;
    }

    println!();
    println!("  OuterClaw Guardian — Installer");
    println!();
    println!("  Detected user: {human_user}");
    println!("  OpenClaw dir:  {human_openclaw}");
    println!();

    // ── Mode selection ───────────────────────────────────────────
    let lightweight = if args.lightweight {
        true
    } else if args.yes {
        false // default to full mode in non-interactive
    } else {
        let items = vec![
            "Full: Create dedicated user (ocagent), migrate data",
            "Lightweight: Keep OpenClaw under current user",
        ];
        let selection = dialoguer::Select::new()
            .with_prompt("Installation mode")
            .items(&items)
            .default(0)
            .interact();
        matches!(selection, Ok(1))
    };

    let (agent_user, agent_home, openclaw_dir) = if lightweight {
        println!("  Lightweight mode: OpenClaw stays under {human_user}");
        (
            human_user.clone(),
            human_home.clone(),
            human_openclaw.clone(),
        )
    } else {
        println!("  Full mode: OpenClaw will use ocagent");
        (
            "ocagent".to_string(),
            "/home/ocagent".to_string(),
            "/home/ocagent/.openclaw".to_string(),
        )
    };

    // ── Step 1: Create users ─────────────────────────────────────
    println!();
    println!("[1/6] Creating service users");

    if !lightweight {
        if let Err(e) = users::ensure_login_user("ocagent") {
            eprintln!("  ERROR creating ocagent: {e}");
            return 1;
        }
        println!("  User 'ocagent' ready");
    }

    let watchdog = cfg.watchdog_user.clone();
    if let Err(e) = users::ensure_system_user(&watchdog, &cfg.vault_dir, "/usr/sbin/nologin") {
        eprintln!("  ERROR creating {watchdog}: {e}");
        return 1;
    }
    println!("  User '{watchdog}' ready");

    // ── Step 2: Deploy (vault, binary, units, config) ────────────
    // Build a temporary config that reflects the chosen agent_user / openclaw_dir
    let mut deploy_cfg = cfg.clone();
    deploy_cfg.agent_user = agent_user.clone();
    deploy_cfg.openclaw_dir = PathBuf::from(&openclaw_dir);

    println!();
    println!("[2/6] Deploying OuterClaw (vault, binary, systemd units)");
    let rc = deploy_inner(&deploy_cfg, platform.as_ref());
    if rc != 0 {
        return rc;
    }

    // ── Step 3: ACLs ─────────────────────────────────────────────
    println!();
    println!("[3/6] Setting ACLs");
    if let Err(e) = set_acls(&agent_home, &openclaw_dir) {
        eprintln!("  WARNING: ACL setup failed: {e}");
        // Non-fatal — continue
    } else {
        println!("  ACLs configured");
    }

    // ── Step 4: Immutable identity files ─────────────────────────
    println!();
    println!("[4/6] Setting identity files immutable");
    let workspace = PathBuf::from(&openclaw_dir).join("workspace");
    for name in &["SOUL.md", "AGENTS.md", "USER.md"] {
        let fpath = workspace.join(name);
        if fpath.exists() {
            match platform.set_immutable(&fpath, true) {
                Ok(()) => println!("  {name} set immutable"),
                Err(e) => eprintln!("  WARNING: {name}: {e}"),
            }
        } else {
            println!("  {name} not found (skipped)");
        }
    }

    // ── Step 5: Enable and start services ────────────────────────
    println!();
    println!("[5/6] Starting services");

    // Enable and start gateway
    let _ = systemctl(&["enable", "openclaw-gateway.service"]);
    let _ = systemctl(&["start", "openclaw-gateway.service"]);
    if check_service_active("openclaw-gateway.service") {
        println!("  Gateway running (User={agent_user})");
    } else {
        eprintln!(
            "  WARNING: Gateway failed to start — check: journalctl -u openclaw-gateway -n 20"
        );
    }

    // Enable and start OuterClaw daemon
    let _ = systemctl(&["enable", "--now", "oc-outerclaw.service"]);
    if check_service_active("oc-outerclaw.service") {
        println!("  OuterClaw watchdog running");
    } else {
        eprintln!("  WARNING: OuterClaw failed to start — check: journalctl -u oc-outerclaw -n 20");
    }

    // Enable timers
    for timer in systemd::CORE_TIMERS {
        let _ = systemctl(&["enable", "--now", timer]);
    }
    println!("  Timers started");

    // ── Step 6: Summary ──────────────────────────────────────────
    println!();
    println!("[6/6] Installation complete");
    println!();
    if lightweight {
        println!("  Two-user isolation active:");
        println!("    {human_user}     -- Admin + OpenClaw");
        println!("    outerclaw  -- OuterClaw (limited sudo)");
    } else {
        println!("  Three-user isolation active:");
        println!("    {human_user}     -- Admin (sudo)");
        println!("    ocagent    -- OpenClaw (no sudo)");
        println!("    outerclaw  -- OuterClaw (limited sudo)");
    }
    println!();
    println!("  Services:");
    println!("    Gateway:     sudo systemctl status openclaw-gateway");
    println!("    Watchdog:    sudo systemctl status oc-outerclaw");
    println!("    Snapshots:   sudo systemctl list-timers oc-*");
    println!();

    0
}

// ═══════════════════════════════════════════════════════════════════════════
// deploy()
// ═══════════════════════════════════════════════════════════════════════════

/// Idempotent deployment update — port of `deploy.sh`.
///
/// Returns 0 on success, non-zero on failure.
pub fn deploy(cfg: Config, platform: Box<dyn Platform>) -> i32 {
    if !platform_detect::is_root() {
        eprintln!("ERROR: Must run as root (sudo outerclaw deploy)");
        return 1;
    }

    println!();
    println!("  OuterClaw — Deployment");
    println!();

    let rc = deploy_inner(&cfg, platform.as_ref());
    if rc != 0 {
        return rc;
    }

    println!();
    println!("  Deployment complete.");
    println!();

    0
}

/// Internal deployment logic shared by `install()` and `deploy()`.
fn deploy_inner(cfg: &Config, _platform: &dyn Platform) -> i32 {
    let vault = cfg.vault_dir.clone();
    let vault_str = vault.to_string_lossy().to_string();
    let watchdog = &cfg.watchdog_user;
    let agent_user = &cfg.agent_user;
    let openclaw_dir = cfg.openclaw_dir.to_string_lossy().to_string();

    // ── Phase 1: Create users if missing ─────────────────────────
    println!("  Phase 1: Ensuring users exist");
    if let Err(e) = users::ensure_system_user(watchdog, &vault, "/usr/sbin/nologin") {
        eprintln!("  ERROR creating {watchdog} user: {e}");
        return 1;
    }
    // Only create ocagent if that's the configured agent user
    if agent_user == "ocagent" && !users::user_exists("ocagent") {
        if let Err(e) = users::ensure_login_user("ocagent") {
            eprintln!("  ERROR creating ocagent user: {e}");
            return 1;
        }
    }

    // ── Phase 2: Create vault structure ──────────────────────────
    println!("  Phase 2: Vault directory structure");
    let subdirs = ["lkg", "snapshots", "postmortem", "audit", "config", "bin"];
    for subdir in &subdirs {
        let dir = vault.join(subdir);
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("  ERROR creating {}: {e}", dir.display());
            return 1;
        }
    }

    // Set ownership: vault owned by the watchdog user
    let owner = format!("{watchdog}:{watchdog}");
    let _ = Command::new("chown")
        .args(["-R", &owner, &vault_str])
        .status();
    let _ = Command::new("chmod")
        .args(["-R", "700", &vault_str])
        .status();
    // Vault root: 711 allows traverse but not listing
    let _ = Command::new("chmod").args(["711", &vault_str]).status();
    // bin/ must be 755 so gateway service (ocagent) can execute start-gateway.sh
    let bin_dir = vault.join("bin");
    let bin_dir_str = bin_dir.to_string_lossy();
    let _ = Command::new("chmod").args(["755", &*bin_dir_str]).status();
    println!("  Vault structure at {vault_str}");

    // ── Phase 3: Copy self binary ────────────────────────────────
    println!("  Phase 3: Deploying binary");
    let dest_bin = cfg.bin_path();
    match std::env::current_exe() {
        Ok(self_path) => {
            if let Err(e) = fs::copy(&self_path, &dest_bin) {
                eprintln!(
                    "  ERROR copying {} -> {}: {e}",
                    self_path.display(),
                    dest_bin.display()
                );
                return 1;
            }
            // Root-owned, 755 — outerclaw can execute but not modify
            let _ = Command::new("chown")
                .args(["root:root", &dest_bin.to_string_lossy()])
                .status();
            let _ = fs::set_permissions(&dest_bin, fs::Permissions::from_mode(0o755));
            println!("  Binary deployed to {}", dest_bin.display());
        }
        Err(e) => {
            eprintln!("  ERROR: cannot determine self binary path: {e}");
            return 1;
        }
    }

    // ── Phase 4: ACLs ────────────────────────────────────────────
    println!("  Phase 4: Setting ACLs");
    let agent_home = format!("/home/{agent_user}");
    if let Err(e) = set_acls(&agent_home, &openclaw_dir) {
        eprintln!("  WARNING: ACL setup: {e}");
    } else {
        println!("  ACLs configured");
    }

    // ── Phase 5: Sudoers ─────────────────────────────────────────
    println!("  Phase 5: Sudoers");
    let sudoers_path = "/etc/sudoers.d/outerclaw";
    let sudoers_content = systemd::sudoers_config(cfg);
    if let Err(e) = fs::write(sudoers_path, &sudoers_content) {
        eprintln!("  ERROR writing sudoers: {e}");
        return 1;
    }
    let _ = fs::set_permissions(sudoers_path, fs::Permissions::from_mode(0o440));

    // Validate with visudo
    let visudo_ok = Command::new("visudo")
        .args(["-c", "-f", sudoers_path])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if visudo_ok {
        println!("  Sudoers rules deployed");
    } else {
        eprintln!("  ERROR: Invalid sudoers syntax, removing");
        let _ = fs::remove_file(sudoers_path);
        return 1;
    }

    // ── Phase 6: Systemd units ───────────────────────────────────
    println!("  Phase 6: Systemd units");
    let units: Vec<(&str, String)> = vec![
        ("oc-outerclaw.service", systemd::outerclaw_service(cfg)),
        ("openclaw-gateway.service", systemd::gateway_service(cfg)),
        ("oc-snapshot.timer", systemd::snapshot_timer().to_string()),
        ("oc-snapshot.service", systemd::snapshot_service(cfg)),
        (
            "oc-healthcheck.timer",
            systemd::healthcheck_timer().to_string(),
        ),
        ("oc-healthcheck.service", systemd::healthcheck_service(cfg)),
        (
            "oc-lkg-promote.timer",
            systemd::lkg_promote_timer().to_string(),
        ),
        ("oc-lkg-promote.service", systemd::lkg_promote_service(cfg)),
        (
            "oc-cloud-sync.timer",
            systemd::cloud_sync_timer().to_string(),
        ),
        ("oc-cloud-sync.service", systemd::cloud_sync_service(cfg)),
        (
            "oc-identity-lock.service",
            systemd::identity_lock_service(cfg),
        ),
        (
            "oc-identity-unlock.service",
            systemd::identity_unlock_service(cfg),
        ),
    ];

    for (name, content) in &units {
        let unit_path = format!("{SYSTEMD_DIR}/{name}");
        if let Err(e) = fs::write(&unit_path, content) {
            eprintln!("  ERROR writing {unit_path}: {e}");
            return 1;
        }
        println!("  Deployed {name}");
    }

    // daemon-reload
    let _ = systemctl(&["daemon-reload"]);
    println!("  systemd reloaded");

    // ── Phase 7: start-gateway.sh ────────────────────────────────
    println!("  Phase 7: Gateway start script");
    let openclaw_bin = platform_detect::detect_openclaw_binary(agent_user);
    let gateway_script_path = vault.join("bin").join("start-gateway.sh");
    match openclaw_bin {
        Some(ref bin_path) => {
            // Use the parent of the detected path (not canonicalized, so symlinks
            // like .npm-global/bin/openclaw -> ../lib/... keep their bin/ parent).
            // Only convert relative paths to absolute via canonicalizing the parent dir.
            let node_dir = match bin_path.parent() {
                Some(dir) => dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()),
                None => PathBuf::from("/usr/local/bin"),
            };
            let script_content = format!(
                "#!/bin/bash\n\
                 # Auto-generated by outerclaw deploy\n\
                 export PATH=\"{}:$PATH\"\n\
                 exec openclaw gateway --port \"${{OPENCLAW_GATEWAY_PORT:-18789}}\"\n",
                node_dir.display()
            );
            if let Err(e) = fs::write(&gateway_script_path, &script_content) {
                eprintln!("  ERROR writing start-gateway.sh: {e}");
            } else {
                let _ = Command::new("chown")
                    .args(["root:root", &gateway_script_path.to_string_lossy()])
                    .status();
                let _ =
                    fs::set_permissions(&gateway_script_path, fs::Permissions::from_mode(0o755));
                println!(
                    "  Generated start-gateway.sh (node: {})",
                    node_dir.display()
                );
            }
        }
        None => {
            // If gateway script already exists, keep it; otherwise warn
            if gateway_script_path.exists() {
                println!("  start-gateway.sh already exists (kept)");
            } else {
                eprintln!("  WARNING: start-gateway.sh not generated (openclaw binary not found)");
            }
        }
    }

    // ── Phase 8: Logrotate ───────────────────────────────────────
    println!("  Phase 8: Logrotate");
    let logrotate_path = "/etc/logrotate.d/outerclaw";
    if let Err(e) = fs::write(logrotate_path, systemd::logrotate_config(cfg)) {
        eprintln!("  WARNING: Could not write logrotate config: {e}");
    } else {
        let _ = fs::set_permissions(logrotate_path, fs::Permissions::from_mode(0o644));
        println!("  Logrotate config deployed");
    }

    // ── Phase 9: outerclaw.env ───────────────────────────────────
    println!("  Phase 9: Environment config");
    let env_path = vault.join("config").join("outerclaw.env");
    if !env_path.exists() {
        let openclaw_bin_str = openclaw_bin
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let env_content = format!(
            "# OuterClaw Configuration\n\
             # Telegram alerts auto-read from OpenClaw's openclaw.json (no config needed)\n\
             # To enable two-way Telegram bot, create a dedicated bot via @BotFather and set:\n\
             # OUTERCLAW_TG_TOKEN=\n\
             # OUTERCLAW_TG_CHAT=\n\
             GATEWAY_PORT={gateway_port}\n\
             AGENT_USER={agent_user}\n\
             OPENCLAW_DIR={openclaw_dir}\n\
             OPENCLAW_BIN={openclaw_bin_str}\n",
            gateway_port = cfg.gateway_port,
        );
        if let Err(e) = fs::write(&env_path, &env_content) {
            eprintln!("  ERROR writing outerclaw.env: {e}");
        } else {
            let _ = Command::new("chown")
                .args(["outerclaw:outerclaw", &env_path.to_string_lossy()])
                .status();
            let _ = fs::set_permissions(&env_path, fs::Permissions::from_mode(0o600));
            println!("  outerclaw.env created");
        }
    } else {
        println!("  outerclaw.env already exists (kept)");
    }

    // ── Phase 10: Enable timers, restart daemon ──────────────────
    println!("  Phase 10: Enable timers");
    for timer in systemd::CORE_TIMERS {
        let _ = systemctl(&["enable", timer]);
        println!("  Enabled {timer}");
    }

    // Cloud sync timer — only enable if configured
    if cfg.cloud_enabled {
        let _ = systemctl(&["enable", "oc-cloud-sync.timer"]);
        println!("  Enabled oc-cloud-sync.timer");
    }

    // Restart oc-outerclaw if running
    if check_service_active("oc-outerclaw.service") {
        let _ = systemctl(&["restart", "oc-outerclaw.service"]);
        println!("  OuterClaw daemon restarted");
    } else {
        println!("  OuterClaw daemon not running (start with: sudo systemctl enable --now oc-outerclaw.service)");
    }

    0
}

// ═══════════════════════════════════════════════════════════════════════════
// uninstall()
// ═══════════════════════════════════════════════════════════════════════════

/// Uninstall OuterClaw — port of `uninstall.sh`.
///
/// Returns 0 on success, non-zero on failure.
pub fn uninstall(args: UninstallArgs, cfg: Config, platform: Box<dyn Platform>) -> i32 {
    if !platform_detect::is_root() {
        eprintln!("ERROR: Must run as root (sudo outerclaw uninstall)");
        return 1;
    }

    let agent_user = &cfg.agent_user;
    let openclaw_dir = cfg.openclaw_dir.to_string_lossy().to_string();
    let agent_home = format!("/home/{agent_user}");

    println!();
    println!("  OuterClaw — Uninstaller");
    println!();
    println!("  Agent user:    {agent_user}");
    println!("  OpenClaw dir:  {openclaw_dir}");
    println!("  Vault:         {}", cfg.vault_dir.display());
    println!();

    // ── Confirmation ─────────────────────────────────────────────
    if !args.yes {
        let confirm = dialoguer::Confirm::new()
            .with_prompt("Proceed with uninstall?")
            .default(false)
            .interact();
        match confirm {
            Ok(true) => {}
            _ => {
                println!("  Aborted.");
                return 0;
            }
        }
    }

    // ── Step 1: Stop and disable all timers + services ───────────
    println!();
    println!("[1/7] Stopping services and timers");
    for timer in systemd::TIMER_UNITS {
        let _ = systemctl(&["disable", "--now", timer]);
    }
    println!("  Timers stopped");

    for svc in systemd::SERVICE_UNITS {
        let _ = systemctl(&["disable", "--now", svc]);
    }
    println!("  Services stopped");

    // ── Step 2: Remove systemd unit files ────────────────────────
    println!();
    println!("[2/7] Removing systemd units");
    for timer in systemd::TIMER_UNITS {
        let path = format!("{SYSTEMD_DIR}/{timer}");
        let _ = fs::remove_file(&path);
    }
    for svc in systemd::SERVICE_UNITS {
        let path = format!("{SYSTEMD_DIR}/{svc}");
        let _ = fs::remove_file(&path);
    }
    let _ = systemctl(&["daemon-reload"]);
    let _ = systemctl(&["reset-failed"]);
    println!("  Unit files removed and systemd reloaded");

    // ── Step 3: Remove sudoers and logrotate ─────────────────────
    println!();
    println!("[3/7] Removing sudoers and logrotate configs");
    let _ = fs::remove_file("/etc/sudoers.d/outerclaw");
    let _ = fs::remove_file("/etc/logrotate.d/outerclaw");
    println!("  Configs removed");

    // ── Step 4: Remove immutable attributes ──────────────────────
    println!();
    println!("[4/7] Removing immutable attributes");
    let workspace = PathBuf::from(&openclaw_dir).join("workspace");
    for name in &["SOUL.md", "AGENTS.md", "USER.md"] {
        let fpath = workspace.join(name);
        if fpath.exists() {
            match platform.set_immutable(&fpath, false) {
                Ok(()) => println!("  {name} unlocked"),
                Err(e) => eprintln!("  WARNING: {name}: {e}"),
            }
        }
    }

    // ── Step 5: Remove ACLs ──────────────────────────────────────
    println!();
    println!("[5/7] Removing ACLs");
    remove_acls(&agent_home, &openclaw_dir);
    println!("  ACLs removed");

    // ── Step 6: Optionally remove vault ──────────────────────────
    println!();
    println!("[6/7] Vault removal");
    let vault_path = cfg.vault_dir.clone();
    if vault_path.exists() {
        let remove_vault = if args.remove_vault || args.yes {
            true
        } else {
            let confirm = dialoguer::Confirm::new()
                .with_prompt("Remove vault and all backup data? This CANNOT be undone")
                .default(false)
                .interact();
            confirm.unwrap_or(false)
        };

        if remove_vault {
            if let Err(e) = fs::remove_dir_all(&vault_path) {
                eprintln!("  WARNING: Could not remove vault: {e}");
            } else {
                println!("  Vault removed");
            }
        } else {
            println!("  Vault preserved at {}", vault_path.display());
        }
    } else {
        println!("  Vault not found (already removed)");
    }

    // ── Step 7: Optionally remove watchdog user ──────────────────
    println!();
    println!("[7/7] User removal");
    let watchdog = cfg.watchdog_user.clone();
    if users::user_exists(&watchdog) {
        let remove_user = if args.remove_users || args.yes {
            true
        } else {
            let confirm = dialoguer::Confirm::new()
                .with_prompt(format!("Remove the '{watchdog}' system user?"))
                .default(false)
                .interact();
            confirm.unwrap_or(false)
        };

        if remove_user {
            match users::remove_user(&watchdog) {
                Ok(()) => println!("  User '{watchdog}' removed"),
                Err(e) => eprintln!("  WARNING: {e}"),
            }
        } else {
            println!("  User '{watchdog}' preserved");
        }
    } else {
        println!("  User '{watchdog}' not found (already removed)");
    }

    // ── Done ─────────────────────────────────────────────────────
    println!();
    println!("  OuterClaw — Uninstall Complete");
    println!();
    println!("  NOTE: openclaw-gateway.service was NOT removed (belongs to OpenClaw).");
    println!(
        "  It references {}/bin/ which may no longer exist.",
        cfg.vault_dir.display()
    );
    println!();
    println!("  To remove it:");
    println!("    sudo systemctl disable --now openclaw-gateway.service");
    println!("    sudo rm {SYSTEMD_DIR}/openclaw-gateway.service");
    println!("    sudo systemctl daemon-reload");
    println!();

    0
}

// ═══════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Run `systemctl` with the given arguments. Returns true if the command
/// succeeded.
fn systemctl(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check whether a systemd service is currently active.
fn check_service_active(service: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", service])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Set filesystem ACLs for the outerclaw user to read OpenClaw data.
///
/// Uses `setfacl` via `std::process::Command`. The mask is set explicitly
/// alongside each user-ACL entry — POSIX ACL masks otherwise get recomputed
/// when group bits change (e.g. `chmod g-rwx`), silently collapsing the
/// effective permission to `---` and locking outerclaw out of the directory.
fn set_acls(agent_home: &str, openclaw_dir: &str) -> Result<(), String> {
    // Home directory traverse — explicit mask (defends against drift).
    run_setfacl(&["-m", "u:outerclaw:x,m::x", agent_home])?;

    // .openclaw root traverse + read
    run_setfacl(&["-m", "u:outerclaw:rx,m::rx", openclaw_dir])?;

    // Recursively grant rX to outerclaw on data subtrees, and set default
    // ACLs so files created later inherit the right permissions.
    let data_subdirs = ["workspace", "memory", "tasks", "logs"];
    for sub in &data_subdirs {
        let path = format!("{openclaw_dir}/{sub}");
        if Path::new(&path).exists() {
            run_setfacl(&["-R", "-m", "u:outerclaw:rX,m::rX", &path])?;
            run_setfacl(&["-R", "-d", "-m", "u:outerclaw:rX,m::rX", &path])?;
        }
    }

    // Default ACL on openclaw root (covers files created at the top level).
    run_setfacl(&["-d", "-m", "u:outerclaw:r,m::r", openclaw_dir])?;

    // Config files
    for cfg_name in &["openclaw.json", "exec-approvals.json", ".env"] {
        let cfg_path = format!("{openclaw_dir}/{cfg_name}");
        if Path::new(&cfg_path).exists() {
            run_setfacl(&["-m", "u:outerclaw:r,m::r", &cfg_path])?;
        }
    }

    Ok(())
}

/// Remove filesystem ACLs for the outerclaw user from OpenClaw directories.
fn remove_acls(agent_home: &str, openclaw_dir: &str) {
    let _ = run_setfacl(&["-x", "u:outerclaw", agent_home]);
    let _ = run_setfacl(&["-x", "u:outerclaw", openclaw_dir]);
    let _ = run_setfacl(&["-d", "-x", "u:outerclaw", openclaw_dir]);

    for subdir in &["workspace", "memory", "logs"] {
        let path = format!("{openclaw_dir}/{subdir}");
        if Path::new(&path).exists() {
            let _ = run_setfacl(&["-R", "-x", "u:outerclaw", &path]);
            let _ = run_setfacl(&["-R", "-d", "-x", "u:outerclaw", &path]);
        }
    }

    for cfg_name in &["openclaw.json", "exec-approvals.json", ".env"] {
        let cfg_path = format!("{openclaw_dir}/{cfg_name}");
        if Path::new(&cfg_path).exists() {
            let _ = run_setfacl(&["-x", "u:outerclaw", &cfg_path]);
        }
    }
}

/// Run setfacl with the given arguments.
fn run_setfacl(args: &[&str]) -> Result<(), String> {
    let output = Command::new("setfacl")
        .args(args)
        .output()
        .map_err(|e| format!("setfacl {}: {e}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "setfacl {} failed (exit {}): {stderr}",
            args.join(" "),
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_service_active_nonexistent() {
        // A bogus service should not be active
        assert!(!check_service_active("outerclaw-test-bogus-12345.service"));
    }

    #[test]
    fn test_systemctl_daemon_reload_as_non_root() {
        // When running tests as non-root, systemctl commands should fail
        // gracefully (return false) rather than panic
        if !platform_detect::is_root() {
            assert!(!systemctl(&["daemon-reload"]));
        }
    }
}
