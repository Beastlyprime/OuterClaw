# OpenClaw Guardian Final Implementation Specification (v4.0)

This document defines the hardened implementation for the OpenClaw Guardian system. It focuses on the "Time Machine" state recovery, Last Known Good (LKG) logic, resource-aware zombie detection, and hardened process isolation.

## 1. Security Architecture & Isolation (Hardened Aegis)

Guardian operates under a strict "least privilege" model where the system controller (Guardian) and the application (OpenClaw) are cryptographically and administratively separated.

### 1.1 User & Permission Matrix
| Entity | User | Role | Permissions |
| :--- | :--- | :--- | :--- |
| **OpenClaw** | `openclaw` | Application | Read/Write to workspace; No access to backups. |
| **Guardian** | `oc-guardian` | Watchdog | Owns `/var/lib/openclaw/backups`; Read-only to workspace (except reset). |

### 1.2 Linux Filesystem Hardening
1.  **Backup Directory**: `/var/lib/openclaw/backups`
    -   Owner: `oc-guardian:oc-guardian`
    -   Mode: `700` (OpenClaw cannot list or read).
2.  **Immutable Backups**: Guardian uses `chattr +i` on verified LKG snapshots to prevent accidental deletion even if the `oc-guardian` account is compromised (requires root to unset).

### 1.3 Sudoers Configuration (`/etc/sudoers.d/oc-guardian`)
Guardian needs specific, passwordless sudo access to manage the OpenClaw service:
```sudoers
# Allow oc-guardian to manage the OpenClaw service
oc-guardian ALL=(root) NOPASSWD: /usr/bin/systemctl restart openclaw.service
oc-guardian ALL=(root) NOPASSWD: /usr/bin/systemctl stop openclaw.service
oc-guardian ALL=(root) NOPASSWD: /usr/bin/systemctl start openclaw.service
# Allow oc-guardian to kill hung processes of the openclaw user
oc-guardian ALL=(root) NOPASSWD: /usr/bin/pkill -u openclaw
```

---

## 2. Time Machine: State Snapshotting

Snapshotting captures Code, Config, and SQLite Session state atomically.

### 2.1 The 'VACUUM INTO' Strategy
To ensure zero-corruption backups of the live SQLite database:
1.  **Integrity Check**: Validate DB before backup.
2.  **Atomic Snapshot**: Use `VACUUM INTO`.
3.  **Hardening**: Set the backup file to read-only for anyone but Guardian.

**Snapshot Script Snippet (`oc-vault-snapshot.sh`):**
```bash
#!/bin/bash
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
BACKUP_DIR="/var/lib/openclaw/backups/$TIMESTAMP"
mkdir -p "$BACKUP_DIR"

# 1. Capture Code & Config (Git)
cd /home/yimeng/.openclaw/workspace
git add .
git commit -m "Snapshot $TIMESTAMP" || echo "No changes to code."

# 2. Capture DB (SQLite)
DB_PATH="/home/yimeng/.openclaw/workspace/main.db"
if sqlite3 "$DB_PATH" "PRAGMA integrity_check;" | grep -q "ok"; then
    sqlite3 "$DB_PATH" "VACUUM INTO '$BACKUP_DIR/main.db';"
    # Store Git Hash for synchronization
    git rev-parse HEAD > "$BACKUP_DIR/git_hash.txt"
else
    echo "DB Integrity Check Failed! Snapshot aborted."
    exit 1
fi
```

---

## 3. LKG (Last Known Good) Logic

We avoid backing up broken states by using a "Verification Window."

### 3.1 Verification Criteria
A state is promoted to **LKG** only if:
1.  OpenClaw has been running for >15 minutes.
2.  The `oc-sentry` "Health Check" endpoint (internal API) returns 200 OK.
3.  No "Critical" level errors found in logs since start.

### 3.2 Git Hooks for LKG Protection
We use a post-commit hook to prevent manual `git reset` or `force-push` from destroying LKG history.

---

## 4. Anti-Lock / Zombie Detection (Resource Telemetry)

Guardian must differentiate between **Heavy Inference** (High CPU/GPU) and **Infinite Loops** (High CPU/No Progress).

### 4.1 Detection Algorithm
Guardian polls `/proc/[pid]/stat` and `/proc/[pid]/io`.
- **Condition RED (Deadlock)**:
  - `CPU_Usage > 90%` for > 180s.
  - AND `read_bytes + write_bytes < 10KB` in the same window.
  - AND `Process_Name` is NOT in `AI_WHITE_LIST` (e.g., llama-server).
- **Condition YELLOW (Stall)**:
  - Heartbeat file `~/.openclaw/.heartbeat` older than 300s.

**Telemetry Implementation:**
```python
def is_zombie(pid):
    # Check I/O movement
    io_start = get_io_stats(pid)
    time.sleep(30)
    io_end = get_io_stats(pid)
    
    if (io_end['write_bytes'] - io_start['write_bytes']) < 1024:
        # No significant disk activity, check if it's an AI model
        if not is_ai_workload(pid):
            return True # Likely infinite loop in Python logic
    return False
```

---

## 5. Systemd Configuration

### 5.1 `oc-guardian.service` (The Watcher)
Runs as `oc-guardian` user.
```ini
[Unit]
Description=OpenClaw Guardian Sentry
After=network.target

[Service]
User=oc-guardian
ExecStart=/usr/bin/python3 /opt/openclaw/guardian/sentry.py
Restart=always
RestartSec=5
# Security Hardening
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/var/lib/openclaw/backups /home/yimeng/.openclaw/workspace/.heartbeat

[Install]
WantedBy=multi-user.target
```

### 5.2 `openclaw.service` (The Managed)
Runs as `openclaw` user.
```ini
[Unit]
Description=OpenClaw Main Agent
PartOf=oc-guardian.service

[Service]
User=openclaw
WorkingDirectory=/home/yimeng/.openclaw/workspace
ExecStart=/home/yimeng/.openclaw/venv/bin/openclaw start
# Restart is handled by Guardian, not systemd directly, to allow for LKG Rollback
Restart=no
```

---

## 6. Recovery Workflow (The Time Machine Trip)

1.  **Trigger**: Sentry detects 3 consecutive failed starts or a "Zombie" state.
2.  **Notification**: Send "CRITICAL: Triggering Rollback" to Telegram.
3.  **Rollback**:
    -   `systemctl stop openclaw`
    -   `git reset --hard $(cat /var/lib/openclaw/backups/latest_lkg/git_hash.txt)`
    -   `cp /var/lib/openclaw/backups/latest_lkg/main.db /home/yimeng/.openclaw/workspace/main.db`
    -   `systemctl start openclaw`
4.  **Audit**: Log the rollback event for human review.

*Specification finalized by Senior Security Engineer (2026-02-17)*
