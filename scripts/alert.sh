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

# Always log locally
echo "[$TS] [$LEVEL] $MESSAGE" >> "$LOG"

# Send via Telegram if configured
if [[ -n "$TG_TOKEN" && -n "$TG_CHAT" ]]; then
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
        -d "{\"chat_id\":\"${TG_CHAT}\",\"text\":$(echo "$TEXT" | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read()))'),\"parse_mode\":\"Markdown\"}" \
        --max-time 10 \
        > /dev/null 2>&1 || echo "[$TS] [ALERT-FAIL] Telegram send failed" >> "$LOG"
fi

exit 0
