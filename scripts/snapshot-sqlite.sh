#!/bin/bash
# ClawShell — Atomic SQLite Backup via VACUUM INTO
# Runs as: occlawshell (via systemd timer)
# Writes to: /var/lib/occlawshell/snapshots/
set -euo pipefail

# Read OPENCLAW_DIR from clawshell.env (grep-based, no shell eval)
ENV_FILE="/var/lib/occlawshell/config/clawshell.env"
OPENCLAW_DIR=$(grep '^OPENCLAW_DIR=' "${ENV_FILE}" 2>/dev/null | cut -d= -f2)
OPENCLAW_DIR="${OPENCLAW_DIR:-/home/ocagent/.openclaw}"
OPENCLAW_DIR=$(realpath -m "$OPENCLAW_DIR" 2>/dev/null)
if [[ ! "$OPENCLAW_DIR" =~ ^/home/ ]]; then
    echo "FATAL: Invalid OPENCLAW_DIR='${OPENCLAW_DIR}'" >&2; exit 1
fi

SRC="${OPENCLAW_DIR}/memory/main.sqlite"
DST_DIR="/var/lib/occlawshell/snapshots"
AUDIT_DIR="/var/lib/occlawshell/audit"
ALERT="/var/lib/occlawshell/bin/alert.sh"
TS=$(date +%Y%m%d-%H%M%S)
DST="${DST_DIR}/main-${TS}.sqlite"
DST_TMP="${DST}.tmp"

log() { echo "[$(date -Iseconds)] $*" >> "${AUDIT_DIR}/backup.log"; }

# Preflight: vault disk quota
QUOTA_CHECK="/var/lib/occlawshell/bin/quota-check.sh"
if [[ -x "$QUOTA_CHECK" ]] && ! "$QUOTA_CHECK"; then
    log "SKIP: Vault over quota, snapshot deferred"
    exit 0
fi

# Preflight: I/O pressure
IO_CHECK="/var/lib/occlawshell/bin/io-pressure-check.sh"
if [[ -x "$IO_CHECK" ]] && ! "$IO_CHECK"; then
    log "SKIP: I/O pressure too high, snapshot deferred"
    exit 0
fi

# Preflight: source must exist and be readable
if [[ ! -r "$SRC" ]]; then
    log "FAIL: Source $SRC not readable"
    "$ALERT" CRITICAL "SQLite backup failed: source not readable" 2>/dev/null || true
    exit 1
fi

# Step 1: VACUUM INTO — atomic, defragmented copy
if ! sqlite3 "$SRC" "VACUUM INTO '${DST_TMP}';" 2>>"${AUDIT_DIR}/backup.log"; then
    log "FAIL: VACUUM INTO failed"
    "$ALERT" CRITICAL "SQLite VACUUM INTO failed" 2>/dev/null || true
    rm -f "$DST_TMP"
    exit 1
fi

# Step 2: Integrity verification of the BACKUP
INTEGRITY=$(sqlite3 "$DST_TMP" "PRAGMA integrity_check;" 2>&1)
if [[ "$INTEGRITY" != "ok" ]]; then
    log "FAIL: Integrity check failed — $INTEGRITY"
    "$ALERT" CRITICAL "SQLite backup integrity FAILED: $INTEGRITY" 2>/dev/null || true
    rm -f "$DST_TMP"
    exit 1
fi

# Step 3: Verify row count sanity
ROW_COUNT=$(sqlite3 "$DST_TMP" "SELECT COUNT(*) FROM chunks;" 2>/dev/null || echo "0")
if [[ "$ROW_COUNT" -eq 0 ]]; then
    log "WARNING: Backup has 0 rows in chunks table (may be OK for fresh install)"
fi

# Step 3b: Row count regression detection — alert if >50% drop from previous snapshot
PREV_SQL=$(ls -1t "${DST_DIR}"/main-*.sqlite 2>/dev/null | head -1 || true)
if [[ -n "$PREV_SQL" && -f "$PREV_SQL" ]]; then
    PREV_COUNT=$(sqlite3 "$PREV_SQL" "SELECT COUNT(*) FROM chunks;" 2>/dev/null || echo "0")
    if [[ "$PREV_COUNT" -gt 0 && "$ROW_COUNT" -gt 0 ]]; then
        DROP_PCT=$(( (PREV_COUNT - ROW_COUNT) * 100 / PREV_COUNT ))
        if [[ "$DROP_PCT" -gt 50 ]]; then
            log "WARNING: Row count dropped ${DROP_PCT}% (${PREV_COUNT} -> ${ROW_COUNT})"
            "$ALERT" WARNING "SQLite row count dropped ${DROP_PCT}% (${PREV_COUNT} -> ${ROW_COUNT}). Possible data loss." 2>/dev/null || true
        fi
    fi
fi

# Step 4: Atomic rename
mv "$DST_TMP" "$DST"

# Step 5: Record SHA-256
SHA=$(sha256sum "$DST" | cut -d' ' -f1)
echo "${TS} ${SHA} ${DST}" >> "${AUDIT_DIR}/sqlite-hashes.log"

# Step 6: Log success
SIZE=$(du -h "$DST" | cut -f1)
log "OK: ${DST} (${SHA:0:16}) size=${SIZE} rows=${ROW_COUNT}"

# Step 7: Prune old snapshots (keep 96 = 48 hours at 30-min intervals)
cd "$DST_DIR"
ls -1t main-*.sqlite 2>/dev/null | tail -n +97 | xargs -r rm -f || true

exit 0
