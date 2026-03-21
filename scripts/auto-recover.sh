#!/bin/bash
# ClawShell — Auto-Recovery from LKG
# Called by: clawshell.py when gateway restart fails (likely data corruption)
# Runs as: root (via sudo, scoped by /etc/sudoers.d/occlawshell)
#
# Steps:
#   1. Validate LKG integrity
#   2. Stop gateway
#   3. Emergency snapshot of current (corrupt) state → postmortem/
#   4. Restore SQLite + Memory from LKG
#   5. Restart gateway
set -uo pipefail

ENV_FILE="/var/lib/occlawshell/config/clawshell.env"
ALERT="/var/lib/occlawshell/bin/alert.sh"
AUDIT_DIR="/var/lib/occlawshell/audit"
LKG_DIR="/var/lib/occlawshell/lkg"
LKG_CURRENT="${LKG_DIR}/current"

AGENT_USER=$(grep '^AGENT_USER=' "${ENV_FILE}" 2>/dev/null | cut -d= -f2)
AGENT_USER="${AGENT_USER:-ocagent}"
OPENCLAW_DIR=$(grep '^OPENCLAW_DIR=' "${ENV_FILE}" 2>/dev/null | cut -d= -f2)
OPENCLAW_DIR="${OPENCLAW_DIR:-/home/${AGENT_USER}/.openclaw}"

# Validate AGENT_USER: must be a valid Unix username
if [[ ! "$AGENT_USER" =~ ^[a-z_][a-z0-9_-]*$ ]]; then
    echo "FATAL: Invalid AGENT_USER='${AGENT_USER}'" >&2
    exit 1
fi
# Validate OPENCLAW_DIR: must be absolute, no '..' components, must exist
OPENCLAW_DIR=$(realpath -m "$OPENCLAW_DIR" 2>/dev/null)
if [[ ! "$OPENCLAW_DIR" =~ ^/home/ || "$OPENCLAW_DIR" == *..* ]]; then
    echo "FATAL: Invalid OPENCLAW_DIR='${OPENCLAW_DIR}'" >&2
    exit 1
fi

GATEWAY_SERVICE="openclaw-gateway.service"
TS=$(date +%Y%m%d-%H%M%S)

log() {
    echo "[$(date -Iseconds)] AUTO-RECOVER: $*" >> "${AUDIT_DIR}/alerts.log" 2>/dev/null
    echo "$*" >&2
}

# ── Pre-check 1: LKG exists ──
LKG=$(readlink -f "$LKG_CURRENT" 2>/dev/null)
if [[ -z "$LKG" || ! -d "$LKG" ]]; then
    log "FAIL: No LKG available at $LKG_CURRENT"
    exit 1
fi

# ── Pre-check 2: LKG SQLite is valid ──
if [[ ! -f "$LKG/main.sqlite" ]]; then
    log "FAIL: No SQLite in LKG: $LKG/main.sqlite"
    exit 1
fi

INTEGRITY=$(sqlite3 "$LKG/main.sqlite" "PRAGMA integrity_check;" 2>&1)
if [[ "$INTEGRITY" != "ok" ]]; then
    log "FAIL: LKG SQLite integrity check failed: $INTEGRITY"
    exit 1
fi

LKG_ROWS=$(sqlite3 "$LKG/main.sqlite" "SELECT COUNT(*) FROM chunks;" 2>/dev/null || echo "0")
log "LKG validated: $LKG (rows=${LKG_ROWS})"

# ── Step 1: Stop gateway ──
systemctl stop "$GATEWAY_SERVICE" 2>/dev/null || true
sleep 1

# ── Step 2: Emergency snapshot of current state (preserve evidence) ──
EMERGENCY_DIR="/var/lib/occlawshell/postmortem/${TS}-pre-auto-recover"
mkdir -p "$EMERGENCY_DIR"
cp -p "${OPENCLAW_DIR}/memory/main.sqlite" "$EMERGENCY_DIR/" 2>/dev/null || true
cp -p "${OPENCLAW_DIR}/workspace/MEMORY.md" "$EMERGENCY_DIR/" 2>/dev/null || true
cp -rp "${OPENCLAW_DIR}/workspace/memory/" "$EMERGENCY_DIR/memory/" 2>/dev/null || true
chown -R occlawshell:occlawshell "$EMERGENCY_DIR"
log "Emergency snapshot saved: $EMERGENCY_DIR"

# ── Step 3: Restore SQLite ──
if [[ -f "$LKG/main.sqlite" ]]; then
    cp -p "$LKG/main.sqlite" "${OPENCLAW_DIR}/memory/main.sqlite"
    chown "${AGENT_USER}:${AGENT_USER}" "${OPENCLAW_DIR}/memory/main.sqlite"
    chmod 600 "${OPENCLAW_DIR}/memory/main.sqlite"
    log "SQLite restored from LKG"
fi

# ── Step 4: Restore MEMORY.md ──
if [[ -f "$LKG/MEMORY.md" ]]; then
    cp -p "$LKG/MEMORY.md" "${OPENCLAW_DIR}/workspace/MEMORY.md"
    chown "${AGENT_USER}:${AGENT_USER}" "${OPENCLAW_DIR}/workspace/MEMORY.md"
    log "MEMORY.md restored from LKG"
fi

# ── Step 5: Restore memory directory ──
if [[ -d "$LKG/memory" ]]; then
    rsync -a --delete "$LKG/memory/" "${OPENCLAW_DIR}/workspace/memory/"
    chown -R "${AGENT_USER}:${AGENT_USER}" "${OPENCLAW_DIR}/workspace/memory/"
    log "memory/ restored from LKG"
fi

# ── Step 6: Fix config compatibility after LKG restore ──
# LKG config may be from an older OpenClaw version; doctor --fix applies safe migrations
OC_BIN=$(grep '^OPENCLAW_BIN=' "${ENV_FILE}" 2>/dev/null | cut -d= -f2)
if [[ -n "$OC_BIN" && -x "$OC_BIN" ]]; then
    OC_NODE_DIR=$(dirname "$OC_BIN")
    sudo -u "${AGENT_USER}" env "PATH=${OC_NODE_DIR}:${PATH}" \
        openclaw doctor --fix --non-interactive 2>/dev/null && \
        log "openclaw doctor --fix applied" || \
        log "WARNING: openclaw doctor --fix failed (non-fatal)"
fi

# ── Step 7: Restart gateway ──
log "Starting gateway..."
systemctl start "$GATEWAY_SERVICE"
sleep 3

STATUS=$(systemctl is-active "$GATEWAY_SERVICE" 2>/dev/null)
if [[ "$STATUS" == "active" ]]; then
    log "OK: Gateway recovered from LKG ($LKG). Status: $STATUS"
    "$ALERT" WARNING "Auto-recovered from LKG. Data rolled back to: $(basename "$LKG"). Corrupt state saved: $EMERGENCY_DIR" 2>/dev/null || true
    exit 0
else
    log "FAIL: Gateway still not active after LKG restore. Status: $STATUS"
    "$ALERT" CRITICAL "Auto-recovery FAILED. Gateway status: $STATUS. Manual intervention required." 2>/dev/null || true
    exit 1
fi
