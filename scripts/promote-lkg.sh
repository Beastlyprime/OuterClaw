#!/bin/bash
# ClawShell — Promote Latest Snapshot to LKG
# Runs as: occlawshell (manual or triggered)
set -euo pipefail

LKG_DIR="/var/lib/occlawshell/lkg"
SNAP_DIR="/var/lib/occlawshell/snapshots"
AUDIT_DIR="/var/lib/occlawshell/audit"
ALERT="/var/lib/occlawshell/bin/alert.sh"
TS=$(date +%Y%m%d-%H%M%S)

MIN_UPTIME_SEC="${MIN_UPTIME_SEC:-1800}"  # 30 minutes default
GATEWAY_UNIT="openclaw-gateway.service"
HEALTHCHECK="/var/lib/occlawshell/bin/healthcheck.sh"

log() { echo "[$(date -Iseconds)] $*" >> "${AUDIT_DIR}/lkg.log"; }

# ── Pre-check 1: Gateway must be running ──
if ! systemctl is-active --quiet "$GATEWAY_UNIT"; then
    log "FAIL: Gateway not active, refusing LKG promotion"
    echo "ERROR: $GATEWAY_UNIT is not running. Cannot promote unstable state."
    exit 1
fi

# ── Pre-check 2: Gateway must have been running >= MIN_UPTIME_SEC ──
ACTIVE_ENTER=$(systemctl show "$GATEWAY_UNIT" --property=ActiveEnterTimestamp --value 2>/dev/null)
if [[ -n "$ACTIVE_ENTER" && "$ACTIVE_ENTER" != "" ]]; then
    ENTER_EPOCH=$(date -d "$ACTIVE_ENTER" +%s 2>/dev/null || echo 0)
    NOW_EPOCH=$(date +%s)
    UPTIME_SEC=$((NOW_EPOCH - ENTER_EPOCH))
    if [[ "$UPTIME_SEC" -lt "$MIN_UPTIME_SEC" ]]; then
        log "FAIL: Gateway uptime ${UPTIME_SEC}s < required ${MIN_UPTIME_SEC}s"
        echo "ERROR: Gateway has only been running for ${UPTIME_SEC}s (need ${MIN_UPTIME_SEC}s)."
        echo "       Wait $((MIN_UPTIME_SEC - UPTIME_SEC))s more, or override with MIN_UPTIME_SEC=0"
        exit 1
    fi
    echo "Gateway uptime: ${UPTIME_SEC}s (>= ${MIN_UPTIME_SEC}s required) ✓"
fi

# ── Pre-check 3: Health check must pass ──
if [[ -x "$HEALTHCHECK" ]]; then
    if ! "$HEALTHCHECK" >/dev/null 2>&1; then
        log "FAIL: Health check failed, refusing LKG promotion"
        echo "ERROR: Health check did not pass. Refusing to promote unhealthy state."
        exit 1
    fi
    echo "Health check: passed ✓"
fi

# Find latest snapshots
LATEST_SQL=$(ls -1t "${SNAP_DIR}"/main-*.sqlite 2>/dev/null | head -1 || true)
LATEST_FILES=$(ls -1dt "${SNAP_DIR}"/files-* 2>/dev/null | head -1 || true)

if [[ -z "$LATEST_SQL" ]]; then
    log "FAIL: No SQLite snapshots found"
    echo "ERROR: No SQLite snapshots in ${SNAP_DIR}"
    exit 1
fi
if [[ -z "$LATEST_FILES" ]]; then
    log "FAIL: No file snapshots found"
    echo "ERROR: No file snapshots in ${SNAP_DIR}"
    exit 1
fi

echo "Promoting to LKG:"
echo "  SQLite: $LATEST_SQL"
echo "  Files:  $LATEST_FILES"

# Create LKG directory
LKG_SNAP="${LKG_DIR}/lkg-${TS}"
mkdir -p "$LKG_SNAP"

# Copy SQLite
cp -p "$LATEST_SQL" "${LKG_SNAP}/main.sqlite"

# Verify SQLite integrity
INTEGRITY=$(sqlite3 "${LKG_SNAP}/main.sqlite" "PRAGMA integrity_check;" 2>&1)
if [[ "$INTEGRITY" != "ok" ]]; then
    rm -rf "$LKG_SNAP"
    log "FAIL: SQLite integrity check failed — $INTEGRITY"
    "$ALERT" CRITICAL "LKG promotion FAILED: SQLite integrity check failed" 2>/dev/null || true
    exit 1
fi

# Row count regression check — refuse promotion if >50% drop from current LKG
NEW_ROWS=$(sqlite3 "${LKG_SNAP}/main.sqlite" "SELECT COUNT(*) FROM chunks;" 2>/dev/null || echo "0")
CURRENT_LKG_DB="${LKG_DIR}/current/main.sqlite"
if [[ -f "$CURRENT_LKG_DB" ]]; then
    OLD_ROWS=$(sqlite3 "$CURRENT_LKG_DB" "SELECT COUNT(*) FROM chunks;" 2>/dev/null || echo "0")
    if [[ "$OLD_ROWS" -gt 0 && "$NEW_ROWS" -gt 0 ]]; then
        DROP_PCT=$(( (OLD_ROWS - NEW_ROWS) * 100 / OLD_ROWS ))
        if [[ "$DROP_PCT" -gt 50 ]]; then
            rm -rf "$LKG_SNAP"
            log "FAIL: Row count regression ${DROP_PCT}% (${OLD_ROWS} -> ${NEW_ROWS})"
            "$ALERT" CRITICAL "LKG promotion REFUSED: row count dropped ${DROP_PCT}% (${OLD_ROWS} -> ${NEW_ROWS})" 2>/dev/null || true
            echo "ERROR: Row count dropped ${DROP_PCT}% (${OLD_ROWS} -> ${NEW_ROWS}). Refusing to promote possibly corrupted state."
            exit 1
        fi
    fi
    echo "Row count: ${NEW_ROWS} (previous LKG: ${OLD_ROWS}) ✓"
fi

# Copy files
cp -rp "$LATEST_FILES"/* "${LKG_SNAP}/" 2>/dev/null || true

# Verify file manifest
if [[ -f "${LKG_SNAP}/MANIFEST.sha256" ]]; then
    cd "$LKG_SNAP"
    if ! sha256sum -c MANIFEST.sha256 --quiet 2>/dev/null; then
        rm -rf "$LKG_SNAP"
        log "FAIL: Manifest verification failed"
        "$ALERT" CRITICAL "LKG promotion FAILED: manifest verification failed" 2>/dev/null || true
        exit 1
    fi
fi

# Update symlink (atomic)
ln -sfn "$LKG_SNAP" "${LKG_DIR}/current"

# Prune old LKGs (keep 10)
cd "$LKG_DIR"
ls -1dt lkg-* 2>/dev/null | tail -n +11 | xargs -r rm -rf || true

log "OK: ${LKG_SNAP}"
"$ALERT" INFO "LKG promoted: ${LKG_SNAP}" 2>/dev/null || true

echo ""
echo "✅ LKG promoted: ${LKG_SNAP}"
echo "   Symlink: ${LKG_DIR}/current -> ${LKG_SNAP}"
