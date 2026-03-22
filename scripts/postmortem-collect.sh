#!/bin/bash
# ClawShell — Post-Mortem Forensic Collector
# Called by: systemd ExecStopPost (runs as root)
# Writes to: /var/lib/occlawshell/postmortem/ (chowned to occlawshell)
set -uo pipefail
# No -e: collect as much as possible even if individual commands fail

UNIT_NAME="${1:-openclaw-gateway}"
# Validate UNIT_NAME to prevent path traversal (script runs as root)
if [[ ! "$UNIT_NAME" =~ ^[a-zA-Z0-9_.-]+$ ]]; then
    echo "FATAL: Invalid UNIT_NAME='${UNIT_NAME}'" >&2; exit 1
fi
TS=$(date +%Y%m%d-%H%M%S)
PM_DIR="/var/lib/occlawshell/postmortem/${TS}-${UNIT_NAME}"
ALERT="/var/lib/occlawshell/bin/alert.sh"
mkdir -p "$PM_DIR"

# Quota warning (non-blocking — postmortem collection must always run)
QUOTA_CHECK="/var/lib/occlawshell/bin/quota-check.sh"
if [[ -x "$QUOTA_CHECK" ]]; then
    "$QUOTA_CHECK" || echo "[$(date -Iseconds)] WARNING: Vault over quota during postmortem collection" >> "/var/lib/occlawshell/audit/backup.log"
fi

# Helper: redact secrets from environment variables
redact_env() {
    sed -E 's/(TOKEN|KEY|SECRET|PASSWORD|APIKEY|API_KEY|CREDENTIAL|PRIVATE|AUTH|JWT|DSN|_URL|COOKIE|SESSION)=[^ ]*/\1=<REDACTED>/gi'
}

# ═══════════════════════════════════════════════════════════════
# 1. SYSTEMD SERVICE STATE
# ═══════════════════════════════════════════════════════════════
{
    echo "=== Systemd Service State ==="
    echo "Collected: $(date -Iseconds)"
    echo ""
    systemctl show "$UNIT_NAME.service" \
        --property=ExecMainPID,ExecMainStatus,Result,ActiveState,SubState,\
NRestarts,MemoryCurrent,MemoryPeak,CPUUsageNSec,TasksCurrent,\
ExecMainStartTimestamp,ExecMainExitTimestamp,\
StatusText,InvocationID 2>/dev/null || echo "(systemctl show failed)"
} > "$PM_DIR/01-systemd-state.txt"

# ═══════════════════════════════════════════════════════════════
# 2. JOURNAL — Last 200 lines (all priorities)
# ═══════════════════════════════════════════════════════════════
journalctl -u "$UNIT_NAME.service" -n 200 --no-pager --output=short-iso \
    > "$PM_DIR/02-journal-last200.txt" 2>/dev/null || true

# ═══════════════════════════════════════════════════════════════
# 3. JOURNAL — Errors only (last 50)
# ═══════════════════════════════════════════════════════════════
journalctl -u "$UNIT_NAME.service" -n 50 --no-pager -p err --output=short-iso \
    > "$PM_DIR/03-journal-errors.txt" 2>/dev/null || true

# ═══════════════════════════════════════════════════════════════
# 4. /proc DATA (may be gone if PID already reaped)
# ═══════════════════════════════════════════════════════════════
PID=$(systemctl show "$UNIT_NAME.service" --property=ExecMainPID --value 2>/dev/null)

