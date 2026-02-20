#!/bin/bash
# ClawShell — Gateway Pre-Start Validation
# Called by: openclaw-gateway.service ExecStartPre
# Runs as: ocagent (same as gateway service)
# Prevents blind restart with corrupt or missing data
# Output goes to journal via systemd
set -uo pipefail

OPENCLAW_DIR="${HOME:-/home/ocagent}/.openclaw"
SQLITE="${OPENCLAW_DIR}/memory/main.sqlite"
ALERT="/var/lib/occlawshell/bin/alert.sh"
FAILED=0

log() { echo "PRE-START: $*" >&2; }

# ── Check 1: SQLite exists and is readable ──
if [[ ! -r "$SQLITE" ]]; then
    log "FAIL: SQLite not readable: $SQLITE"
    FAILED=1
else
    # ── Check 2: SQLite structural integrity ──
    INTEGRITY=$(sqlite3 "$SQLITE" "PRAGMA integrity_check;" 2>&1)
    if [[ "$INTEGRITY" != "ok" ]]; then
        log "FAIL: SQLite integrity check failed: $INTEGRITY"
        FAILED=1
    fi

    # ── Check 3: SQLite has data ──
    ROW_COUNT=$(sqlite3 "$SQLITE" "SELECT COUNT(*) FROM chunks;" 2>/dev/null || echo "-1")
    if [[ "$ROW_COUNT" -eq -1 ]]; then
        log "FAIL: Cannot query chunks table in $SQLITE"
        FAILED=1
    elif [[ "$ROW_COUNT" -eq 0 ]]; then
        log "WARNING: SQLite has 0 rows (may be fresh install)"
    fi
fi

# ── Check 4: Workspace directory exists ──
if [[ ! -d "${OPENCLAW_DIR}/workspace" ]]; then
    log "FAIL: Workspace directory missing: ${OPENCLAW_DIR}/workspace"
    FAILED=1
fi

# ── Result ──
if [[ "$FAILED" -ne 0 ]]; then
    log "BLOCKED: Gateway start prevented due to validation failures"
    "$ALERT" CRITICAL "Gateway start BLOCKED: pre-start validation failed. Check: journalctl -u openclaw-gateway" 2>/dev/null || true
    exit 1
fi

log "OK: Pre-start validation passed (rows=${ROW_COUNT:-?})"
exit 0
