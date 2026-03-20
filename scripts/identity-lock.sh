#!/bin/bash
# ClawShell — Identity File Lock/Unlock
# Usage: identity-lock.sh lock|unlock
# Only operates on the 3 identity files (SOUL.md, AGENTS.md, USER.md).
# Must run as root (via sudoers whitelist).
set -uo pipefail

ACTION="${1:-}"
ENV_FILE="/var/lib/occlawshell/config/clawshell.env"
AUDIT_LOG="/var/lib/occlawshell/audit/alerts.log"

OPENCLAW_DIR=$(grep -oP '^OPENCLAW_DIR=\K.+' "$ENV_FILE" 2>/dev/null || true)
OPENCLAW_DIR="${OPENCLAW_DIR:-/home/ocagent/.openclaw}"
WORKSPACE="${OPENCLAW_DIR}/workspace"

IDENTITY_FILES=(
    "${WORKSPACE}/SOUL.md"
    "${WORKSPACE}/AGENTS.md"
    "${WORKSPACE}/USER.md"
)

log() { echo "[$(date -Iseconds)] IDENTITY: $*" >> "$AUDIT_LOG" 2>/dev/null || true; }

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: Must run as root (via sudo)" >&2
    exit 1
fi

if [[ "$ACTION" != "lock" && "$ACTION" != "unlock" ]]; then
    echo "Usage: identity-lock.sh lock|unlock" >&2
    exit 1
fi

for f in "${IDENTITY_FILES[@]}"; do
    if [[ ! -e "$f" ]]; then
        echo "  SKIP: $f (not found)"
        continue
    fi
    if [[ "$ACTION" == "lock" ]]; then
        chattr +i "$f" 2>/dev/null && echo "  LOCKED: $(basename "$f")" || echo "  FAIL: $(basename "$f")"
    else
        chattr -i "$f" 2>/dev/null && echo "  UNLOCKED: $(basename "$f")" || echo "  FAIL: $(basename "$f")"
    fi
done

log "${ACTION^^}ED identity files ($(basename "${IDENTITY_FILES[0]}"), $(basename "${IDENTITY_FILES[1]}"), $(basename "${IDENTITY_FILES[2]}"))"
exit 0
