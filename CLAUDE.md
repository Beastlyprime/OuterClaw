# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

ClawShell is an external watchdog and data protection system for OpenClaw (v1.0). It runs as an independent process under a dedicated system user (`occlawshell`) to monitor OpenClaw's gateway health, maintain atomic backups, detect tampering, and send independent alerts. Core invariants: OpenClaw stays online, no data leakage, no prompt injection exploitation, memory never lost, rollback to any LKG state, no zombie processes.

## Commands

### Install (new users)
```bash
sudo bash install.sh
```

### Run tests
```bash
python3 test_clawshell.py
```

### Deploy updates (existing installation)
```bash
sudo bash deploy.sh
```

### Status
```bash
python3 clawshell.py --status
```

### Monitor
```bash
journalctl -u oc-clawshell -f
tail -f /var/lib/occlawshell/audit/alerts.log
```

## Architecture: Three Pillars

### 1. WATCH — `clawshell.py`
Pure Python (stdlib only) watchdog daemon (~1000 lines). Features:
- `/proc` collection every 30s + HTTP health check at `:18789/health`
- 6-state machine: `UNKNOWN → HEALTHY → HEAVY_INFERENCE → POSSIBLE_HANG → CONFIRMED_HANG / ZOMBIE / DOWN`
- Two-step auto-recovery: restart gateway → LKG recovery (30-min cooldown)
- inotify monitoring of identity files (SOUL.md, AGENTS.md, USER.md, openclaw.json)
- Two-way Telegram bot (`/status`, `/restart`, `/snapshots`, `/unlock_identity`, `/lock_identity`, `/help`)
- `--status` CLI for human-friendly status report
- `sd_notify()` heartbeat every 1s for systemd watchdog

### 2. VAULT — Snapshots & LKG
Atomic backups stored in `/var/lib/occlawshell/` (owned by `occlawshell`, mode 0700):
- `snapshots/` — 30-min interval backups (96 retained = 48 hours), with disk quota and I/O pressure gating
- `lkg/` — Last Known Good states (10 retained), promoted every 2 hours after safety gates
- `postmortem/` — Forensic data from crashes
- `audit/` — Alert, backup, and LKG logs

Key scripts: `snapshot-sqlite.sh` (VACUUM INTO), `snapshot-files.sh`, `promote-lkg.sh` (uptime + health gates), `rollback.sh` (human-triggered only), `postmortem-collect.sh`, `auto-recover.sh` (automated LKG recovery on persistent failures), `pre-start-check.sh` (blocks gateway startup if data corrupted), `quota-check.sh` (disk quota with emergency pruning), `io-pressure-check.sh` (PSI-based I/O gating).

### 3. ALERT — `scripts/alert.sh`
Independent Telegram notification channel. Auto-reads bot token from OpenClaw's `openclaw.json` (zero config). Loads credentials via `grep` (not `source`) to prevent injection. Always logs locally to `audit/alerts.log`. Levels: INFO, WARNING, CRITICAL.

## Security Model — Three-User Isolation

```
<admin>      — Human admin, has sudo (controls everything)
ocagent      — Runs OpenClaw, no sudo (pure application user)
occlawshell  — Runs ClawShell, limited sudo (nologin system user)
```

- **Three-user OS isolation:** `ocagent` (runs OpenClaw, no sudo) and `occlawshell` (runs ClawShell, limited sudo) are separate unprivileged users — they can't kill each other's processes or access each other's files. `occlawshell` has narrowly scoped sudoers entries for gateway restart, auto-recover, and identity lock/unlock only.
- **Immutability:** `chattr +i` on identity files (SOUL.md, AGENTS.md, USER.md), with CLI/Telegram unlock command (10-min auto-relock timeout)
- **systemd hardening:** `ProtectSystem=strict`, `SystemCallFilter=@system-service`, `OOMScoreAdjust=-500`, `WatchdogSec=120`
- **Injection-safe scripts:** All shell scripts parse env vars with `grep` instead of `source`
- **inotify tampering detection:** Real-time monitoring of identity and config files, CRITICAL alert on modification

## Key Design Constraints

