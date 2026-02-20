#!/bin/bash
# ClawShell — Health Check
# Runs as: occlawshell (via systemd timer, every 2 min)
set -uo pipefail

GATEWAY_PORT="${GATEWAY_PORT:-18789}"
ALERT="/var/lib/occlawshell/bin/alert.sh"
STATE_FILE="/var/lib/occlawshell/audit/health-state.json"

# Check 1: Service active?
if ! systemctl is-active --quiet openclaw-gateway.service; then
    "$ALERT" CRITICAL "Gateway service not active ($(systemctl is-active openclaw-gateway.service 2>/dev/null))"
    exit 1
fi

# Check 2: HTTP responsive?
HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' \
    --max-time 10 \
    "http://127.0.0.1:${GATEWAY_PORT}/health" 2>/dev/null || echo "000")

if [[ "$HTTP_CODE" == "000" ]]; then
    "$ALERT" WARNING "Gateway not responding to HTTP health check (timeout)"
elif [[ "$HTTP_CODE" != "200" && "$HTTP_CODE" != "404" ]]; then
    # 404 is OK — means the server is up but may not have /health endpoint
    "$ALERT" WARNING "Gateway HTTP health returned ${HTTP_CODE}"
fi

# Check 3: Memory pressure
MEM_CURRENT=$(systemctl show openclaw-gateway.service --property=MemoryCurrent --value 2>/dev/null)
if [[ -n "$MEM_CURRENT" && "$MEM_CURRENT" != "[not set]" ]]; then
    MEM_MB=$((MEM_CURRENT / 1048576))
    if [[ "$MEM_MB" -gt 7168 ]]; then  # >7GB
        "$ALERT" WARNING "Gateway memory at ${MEM_MB}MB (limit: 8192MB)"
    fi
fi

# Check 4: Restart count (detect crash loops)
N_RESTARTS=$(systemctl show openclaw-gateway.service --property=NRestarts --value 2>/dev/null)
if [[ -n "$N_RESTARTS" && "$N_RESTARTS" -gt 0 ]]; then
    # Check if this is new
    PREV_RESTARTS=0
    if [[ -f "$STATE_FILE" ]]; then
        PREV_RESTARTS=$(python3 -c "import json; print(json.load(open('$STATE_FILE')).get('restarts',0))" 2>/dev/null || echo 0)
    fi
    if [[ "$N_RESTARTS" -gt "$PREV_RESTARTS" ]]; then
        "$ALERT" WARNING "Gateway restarted (count: ${N_RESTARTS}, was: ${PREV_RESTARTS})"
    fi
fi

# Save state for next run (pass values via env to prevent injection)
HC_HTTP_CODE="${HTTP_CODE}" \
HC_MEM_CURRENT="${MEM_CURRENT:-0}" \
HC_N_RESTARTS="${N_RESTARTS:-0}" \
HC_STATE_FILE="${STATE_FILE}" \
python3 -c "
import json, os
from datetime import datetime
state = {
    'timestamp': datetime.utcnow().isoformat(),
    'http_code': os.environ['HC_HTTP_CODE'],
    'mem_current': os.environ['HC_MEM_CURRENT'],
    'restarts': int(os.environ.get('HC_N_RESTARTS', '0'))
}
with open(os.environ['HC_STATE_FILE'], 'w') as f:
    json.dump(state, f, indent=2)
" 2>/dev/null || true

exit 0
