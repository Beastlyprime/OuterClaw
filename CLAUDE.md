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
Pure Python (stdlib only) watchdog daemon (~460 lines). Collects `/proc` data every 30s, checks HTTP health at `:18789/health`, and classifies process state via a 6-state machine: `UNKNOWN → HEALTHY → HEAVY_INFERENCE → POSSIBLE_HANG → CONFIRMED_HANG / ZOMBIE / DOWN`. Sends `sd_notify()` heartbeat every 1s. Calls `alert.sh` on state transitions. Includes two-step auto-recovery: on `CONFIRMED_HANG`, restarts gateway; if still unhealthy after 90s, triggers LKG recovery via `auto-recover.sh` (30-min cooldown between attempts).

### 2. VAULT — Snapshots & LKG
Atomic backups stored in `/var/lib/occlawshell/` (owned by `occlawshell`, mode 0700):
- `snapshots/` — 30-min interval backups (96 retained = 48 hours)
- `lkg/` — Last Known Good states (10 retained), promoted every 2 hours after safety gates
- `postmortem/` — Forensic data from crashes
- `audit/` — Alert, backup, and LKG logs

Key scripts: `snapshot-sqlite.sh` (VACUUM INTO), `snapshot-files.sh`, `promote-lkg.sh` (uptime + health gates), `rollback.sh` (human-triggered only), `postmortem-collect.sh`, `auto-recover.sh` (automated LKG recovery on persistent failures), `pre-start-check.sh` (blocks gateway startup if data corrupted).

### 3. ALERT — `scripts/alert.sh`
Independent Telegram notification channel. Loads credentials via `grep` (not `source`) to prevent injection. Always logs locally to `audit/alerts.log`. Levels: INFO, WARNING, CRITICAL.

## Security Model — Three-User Isolation

```
yimeng       — Human admin, has sudo (controls everything)
ocagent      — Runs OpenClaw, no sudo (pure application user)
occlawshell  — Runs ClawShell, limited sudo (nologin system user)
```

- **Three-user OS isolation:** `ocagent` (runs OpenClaw, no sudo) and `occlawshell` (runs ClawShell, limited sudo) are separate unprivileged users — they can't kill each other's processes or access each other's files. `occlawshell` has narrowly scoped sudoers entries for `systemctl restart openclaw-gateway.service` and `auto-recover.sh` only. `yimeng` is the human admin with full sudo for management.
- **Immutability:** `chattr +i` on identity files (SOUL.md, AGENTS.md, USER.md)
- **systemd hardening:** `NoNewPrivileges=yes`, `ProtectSystem=strict`, `SystemCallFilter=@system-service`, `OOMScoreAdjust=-500`, `WatchdogSec=120`
- **Injection-safe scripts:** All shell scripts parse env vars with `grep` instead of `source`

## Key Design Constraints

- `clawshell.py` must have **zero external dependencies** — Python stdlib only
- Shell scripts must never use `source`/`eval` on config files — parse with `grep` to prevent injection
- `rollback.sh` is **human-triggered only** — never automated; `auto-recover.sh` is the separate automated recovery path (triggered by clawshell.py, limited scope, with cooldown)
- Snapshots use atomic operations: `VACUUM INTO` for SQLite, atomic rename for files
- All scripts must be idempotent

## File Layout

| File | Purpose |
|------|---------|
| `clawshell.py` | Core watchdog daemon with state machine and auto-recovery |
| `test_clawshell.py` | Unit tests (unittest framework) |
| `deploy.sh` | Idempotent deployment orchestration |
| `scripts/alert.sh` | Telegram + local audit log notifications |
| `scripts/healthcheck.sh` | 4-factor health check (service state, HTTP, memory, restarts) |
| `scripts/snapshot-sqlite.sh` | Atomic SQLite backup (VACUUM INTO + integrity check) |
| `scripts/snapshot-files.sh` | MEMORY.md, memory/, config, git bundle backup |
| `scripts/promote-lkg.sh` | LKG promotion with safety gates (uptime + health + row count) |
| `scripts/rollback.sh` | Human-triggered destructive rollback with confirmation |
| `scripts/postmortem-collect.sh` | Crash forensics (14 /proc categories + dmesg + coredump) |
| `scripts/auto-recover.sh` | Automated LKG recovery triggered by clawshell.py |
| `scripts/pre-start-check.sh` | Blocks gateway start if SQLite missing/corrupted |
| `systemd/oc-clawshell.service` | Watchdog daemon (Type=notify, WatchdogSec=120, OOMScoreAdjust=-500) |
| `systemd/oc-snapshot.timer/service` | 30-min snapshot trigger (sqlite + files) |
| `systemd/oc-healthcheck.timer/service` | 2-min health check trigger |
| `systemd/oc-lkg-promote.timer/service` | 2-hour LKG promotion trigger |
| `systemd/openclaw-gateway.service` | Gateway service (User=ocagent, pre-start check, postmortem on crash) |
| `logrotate/occlawshell` | Daily log rotation (30 days) |

## Configuration

Environment variables in `/var/lib/occlawshell/config/clawshell.env`:
- `GATEWAY_PORT` (default: 18789) — OpenClaw health endpoint port
- `CLAWSHELL_TG_TOKEN` / `CLAWSHELL_TG_CHAT` — Telegram bot credentials
- `CLAWSHELL_DEBUG` — Enable debug logging
- `MIN_UPTIME_SEC` (default: 1800) — Min uptime before LKG promotion
- `AGENT_USER` (default: ocagent) — User running OpenClaw
- `OPENCLAW_DIR` (default: /home/ocagent/.openclaw) — OpenClaw data directory

Key thresholds in `clawshell.py`: `COLLECT_INTERVAL=30s`, `HANG_WARN_SECS=120`, `HANG_CRIT_SECS=300`, `IO_DELTA_THRESHOLD=1MB`, `CTX_SWITCH_THRESHOLD=10`, `RESTART_SETTLE_WAIT=90s` (wait before escalating to LKG recovery), `RECOVERY_COOLDOWN=1800s` (30-min cooldown between recovery attempts).
