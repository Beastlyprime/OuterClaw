# ClawShell

External security guardian for [OpenClaw](https://openclaw.ai). Monitors your gateway, protects your data, alerts you independently.

## Why ClawShell?

OpenClaw can't detect when it's crashed, hung, or compromised. ClawShell runs as a **separate process under a separate user**, so it keeps working even when OpenClaw doesn't.

| Problem | OpenClaw alone | With ClawShell |
|---------|---------------|----------------|
| Gateway hangs | Nobody notices | Detected in 2-5 min, auto-restarted |
| Process crashes | Restarts with possibly corrupt data | Pre-start integrity check + postmortem forensics |
| Agent deletes its own memory | Data lost | Atomic backups every 30 min, isolated from agent |
| Prompt injection modifies identity | SOUL.md overwritten | `chattr +i` immutable + inotify real-time alert |
| Server is down | No way to know | Independent Telegram alerts |

## Install

```bash
git clone https://github.com/Beastlyprime/ClawShell.git
cd ClawShell
sudo bash install.sh
```

One command. The installer:
- Detects your existing OpenClaw installation
- Creates isolated users (`ocagent` for OpenClaw, `occlawshell` for ClawShell)
- Installs Node.js globally if needed
- Migrates your data to the new user
- Deploys the watchdog, backup timers, and alert system
- Starts everything

Your original data is preserved — nothing is deleted.

## What You Get

### Three-User Isolation

```
you          — Admin (sudo)
ocagent      — Runs OpenClaw (no sudo, can't touch backups)
occlawshell  — Runs ClawShell (limited sudo, can't modify OpenClaw data)
```

Even if an attacker compromises your AI agent, they can't access backups, kill the watchdog, or escalate privileges.

### Automatic Protection

| Feature | Interval | Details |
|---------|----------|---------|
| Health monitoring | 30s | /proc state machine + HTTP health check |
| SQLite backup | 30 min | Atomic `VACUUM INTO` + integrity check |
| File backup | 30 min | MEMORY.md, memory/, config, git bundle |
| LKG promotion | 2 hr | Only promotes when gateway is confirmed healthy |
| Health check | 2 min | Service state + HTTP + memory + restart count |
| Crash forensics | On crash | 14 /proc categories + dmesg + coredump |

### Telegram Alerts

ClawShell **automatically reads your OpenClaw Telegram config** — no extra setup needed. You'll receive alerts when:

- Gateway goes down or hangs
- Gateway recovers
- Identity files are tampered with
- Auto-recovery is triggered
- Disk quota is exceeded

### Telegram Commands (Optional)

Create a dedicated bot via `@BotFather` for two-way interaction:

```
/status           — Gateway state, PID, uptime, memory
/restart          — Restart the gateway
/snapshots        — List recent backups
/unlock_identity  — Temporarily unlock identity files (10 min auto-relock)
/lock_identity    — Re-lock identity files
/help             — List commands
```

Configure in `/var/lib/occlawshell/config/clawshell.env`:
```
CLAWSHELL_TG_TOKEN=your_dedicated_bot_token
CLAWSHELL_TG_CHAT=your_chat_id
```

### Status Report

```bash
sudo python3 /var/lib/occlawshell/bin/clawshell.py --status
```

```
╔══════════════════════════════════════╗
║     ClawShell Guardian v1.0          ║
╚══════════════════════════════════════╝

  System        🟢 HEALTHY
  Gateway       PID 12345
  Uptime        3 days 7 hrs

  Data Protection
  ├─ Last backup     12 min ago
  ├─ Backup count    96
  ├─ LKG states      5 available
  └─ Integrity       ✓ passed

  Security
  ├─ User isolation  ✓ 3-user isolation active
  ├─ Identity files  ✓ 3/3 immutable
  ├─ Config monitor  ✓ inotify active
  └─ Alert channel   ✓ Telegram connected
```

## How It Works

ClawShell has three pillars:

**WATCH** — A Python daemon (`clawshell.py`) that monitors the gateway process via `/proc` and HTTP. Classifies state through a 6-state machine. On confirmed hang: restarts gateway. If still down after 90s: rolls back to last known good state.

**VAULT** — Atomic backups stored in `/var/lib/occlawshell/` (owned by `occlawshell`, inaccessible to `ocagent`). SQLite via `VACUUM INTO`, files via atomic rename. Disk quota prevents unbounded growth. I/O pressure sensing defers snapshots during heavy inference.

**ALERT** — Independent Telegram channel. If the gateway dies, ClawShell can still alert you because it's a separate process. All alerts also logged locally to `audit/alerts.log`.

## Management

```bash
# Service status
sudo systemctl status oc-clawshell
sudo systemctl status openclaw-gateway

# View timers
sudo systemctl list-timers oc-*

# Logs
journalctl -u oc-clawshell -f
journalctl -u openclaw-gateway -f

# Manual snapshot
sudo systemctl start oc-snapshot.service

# Manual rollback (interactive, requires confirmation)
sudo /var/lib/occlawshell/bin/rollback.sh

# Identity file management
sudo python3 /var/lib/occlawshell/bin/clawshell.py --unlock-identity
sudo python3 /var/lib/occlawshell/bin/clawshell.py --lock-identity

# Stop ClawShell (gateway keeps running)
sudo systemctl stop oc-clawshell oc-snapshot.timer oc-healthcheck.timer oc-lkg-promote.timer
```

## Requirements

- Linux (systemd, inotify, /proc, ACL support)
- Python 3.8+ (stdlib only, zero pip dependencies)
- Node.js 20+ (installer handles this)
- OpenClaw installed and configured
- `sudo` access for installation

## Architecture

```
┌──────────────────────────────────────────────────┐
│ systemd                                          │
│  ├─ oc-clawshell.service (occlawshell)           │
│  │   └─ clawshell.py                             │
│  │       ├─ /proc monitor (30s)                  │
│  │       ├─ HTTP health check                    │
│  │       ├─ inotify (SOUL.md, AGENTS.md, ...)    │
│  │       ├─ Telegram bot (optional)              │
│  │       └─ auto-recovery (restart → LKG)        │
│  │                                               │
│  ├─ oc-snapshot.timer (30 min)                   │
│  │   └─ snapshot-sqlite.sh + snapshot-files.sh   │
│  ├─ oc-healthcheck.timer (2 min)                 │
│  │   └─ healthcheck.sh                           │
│  ├─ oc-lkg-promote.timer (2 hr)                  │
│  │   └─ promote-lkg.sh                           │
│  │                                               │
│  └─ openclaw-gateway.service (ocagent)           │
│      ├─ ExecStartPre: pre-start-check.sh         │
│      └─ ExecStopPost: postmortem-collect.sh       │
└──────────────────────────────────────────────────┘
```

## License

MIT
