<p align="center">
  <img src="docs/assets/outerclaw-logo.png" alt="OuterClaw" width="180"/>
</p>

<h1 align="center">OuterClaw</h1>

<p align="center">
  External security guardian for <a href="https://openclaw.ai">OpenClaw</a>.<br/>
  <em>Always on. Every memory kept. Every heartbeat watched.</em>
</p>

<p align="center">
  <a href="https://github.com/Beastlyprime/OuterClaw/releases"><img alt="Release" src="https://img.shields.io/github/v/release/Beastlyprime/OuterClaw?style=flat-square&color=1A3A5C"/></a>
  <a href="https://github.com/Beastlyprime/OuterClaw/blob/main/LICENSE"><img alt="License" src="https://img.shields.io/badge/license-AGPL--3.0-blue?style=flat-square"/></a>
  <a href="https://github.com/Beastlyprime/OuterClaw/actions"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/Beastlyprime/OuterClaw/ci.yml?style=flat-square&label=tests"/></a>
  <a href="https://github.com/Beastlyprime/OuterClaw/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/Beastlyprime/OuterClaw?style=flat-square&color=E8732A"/></a>
  <a href="https://github.com/Beastlyprime/OuterClaw/commits/main"><img alt="Last Commit" src="https://img.shields.io/github/last-commit/Beastlyprime/OuterClaw?style=flat-square"/></a>
</p>

---

A single Rust binary (~3.4 MB) that monitors your OpenClaw gateway, protects your data, and alerts you independently -- running as a **separate process under a separate user**, so it keeps working even when OpenClaw doesn't.

## Why OuterClaw?

OpenClaw can't detect when it's crashed, hung, or compromised.

| Problem | OpenClaw alone | With OuterClaw |
|---------|---------------|----------------|
| Gateway hangs | Nobody notices | Detected in 2-5 min, auto-restarted |
| Process crashes | Restarts with possibly corrupt data | Pre-start integrity check + postmortem forensics |
| Agent deletes its own memory | Data lost | Atomic backups every 30 min, isolated from agent |
| Prompt injection modifies identity | SOUL.md overwritten | Immutable flag + real-time alert |
| Server is down | No way to know | Independent Telegram alerts |

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/Beastlyprime/OuterClaw/main/scripts/install.sh | sudo bash
```

Or manually:

```bash
# Download the binary for your platform
sudo cp outerclaw /usr/local/bin/
sudo chmod +x /usr/local/bin/outerclaw

# Run interactive setup
sudo outerclaw setup
```

The installer asks you to choose:

- **Full mode** (default): Creates `ocagent` + `outerclaw` users, three-user isolation
- **Lightweight mode**: Keeps OpenClaw under your current user, only creates `outerclaw`

## What You Get

### Single Binary, 18 Subcommands

```
outerclaw daemon           # Watchdog daemon (state machine + auto-recovery)
outerclaw status           # Guardian status report
outerclaw setup            # First-time installation
outerclaw deploy           # Idempotent deployment update
outerclaw uninstall        # Clean removal
outerclaw snapshot         # Take SQLite + file backup
outerclaw promote-lkg      # Promote snapshot to Last Known Good
outerclaw rollback         # Interactive destructive rollback
outerclaw auto-recover     # Automated LKG recovery
outerclaw healthcheck      # 4-factor health check
outerclaw pre-start-check  # Gateway startup validation
outerclaw postmortem       # Forensic data collection
outerclaw identity lock    # Set identity files immutable
outerclaw identity unlock  # Temporarily unlock (10 min auto-relock)
outerclaw cloud setup      # Interactive cloud backup setup
outerclaw cloud sync       # Sync to encrypted cloud
outerclaw cloud restore    # Download from cloud
outerclaw completions      # Shell completions (bash/zsh/fish)
```

### Three Pillars

```
  WATCH              VAULT                ALERT
  ─────              ─────                ─────
  /proc monitor      Atomic SQLite        Independent Telegram
  HTTP health        File snapshots       Works when gateway is dead
  6-state machine    30-min interval      Two-way bot commands
  Auto-recovery      LKG promotion        Local audit log
  Identity watcher   Cloud backup (opt)   INFO/WARNING/CRITICAL
