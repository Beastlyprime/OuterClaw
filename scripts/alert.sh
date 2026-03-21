#!/bin/bash
# ClawShell — Telegram Alert
# Usage: alert.sh <LEVEL> <MESSAGE>
# Levels: INFO, WARNING, CRITICAL
set -uo pipefail

LEVEL="${1:-INFO}"
MESSAGE="${2:-No message provided}"
TS=$(date -Iseconds)
LOG="/var/lib/occlawshell/audit/alerts.log"

# Load config safely (grep-based, no shell eval)
ENV_FILE="/var/lib/occlawshell/config/clawshell.env"
TG_TOKEN=""
TG_CHAT=""
if [[ -f "$ENV_FILE" ]]; then
    TG_TOKEN=$(grep -oP '^CLAWSHELL_TG_TOKEN=\K.+' "$ENV_FILE" 2>/dev/null || true)
    TG_CHAT=$(grep -oP '^CLAWSHELL_TG_CHAT=\K.+' "$ENV_FILE" 2>/dev/null || true)
fi

# Fallback: read from OpenClaw's openclaw.json if not configured in clawshell.env
if [[ -z "$TG_TOKEN" || -z "$TG_CHAT" ]]; then
    OPENCLAW_DIR=$(grep -oP '^OPENCLAW_DIR=\K.+' "$ENV_FILE" 2>/dev/null || true)
    OPENCLAW_DIR="${OPENCLAW_DIR:-/home/ocagent/.openclaw}"
    OC_CONFIG="${OPENCLAW_DIR}/openclaw.json"
    if [[ -r "$OC_CONFIG" ]]; then
        if [[ -z "$TG_TOKEN" ]]; then
            TG_TOKEN=$(OC_CFG="$OC_CONFIG" python3 -c "
import json,os
try:
    d=json.load(open(os.environ['OC_CFG']))
    print(d.get('channels',{}).get('telegram',{}).get('botToken',''))
except: pass
" 2>/dev/null || true)
        fi
        if [[ -z "$TG_CHAT" ]]; then
            TG_CHAT=$(OC_CFG="$OC_CONFIG" python3 -c "
import json,os
try:
    d=json.load(open(os.environ['OC_CFG']))
    af=d.get('channels',{}).get('telegram',{}).get('allowFrom',[])
    if af: print(af[0])
except: pass
" 2>/dev/null || true)
        fi
    fi
fi

# Always log locally
echo "[$TS] [$LEVEL] $MESSAGE" >> "$LOG"

# Send via Telegram if configured
# Validate TG_TOKEN format (digits:alphanumeric) and TG_CHAT is numeric
if [[ -n "$TG_TOKEN" && -n "$TG_CHAT" && "$TG_TOKEN" =~ ^[0-9]+:[a-zA-Z0-9_-]+$ && "$TG_CHAT" =~ ^-?[0-9]+$ ]]; then
    case "$LEVEL" in
        CRITICAL) EMOJI="🚨" ;;
        WARNING)  EMOJI="⚠️" ;;
        INFO)     EMOJI="ℹ️" ;;
        *)        EMOJI="📋" ;;
    esac

    TEXT="${EMOJI} *ClawShell ${LEVEL}*
\`$(date +%H:%M:%S)\` ${MESSAGE}"

    curl -s -X POST \
        "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
        -H "Content-Type: application/json" \
        -d "{\"chat_id\":\"${TG_CHAT}\",\"text\":$(printf '%s' "$TEXT" | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read()))'),\"parse_mode\":\"Markdown\"}" \
        --max-time 10 \
        > /dev/null 2>&1 || echo "[$TS] [ALERT-FAIL] Telegram send failed" >> "$LOG"
fi

exit 0
