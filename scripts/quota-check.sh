#!/bin/bash
# ClawShell — Vault Disk Quota Check
# Usage: quota-check.sh [MAX_MB]
# Exit 0 = OK to write, Exit 1 = over quota (after emergency prune attempt)
# Called by snapshot scripts before writing.
set -uo pipefail

VAULT="/var/lib/occlawshell"
AUDIT_DIR="${VAULT}/audit"
ALERT="${VAULT}/bin/alert.sh"
ENV_FILE="${VAULT}/config/clawshell.env"

MAX_VAULT_MB="${1:-}"
if [[ -z "$MAX_VAULT_MB" ]]; then
    MAX_VAULT_MB=$(grep '^MAX_VAULT_MB=' "${ENV_FILE}" 2>/dev/null | cut -d= -f2)
    MAX_VAULT_MB="${MAX_VAULT_MB:-2048}"
fi

log() { echo "[$(date -Iseconds)] QUOTA: $*" >> "${AUDIT_DIR}/backup.log"; }

CURRENT_MB=$(du -sm "$VAULT" 2>/dev/null | cut -f1)

if [[ "$CURRENT_MB" -le "$MAX_VAULT_MB" ]]; then
    exit 0
fi

log "WARNING: Vault at ${CURRENT_MB}MB / ${MAX_VAULT_MB}MB — attempting emergency prune"

# Emergency prune: reduce snapshot retention from 96 to 48
cd "${VAULT}/snapshots" 2>/dev/null || true
ls -1t main-*.sqlite 2>/dev/null | tail -n +49 | xargs -r rm -f || true
ls -1dt files-* 2>/dev/null | tail -n +49 | xargs -r rm -rf || true

# Prune postmortems to 10
cd "${VAULT}/postmortem" 2>/dev/null || true
ls -1dt */ 2>/dev/null | tail -n +11 | xargs -r rm -rf || true

# Prune LKGs to 5
cd "${VAULT}/lkg" 2>/dev/null || true
ls -1dt lkg-* 2>/dev/null | tail -n +6 | xargs -r rm -rf || true

# Re-check
CURRENT_MB=$(du -sm "$VAULT" 2>/dev/null | cut -f1)
if [[ "$CURRENT_MB" -le "$MAX_VAULT_MB" ]]; then
    log "OK: Emergency prune freed space, now ${CURRENT_MB}MB / ${MAX_VAULT_MB}MB"
    exit 0
fi

log "FAIL: Vault still at ${CURRENT_MB}MB after emergency prune (limit: ${MAX_VAULT_MB}MB)"
"$ALERT" WARNING "Vault disk quota exceeded: ${CURRENT_MB}MB / ${MAX_VAULT_MB}MB. Snapshot deferred." 2>/dev/null || true
exit 1