```

### User Isolation

```
you          -- Admin (sudo)
ocagent      -- Runs OpenClaw (no sudo, can't touch backups)
outerclaw    -- Runs OuterClaw (limited sudo, can't modify OpenClaw data)
```

### Automatic Protection

| Feature | Interval | Details |
|---------|----------|---------|
| Health monitoring | 30s | /proc state machine + HTTP health check |
| SQLite backup | 30 min | Atomic `VACUUM INTO` + integrity check |
| File backup | 30 min | MEMORY.md, memory/, config, git bundle |
| LKG promotion | 2 hr | Only promotes when gateway is confirmed healthy |
| Health check | 2 min | Service state + HTTP + memory + restart count |
| Cloud backup | 2 hr | Encrypted sync to R2/B2/S3 via rclone (optional) |
| Crash forensics | On crash | 17 /proc categories + dmesg + coredump |

### Cloud Backup (Optional)

```bash
sudo outerclaw cloud setup
```

- Supports **Cloudflare R2**, Backblaze B2, AWS S3, or any rclone backend
- **Client-side AES-256 encryption** -- cloud provider never sees plaintext
- `sudo outerclaw cloud restore --list` to view backups

### Telegram Alerts

OuterClaw **automatically reads your OpenClaw Telegram config** -- no extra setup needed. Optional two-way bot with commands:

```
/status  /restart  /kill  /sessions  /kill_session <id>
/snapshots  /unlock_identity  /lock_identity  /help
```

### Status Report

```bash
sudo outerclaw status
```

```
==========================================
     OuterClaw Guardian v0.1.0
==========================================

  System        [OK] HEALTHY
  Gateway       PID 12345
  Uptime        3 days 7 hrs

  Data Protection
  +-- Last backup     12 min ago
  +-- Backup count    96
  +-- LKG states      10 available
  +-- Integrity       [OK] passed

  Security
  +-- User isolation  [OK] 3-user isolation active
  +-- Identity files  [OK] 3/3 immutable
  +-- Config monitor  [OK] file watcher active
  +-- Alert channel   [OK] Telegram connected
```

## How It Works

**WATCH** -- Daemon that monitors the gateway via `/proc` and HTTP. 6-state machine classifies process health. On confirmed hang or failure: restarts gateway. If still down after 90s: rolls back to last known good state. Cross-platform file watching for identity tampering.

**VAULT** -- Atomic backups in `/var/lib/outerclaw/` (inaccessible to the agent). SQLite via `VACUUM INTO`, files with SHA-256 manifests. Disk quota with emergency pruning. I/O pressure gating. Optional encrypted cloud sync.

**ALERT** -- Independent Telegram channel. Separate process means alerts work even when the gateway is dead. All alerts also logged locally.

## Management

```bash
# Status
sudo outerclaw status
sudo systemctl status oc-outerclaw

# Logs
journalctl -u oc-outerclaw -f

# Manual operations
sudo -u outerclaw outerclaw snapshot
sudo outerclaw rollback
sudo outerclaw identity unlock

# Cloud
sudo outerclaw cloud setup
sudo outerclaw cloud restore --list

# Deploy updates
sudo outerclaw deploy

# Uninstall
sudo outerclaw uninstall
```

## Supported Platforms

| Platform | Init System | Status |
|----------|-------------|--------|
| Linux | systemd | Full support |
| Linux | OpenRC | Supported |
| Linux | runit | Supported |
| Linux | Container (Docker/Podman) | Supported (no init) |
| macOS | launchd | Supported |
| FreeBSD | rc.d | Supported |

## Requirements

- OpenClaw installed and configured
- `sudo` access for installation
- rclone (optional, for cloud backup)

## Contributing

Contributions welcome! Check out our [issues](https://github.com/Beastlyprime/OuterClaw/issues) -- look for the `good first issue` label.

## License

[AGPL-3.0](LICENSE)