if [[ -n "$PID" && "$PID" != "0" && -d "/proc/$PID" ]]; then
    echo "PID $PID still alive, collecting /proc data..." > "$PM_DIR/04-proc-status.txt"
    echo "" >> "$PM_DIR/04-proc-status.txt"
    cat "/proc/$PID/status" >> "$PM_DIR/04-proc-status.txt" 2>/dev/null || true

    cat "/proc/$PID/io" > "$PM_DIR/05-proc-io.txt" 2>/dev/null || true
    cat "/proc/$PID/stack" > "$PM_DIR/06-proc-kernel-stack.txt" 2>/dev/null || true
    cat "/proc/$PID/wchan" > "$PM_DIR/07-proc-wchan.txt" 2>/dev/null || true
    cat "/proc/$PID/cmdline" | tr '\0' ' ' > "$PM_DIR/08-proc-cmdline.txt" 2>/dev/null || true
    cat "/proc/$PID/environ" | tr '\0' '\n' | redact_env > "$PM_DIR/09-proc-environ.txt" 2>/dev/null || true
    cat "/proc/$PID/oom_score" > "$PM_DIR/10-proc-oom-score.txt" 2>/dev/null || true
    cat "/proc/$PID/oom_score_adj" > "$PM_DIR/10-proc-oom-score-adj.txt" 2>/dev/null || true

    # File descriptors
    ls -la "/proc/$PID/fd/" > "$PM_DIR/11-proc-fd-list.txt" 2>/dev/null || true
    wc -l "$PM_DIR/11-proc-fd-list.txt" 2>/dev/null | cut -d' ' -f1 > "$PM_DIR/11-proc-fd-count.txt" || true
    readlink -f "/proc/$PID/fd/"* 2>/dev/null | sort | uniq -c | sort -rn \
        > "$PM_DIR/11-proc-fd-targets.txt" 2>/dev/null || true

    # Network connections
    ss -tlnp 2>/dev/null | grep "pid=$PID" > "$PM_DIR/12-proc-sockets.txt" 2>/dev/null || true
    ss -tnp 2>/dev/null | grep "pid=$PID" >> "$PM_DIR/12-proc-sockets.txt" 2>/dev/null || true

    # cgroup memory & CPU stats
    CGROUP_PATH=$(cat "/proc/$PID/cgroup" 2>/dev/null | head -1 | cut -d: -f3)
    if [[ -n "$CGROUP_PATH" && "$CGROUP_PATH" != *..* && -d "/sys/fs/cgroup${CGROUP_PATH}" ]]; then
        cat "/sys/fs/cgroup${CGROUP_PATH}/memory.stat" > "$PM_DIR/13-cgroup-memory-stat.txt" 2>/dev/null || true
        cat "/sys/fs/cgroup${CGROUP_PATH}/memory.current" > "$PM_DIR/13-cgroup-memory-current.txt" 2>/dev/null || true
        cat "/sys/fs/cgroup${CGROUP_PATH}/memory.peak" > "$PM_DIR/13-cgroup-memory-peak.txt" 2>/dev/null || true
        cat "/sys/fs/cgroup${CGROUP_PATH}/cpu.stat" > "$PM_DIR/13-cgroup-cpu-stat.txt" 2>/dev/null || true
    fi

    # Process tree
    pstree -p "$PID" > "$PM_DIR/14-process-tree.txt" 2>/dev/null || true

else
    echo "PID ${PID:-unknown} not available (already reaped)." > "$PM_DIR/04-proc-UNAVAILABLE.txt"
    echo "Relying on journal, systemd state, and ClawShell's last /proc snapshot." >> "$PM_DIR/04-proc-UNAVAILABLE.txt"

    # Fall back to ClawShell's continuous /proc monitor snapshot
    LAST_SNAPSHOT="/var/lib/occlawshell/audit/gateway-proc-latest.json"
    if [[ -f "$LAST_SNAPSHOT" ]]; then
        cp "$LAST_SNAPSHOT" "$PM_DIR/04-proc-last-known-snapshot.json"
        echo "" >> "$PM_DIR/04-proc-UNAVAILABLE.txt"
        echo "Last known /proc snapshot (from ClawShell monitor) attached as 04-proc-last-known-snapshot.json" \
            >> "$PM_DIR/04-proc-UNAVAILABLE.txt"
    fi
fi

# ═══════════════════════════════════════════════════════════════
# 5. SYSTEM-WIDE CONTEXT
# ═══════════════════════════════════════════════════════════════
{
    echo "=== System Context at $(date -Iseconds) ==="
    echo ""
    echo "--- uptime ---"
    uptime 2>/dev/null || true
    echo ""
    echo "--- free ---"
    free -m 2>/dev/null || true
    echo ""
    echo "--- df ---"
    df -h / /tmp /home 2>/dev/null || true
    echo ""
    echo "--- loadavg ---"
    cat /proc/loadavg 2>/dev/null || true
    echo ""
    echo "--- top 10 memory consumers ---"
    ps aux --sort=-%mem 2>/dev/null | head -11 || true
    echo ""
    echo "--- top 10 CPU consumers ---"
    ps aux --sort=-%cpu 2>/dev/null | head -11 || true
} > "$PM_DIR/15-system-context.txt" 2>/dev/null

