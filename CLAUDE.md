# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Guardian (ClawShell) is an external watchdog and data protection system for OpenClaw. It runs as a separate system user (`occlawshell`) to monitor OpenClaw's gateway process health, maintain atomic backups, and send independent alerts. The core invariants: OpenClaw stays online, no data leakage, no prompt injection exploitation, memory never lost, rollback to any LKG state, no zombie processes.

## Commands

### Run tests
```bash
python3 test_clawshell.py
```

### Deploy (requires sudo)
```bash
sudo bash deploy.sh
```

### Start/manage services
```bash
sudo systemctl enable --now oc-clawshell.service
sudo systemctl start oc-snapshot.timer oc-healthcheck.timer oc-lkg-promote.timer
```

### Monitor
```bash
journalctl -u oc-clawshell -f
tail -f /var/lib/occlawshell/audit/alerts.log
```

## Architecture: Three Pillars

### 1. WATCH — `clawshell.py`
Pure Python (stdlib only) watchdog daemon (~380 lines). Collects `/proc` data every 30s, checks HTTP health at `:18789/health`, and classifies process state via a 6-state machine: `UNKNOWN → HEALTHY → HEAVY_INFERENCE → POSSIBLE_HANG → CONFIRMED_HANG / ZOMBIE / DOWN`. Sends `sd_notify()` heartbeat every 1s. Calls `alert.sh` on state transitions.

### 2. VAULT — Snapshots & LKG
Atomic backups stored in `/var/lib/occlawshell/` (owned by `occlawshell`, mode 0700):
- `snapshots/` — 30-min interval backups (96 retained = 48 hours)
- `lkg/` — Last Known Good states (10 retained), promoted every 2 hours after safety gates
- `postmortem/` — Forensic data from crashes
- `audit/` — Alert, backup, and LKG logs

Key scripts: `snapshot-sqlite.sh` (VACUUM INTO), `snapshot-files.sh`, `promote-lkg.sh` (uptime + health gates), `rollback.sh` (human-triggered only), `postmortem-collect.sh`.

### 3. ALERT — `scripts/alert.sh`
Independent Telegram notification channel. Loads credentials via `grep` (not `source`) to prevent injection. Always logs locally to `audit/alerts.log`. Levels: INFO, WARNING, CRITICAL.

## Security Model — Three-User Isolation

```
yimeng       — Human admin, has sudo (controls everything)
ocagent      — Runs OpenClaw, no sudo (pure application user)
occlawshell  — Runs ClawShell, no sudo (nologin system user)
```

- **Three-user OS isolation:** `ocagent` (runs OpenClaw) and `occlawshell` (runs ClawShell) are separate unprivileged users — neither has sudo, they can't kill each other's processes or access each other's files. `yimeng` is the human admin with sudo for management only.
- **Immutability:** `chattr +i` on identity files (SOUL.md, AGENTS.md, USER.md)
- **systemd hardening:** `NoNewPrivileges=yes`, `ProtectSystem=strict`, `SystemCallFilter=@system-service`, `OOMScoreAdjust=-500`, `WatchdogSec=120`
- **Injection-safe scripts:** All shell scripts parse env vars with `grep` instead of `source`

## Key Design Constraints

- `clawshell.py` must have **zero external dependencies** — Python stdlib only
- Shell scripts must never use `source`/`eval` on config files — parse with `grep` to prevent injection
- `rollback.sh` is **human-triggered only** — never automated
- Snapshots use atomic operations: `VACUUM INTO` for SQLite, atomic rename for files
- All scripts must be idempotent

## File Layout

| File | Purpose |
|------|---------|
| `clawshell.py` | Core watchdog daemon with state machine |
| `test_clawshell.py` | Unit tests (unittest framework) |
| `deploy.sh` | Idempotent deployment orchestration |
| `scripts/*.sh` | Alert, health check, snapshot, LKG, rollback, postmortem |
| `systemd/` | Service and timer units for daemon, snapshots, health checks, LKG promotion |
| `logrotate/occlawshell` | Daily log rotation (30 days) |

## Configuration

Environment variables in `/var/lib/occlawshell/config/clawshell.env`:
- `GATEWAY_PORT` (default: 18789) — OpenClaw health endpoint port
- `CLAWSHELL_TG_TOKEN` / `CLAWSHELL_TG_CHAT` — Telegram bot credentials
- `CLAWSHELL_DEBUG` — Enable debug logging
- `MIN_UPTIME_SEC` (default: 1800) — Min uptime before LKG promotion
- `AGENT_USER` (default: ocagent) — User running OpenClaw
- `OPENCLAW_DIR` (default: /home/ocagent/.openclaw) — OpenClaw data directory

Key thresholds in `clawshell.py`: `COLLECT_INTERVAL=30s`, `HANG_WARN_SECS=120`, `HANG_CRIT_SECS=300`, `IO_DELTA_THRESHOLD=1MB`, `CTX_SWITCH_THRESHOLD=10`.
