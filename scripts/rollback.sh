#!/bin/bash
# ClawShell — Rollback to LKG State
# DESTRUCTIVE — Must be run by human admin, NOT by the agent
# Usage: sudo /var/lib/occlawshell/bin/rollback.sh [/path/to/lkg]
set -euo pipefail

# Read AGENT_USER and OPENCLAW_DIR from clawshell.env (grep-based, no shell eval)
ENV_FILE="/var/lib/occlawshell/config/clawshell.env"
AGENT_USER=$(grep '^AGENT_USER=' "${ENV_FILE}" 2>/dev/null | cut -d= -f2)
AGENT_USER="${AGENT_USER:-ocagent}"
OPENCLAW_DIR=$(grep '^OPENCLAW_DIR=' "${ENV_FILE}" 2>/dev/null | cut -d= -f2)
OPENCLAW_DIR="${OPENCLAW_DIR:-/home/ocagent/.openclaw}"

# Validate AGENT_USER and OPENCLAW_DIR to prevent injection (script runs as root)
if [[ ! "$AGENT_USER" =~ ^[a-z_][a-z0-9_-]*$ ]]; then
    echo "FATAL: Invalid AGENT_USER='${AGENT_USER}'" >&2; exit 1
fi
OPENCLAW_DIR=$(realpath -m "$OPENCLAW_DIR" 2>/dev/null)
if [[ ! "$OPENCLAW_DIR" =~ ^/home/ || "$OPENCLAW_DIR" == *..* ]]; then
    echo "FATAL: Invalid OPENCLAW_DIR='${OPENCLAW_DIR}'" >&2; exit 1
fi

LKG="${1:-/var/lib/occlawshell/lkg/current}"

# Resolve symlink
LKG=$(readlink -f "$LKG")

if [[ ! -d "$LKG" ]]; then
    echo "ERROR: LKG not found at $LKG"
    echo "Available LKGs:"
    ls -1dt /var/lib/occlawshell/lkg/lkg-* 2>/dev/null || echo "  (none)"
    exit 1
fi

echo "╔══════════════════════════════════════════════════════════╗"
echo "║               ROLLBACK PLAN                              ║"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""
echo "Source LKG: $LKG"
echo ""
echo "Will restore:"
[[ -f "$LKG/main.sqlite" ]] && echo "  ✓ SQLite sessions:  $(du -h "$LKG/main.sqlite" | cut -f1)"
[[ -f "$LKG/MEMORY.md" ]]   && echo "  ✓ MEMORY.md:        $(du -h "$LKG/MEMORY.md" | cut -f1)"
[[ -d "$LKG/memory" ]]      && echo "  ✓ memory/ dir:      $(ls "$LKG/memory/" 2>/dev/null | wc -l) files"
[[ -d "$LKG/config" ]]      && echo "  ✓ config/:          $(ls "$LKG/config/" 2>/dev/null | wc -l) files"
echo ""
echo "Will OVERWRITE:"
echo "  → ${OPENCLAW_DIR}/memory/main.sqlite"
echo "  → ${OPENCLAW_DIR}/workspace/MEMORY.md"
echo "  → ${OPENCLAW_DIR}/workspace/memory/"
echo ""
echo "⚠️  This will STOP the OpenClaw gateway during rollback."
echo ""
read -p "Type 'ROLLBACK' to confirm: " CONFIRM
if [[ "$CONFIRM" != "ROLLBACK" ]]; then
    echo "Aborted."
    exit 0
fi

echo ""
echo "Stopping gateway..."
systemctl stop openclaw-gateway.service 2>/dev/null || true
sleep 2

# Pre-rollback: take emergency snapshot of CURRENT state
EMERGENCY_TS=$(date +%Y%m%d-%H%M%S)
EMERGENCY_DIR="/var/lib/occlawshell/postmortem/${EMERGENCY_TS}-pre-rollback"
mkdir -p "$EMERGENCY_DIR"
cp -p "${OPENCLAW_DIR}/memory/main.sqlite" "$EMERGENCY_DIR/" 2>/dev/null || true
cp -p "${OPENCLAW_DIR}/workspace/MEMORY.md" "$EMERGENCY_DIR/" 2>/dev/null || true
cp -rp "${OPENCLAW_DIR}/workspace/memory/" "$EMERGENCY_DIR/memory/" 2>/dev/null || true
chown -R occlawshell:occlawshell "$EMERGENCY_DIR"
echo "Pre-rollback snapshot saved: $EMERGENCY_DIR"

# Restore SQLite
if [[ -f "$LKG/main.sqlite" ]]; then
    cp -p "$LKG/main.sqlite" "${OPENCLAW_DIR}/memory/main.sqlite"
    chown "${AGENT_USER}:${AGENT_USER}" "${OPENCLAW_DIR}/memory/main.sqlite"
    chmod 600 "${OPENCLAW_DIR}/memory/main.sqlite"
    echo "  ✓ SQLite restored"
fi

# Restore MEMORY.md
if [[ -f "$LKG/MEMORY.md" ]]; then
    cp -p "$LKG/MEMORY.md" "${OPENCLAW_DIR}/workspace/MEMORY.md"
    chown "${AGENT_USER}:${AGENT_USER}" "${OPENCLAW_DIR}/workspace/MEMORY.md"
    echo "  ✓ MEMORY.md restored"
fi

# Restore memory directory
if [[ -d "$LKG/memory" ]]; then
    rsync -a --delete "$LKG/memory/" "${OPENCLAW_DIR}/workspace/memory/"
    chown -R "${AGENT_USER}:${AGENT_USER}" "${OPENCLAW_DIR}/workspace/memory/"
    echo "  ✓ memory/ restored"
fi

# Restore config (optional — prompted)
if [[ -f "$LKG/config/openclaw.json" ]]; then
    read -p "Also restore openclaw.json? (y/N): " RESTORE_CONFIG
    if [[ "$RESTORE_CONFIG" == "y" || "$RESTORE_CONFIG" == "Y" ]]; then
        cp -p "$LKG/config/openclaw.json" "${OPENCLAW_DIR}/openclaw.json"
        chown "${AGENT_USER}:${AGENT_USER}" "${OPENCLAW_DIR}/openclaw.json"
        chmod 600 "${OPENCLAW_DIR}/openclaw.json"
        echo "  ✓ openclaw.json restored"
    fi
fi

# Restart gateway
echo ""
echo "Starting gateway..."
systemctl start openclaw-gateway.service
sleep 3

STATUS=$(systemctl is-active openclaw-gateway.service 2>/dev/null)
echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "║               ROLLBACK COMPLETE                          ║"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""
echo "  Restored from: $LKG"
echo "  Gateway status: $STATUS"
echo "  Pre-rollback backup: $EMERGENCY_DIR"
echo ""

# Log to ClawShell audit
echo "[$(date -Iseconds)] ROLLBACK from $LKG by $(whoami)" \
    >> /var/lib/occlawshell/audit/lkg.log

/var/lib/occlawshell/bin/alert.sh WARNING \
    "ROLLBACK executed from $LKG. Gateway status: $STATUS" 2>/dev/null || true
