#!/bin/bash
# ClawShell — File Snapshot (Memory, Config, Workspace Bundle)
# Runs as: occlawshell (via systemd timer)
set -euo pipefail

# Read OPENCLAW_DIR from clawshell.env (grep-based, no shell eval)
ENV_FILE="/var/lib/occlawshell/config/clawshell.env"
OPENCLAW_DIR=$(grep '^OPENCLAW_DIR=' "${ENV_FILE}" 2>/dev/null | cut -d= -f2)
OPENCLAW_DIR="${OPENCLAW_DIR:-/home/ocagent/.openclaw}"

SRC_WORKSPACE="${OPENCLAW_DIR}/workspace"
SRC_CONFIG="${OPENCLAW_DIR}/openclaw.json"
SRC_APPROVALS="${OPENCLAW_DIR}/exec-approvals.json"
DST_DIR="/var/lib/occlawshell/snapshots"
AUDIT_DIR="/var/lib/occlawshell/audit"
TS=$(date +%Y%m%d-%H%M%S)
SNAP="${DST_DIR}/files-${TS}"

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

# Wait for a file's mtime to stabilize (not modified in last 2 seconds).
# Prevents copying half-written files during active OpenClaw writes.
wait_stable() {
    local file="$1"
    local max_wait=10  # give up after 10 seconds
    local waited=0
    while [[ "$waited" -lt "$max_wait" ]]; do
        local mtime_age=$(( $(date +%s) - $(stat -c %Y "$file" 2>/dev/null || echo 0) ))
        if [[ "$mtime_age" -ge 2 ]]; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    log "WARNING: ${file} still being written after ${max_wait}s, copying anyway"
    return 0
}

mkdir -p "${SNAP}/memory" "${SNAP}/config"

# Memory files (wait for stable mtime before copying)
if [[ -f "${SRC_WORKSPACE}/MEMORY.md" ]]; then
    wait_stable "${SRC_WORKSPACE}/MEMORY.md"
fi
rsync -a --checksum \
    "${SRC_WORKSPACE}/MEMORY.md" \
    "${SNAP}/" 2>/dev/null || log "WARNING: MEMORY.md copy failed"

rsync -a --checksum \
    "${SRC_WORKSPACE}/memory/" \
    "${SNAP}/memory/" 2>/dev/null || log "WARNING: memory/ copy failed"

# Config files
cp -p "$SRC_CONFIG" "${SNAP}/config/openclaw.json" 2>/dev/null || true
cp -p "$SRC_APPROVALS" "${SNAP}/config/exec-approvals.json" 2>/dev/null || true

# Workspace git bundle (all history in one file)
if [ -d "${SRC_WORKSPACE}/.git" ]; then
    cd "$SRC_WORKSPACE"
    git bundle create "${SNAP}/workspace.bundle" --all 2>/dev/null || \
        log "WARNING: git bundle failed"
fi

# Generate manifest
find "$SNAP" -type f ! -name 'MANIFEST.sha256' -exec sha256sum {} \; \
    > "${SNAP}/MANIFEST.sha256"

# Count files
FILE_COUNT=$(find "$SNAP" -type f | wc -l)
SIZE=$(du -sh "$SNAP" | cut -f1)
log "OK: ${SNAP} (${FILE_COUNT} files, ${SIZE})"

# Prune (keep 96 = 48h at 30-min intervals)
cd "$DST_DIR"
ls -1dt files-* 2>/dev/null | tail -n +97 | xargs -r rm -rf || true

exit 0