- `clawshell.py` must have **zero external dependencies** — Python stdlib only
- Shell scripts must never use `source`/`eval` on config files — parse with `grep` to prevent injection
- `rollback.sh` is **human-triggered only** — never automated; `auto-recover.sh` is the separate automated recovery path (triggered by clawshell.py, limited scope, with cooldown)
- Snapshots use atomic operations: `VACUUM INTO` for SQLite, atomic rename for files
- All scripts must be idempotent
- Telegram token auto-reads from OpenClaw config; dedicated bot optional for two-way interaction

## File Layout

| File | Purpose |
|------|---------|
| `install.sh` | One-click installer (detects OpenClaw, creates users, migrates data, deploys everything) |
| `clawshell.py` | Core watchdog daemon (~1000 lines): state machine, auto-recovery, inotify, Telegram bot, --status |
| `test_clawshell.py` | Unit tests (27 tests, unittest framework) |
| `deploy.sh` | Idempotent deployment for updates (vault, scripts, ACLs, sudoers, systemd) |
| `scripts/alert.sh` | Telegram + local audit log notifications (auto-reads OpenClaw config) |
| `scripts/healthcheck.sh` | 4-factor health check (service state, HTTP, memory, restarts) |
| `scripts/snapshot-sqlite.sh` | Atomic SQLite backup (VACUUM INTO + integrity check + quota + I/O gate) |
| `scripts/snapshot-files.sh` | MEMORY.md, memory/, config, git bundle backup (+ quota + I/O gate) |
| `scripts/promote-lkg.sh` | LKG promotion with safety gates (uptime + health + row count) |
| `scripts/rollback.sh` | Human-triggered destructive rollback with confirmation |
| `scripts/postmortem-collect.sh` | Crash forensics (14 /proc categories + dmesg + coredump) |
| `scripts/auto-recover.sh` | Automated LKG recovery triggered by clawshell.py |
| `scripts/pre-start-check.sh` | Blocks gateway start if SQLite missing/corrupted |
| `scripts/quota-check.sh` | Vault disk quota check with emergency pruning |
| `scripts/io-pressure-check.sh` | PSI-based I/O pressure gate with retry |
| `scripts/identity-lock.sh` | Identity file chattr +i/-i (lock/unlock) |
| `scripts/migrate-to-ocagent.sh` | Data migration from current user to ocagent |
| `systemd/oc-clawshell.service` | Watchdog daemon (Type=notify, WatchdogSec=120, OOMScoreAdjust=-500) |
| `systemd/oc-snapshot.timer/service` | 30-min snapshot trigger (sqlite + files) |
| `systemd/oc-healthcheck.timer/service` | 2-min health check trigger |
| `systemd/oc-lkg-promote.timer/service` | 2-hour LKG promotion trigger |
| `systemd/oc-identity-lock.service` | Identity file lock (oneshot, runs outside sandbox) |
| `systemd/oc-identity-unlock.service` | Identity file unlock (oneshot, runs outside sandbox) |
| `systemd/openclaw-gateway.service` | Gateway service (User=ocagent, pre-start check, postmortem on crash) |
| `logrotate/occlawshell` | Daily log rotation (30 days) |

## Configuration

Environment variables in `/var/lib/occlawshell/config/clawshell.env`:
- `GATEWAY_PORT` (default: 18789) — OpenClaw health endpoint port
- `CLAWSHELL_TG_TOKEN` / `CLAWSHELL_TG_CHAT` — Dedicated Telegram bot credentials (optional; alerts auto-read from OpenClaw config)
- `CLAWSHELL_DEBUG` — Enable debug logging
- `AGENT_USER` (default: ocagent) — User running OpenClaw
- `OPENCLAW_DIR` (default: /home/ocagent/.openclaw) — OpenClaw data directory
- `MAX_VAULT_MB` (default: 2048) — Maximum vault disk usage in MB before snapshots are deferred
- `IO_PRESSURE_THRESHOLD` (default: 25) — Max I/O PSI avg10 percentage before deferring snapshots

Key thresholds in `clawshell.py`: `COLLECT_INTERVAL=30s`, `HANG_WARN_SECS=120`, `HANG_CRIT_SECS=300`, `IO_DELTA_THRESHOLD=1MB`, `CTX_SWITCH_THRESHOLD=10`, `RESTART_SETTLE_WAIT=90s` (wait before escalating to LKG recovery), `RECOVERY_COOLDOWN=1800s` (30-min cooldown between recovery attempts), `IDENTITY_UNLOCK_TIMEOUT=600s` (10-min auto-relock).
