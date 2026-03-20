#!/bin/bash
# ClawShell — I/O Pressure Gate
# Usage: io-pressure-check.sh [THRESHOLD]
# Exit 0 = I/O pressure acceptable, Exit 1 = too high (defer snapshot)
# Reads /proc/pressure/io (Linux PSI). Falls through if PSI unavailable.
set -uo pipefail

PSI_FILE="/proc/pressure/io"
VAULT="/var/lib/occlawshell"
AUDIT_DIR="${VAULT}/audit"
ENV_FILE="${VAULT}/config/clawshell.env"

THRESHOLD="${1:-}"
if [[ -z "$THRESHOLD" ]]; then
    THRESHOLD=$(grep '^IO_PRESSURE_THRESHOLD=' "${ENV_FILE}" 2>/dev/null | cut -d= -f2)
    THRESHOLD="${THRESHOLD:-25}"
fi

RETRY_DELAY=30

log() { echo "[$(date -Iseconds)] IO-PRESSURE: $*" >> "${AUDIT_DIR}/backup.log"; }

# Fallback: if PSI not available, allow snapshot
if [[ ! -r "$PSI_FILE" ]]; then
    exit 0
fi

# Read avg10 from the "some" line (% of time at least some tasks stalled on I/O)
# Format: some avg10=0.00 avg60=0.00 avg300=0.00 total=123456
read_pressure() {
    local raw
    raw=$(grep '^some' "$PSI_FILE" 2>/dev/null | sed -n 's/.*avg10=\([0-9]*\).*/\1/p')
    echo "${raw:-0}"
}

PRESSURE=$(read_pressure)

if [[ "$PRESSURE" -lt "$THRESHOLD" ]]; then
    exit 0
fi

log "I/O pressure high (avg10=${PRESSURE}% >= ${THRESHOLD}%), waiting ${RETRY_DELAY}s..."
sleep "$RETRY_DELAY"

PRESSURE=$(read_pressure)
if [[ "$PRESSURE" -lt "$THRESHOLD" ]]; then
    log "I/O pressure subsided (avg10=${PRESSURE}%), proceeding"
    exit 0
fi

log "I/O pressure still high (avg10=${PRESSURE}% >= ${THRESHOLD}%), deferring snapshot"
exit 1