# ═══════════════════════════════════════════════════════════════
# 6. DMESG — OOM killer, segfaults, kernel messages
# ═══════════════════════════════════════════════════════════════
dmesg --since="-5min" --time-format=iso > "$PM_DIR/16-dmesg-last5min.txt" 2>/dev/null || \
    dmesg 2>/dev/null | tail -100 > "$PM_DIR/16-dmesg-tail100.txt" 2>/dev/null || true

# ═══════════════════════════════════════════════════════════════
# 7. CORE DUMP (if available via coredumpctl)
# ═══════════════════════════════════════════════════════════════
if command -v coredumpctl &>/dev/null; then
    CORE_INFO=$(coredumpctl list --since="-5min" --no-pager 2>/dev/null | tail -5)
    if [[ -n "$CORE_INFO" ]]; then
        echo "$CORE_INFO" > "$PM_DIR/17-coredump-info.txt"
        # Don't extract the actual core dump (can be huge) — just record metadata
        coredumpctl info --since="-5min" --no-pager >> "$PM_DIR/17-coredump-info.txt" 2>/dev/null || true
    fi
fi

# ═══════════════════════════════════════════════════════════════
# 8. GENERATE SUMMARY
# ═══════════════════════════════════════════════════════════════
{
    echo "╔══════════════════════════════════════════════════════════╗"
    echo "║            POST-MORTEM REPORT                           ║"
    echo "╚══════════════════════════════════════════════════════════╝"
    echo ""
    echo "Unit:      ${UNIT_NAME}.service"
    echo "Timestamp: ${TS}"
    echo "PID:       ${PID:-unknown}"
    echo "Directory: ${PM_DIR}"
    echo ""
    echo "── Systemd Result ──"
    grep -E 'Result|ExecMainStatus|ActiveState|NRestarts' \
        "$PM_DIR/01-systemd-state.txt" 2>/dev/null || echo "(not available)"
    echo ""
    echo "── Last 5 Errors ──"
    tail -5 "$PM_DIR/03-journal-errors.txt" 2>/dev/null || echo "(no errors captured)"
    echo ""
    echo "── OOM Killer ──"
    grep -i "oom\|killed process\|out of memory" \
        "$PM_DIR/16-dmesg-last5min.txt" "$PM_DIR/16-dmesg-tail100.txt" 2>/dev/null || echo "(no OOM events)"
    echo ""
    echo "── Memory at Death ──"
    if [[ -f "$PM_DIR/04-proc-status.txt" ]]; then
        grep -E 'VmRSS|VmPeak|VmSize|Threads' "$PM_DIR/04-proc-status.txt" 2>/dev/null || true
    elif [[ -f "$PM_DIR/04-proc-last-known-snapshot.json" ]]; then
        cat "$PM_DIR/04-proc-last-known-snapshot.json" 2>/dev/null | \
            python3 -c "import sys,json; d=json.load(sys.stdin); [print(f'{k}: {v}') for k,v in d.items() if k.startswith(('Vm','Thread','fd_','oom_'))]" 2>/dev/null || true
    fi
    echo ""
    echo "── Files Collected ──"
    ls -la "$PM_DIR/" 2>/dev/null
} > "$PM_DIR/00-SUMMARY.txt"

# ═══════════════════════════════════════════════════════════════
# 9. FINALIZE
# ═══════════════════════════════════════════════════════════════
# Set ownership
chown -R occlawshell:occlawshell "$PM_DIR"
chmod -R 700 "$PM_DIR"

# Alert
"$ALERT" WARNING \
    "Post-mortem collected for ${UNIT_NAME} (PID ${PID:-?}). Path: ${PM_DIR}" \
    2>/dev/null || true

# Prune old postmortems (keep 20)
cd /var/lib/occlawshell/postmortem || exit 0
ls -1dt ./*/ 2>/dev/null | tail -n +21 | xargs -r rm -rf

exit 0
