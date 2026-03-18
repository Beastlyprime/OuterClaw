#!/usr/bin/env bash
# migrate-to-ocagent.sh — Copy OpenClaw data from yimeng to ocagent
#
# Usage: sudo bash migrate-to-ocagent.sh
# Prerequisites: ocagent user must exist, openclaw must be installed under ocagent
#
# This script copies memory, identity, credentials, config, and workspace files.
# It does NOT stop/start any services — do that manually before/after.

set -euo pipefail

SRC="/home/yimeng/.openclaw"
DST="/home/ocagent/.openclaw"

# ── Preflight checks ─────────────────────────────────────────

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: Run with sudo" >&2
    exit 1
fi

if ! id ocagent &>/dev/null; then
    echo "ERROR: ocagent user does not exist. Create it first:" >&2
    echo "  sudo useradd -r -m -s /bin/bash ocagent" >&2
    exit 1
fi

if [[ ! -d "$SRC" ]]; then
    echo "ERROR: Source $SRC does not exist" >&2
    exit 1
fi

if [[ ! -d "$DST" ]]; then
    echo "WARNING: $DST does not exist. Creating it." >&2
    mkdir -p "$DST"
    chown ocagent:ocagent "$DST"
fi

echo "=== Migrating OpenClaw data: $SRC -> $DST ==="
echo ""

# ── Helper ────────────────────────────────────────────────────

copy_item() {
    local src_path="$1"
    local dst_path="$2"

    if [[ ! -e "$src_path" ]]; then
        echo "  SKIP (not found): $src_path"
        return
    fi

    # Ensure parent directory exists
    mkdir -p "$(dirname "$dst_path")"

    if [[ -d "$src_path" ]]; then
        cp -a "$src_path/" "$dst_path/"
    else
        cp -a "$src_path" "$dst_path"
    fi
    echo "  OK: $src_path"
}

# ── 1. Memory (core data) ────────────────────────────────────

echo "[1/6] Memory files"
copy_item "$SRC/memory"                         "$DST/memory"

echo ""
echo "[2/6] Workspace memory & identity"
copy_item "$SRC/workspace/MEMORY.md"            "$DST/workspace/MEMORY.md"
copy_item "$SRC/workspace/memory"               "$DST/workspace/memory"
copy_item "$SRC/workspace/SOUL.md"              "$DST/workspace/SOUL.md"
copy_item "$SRC/workspace/AGENTS.md"            "$DST/workspace/AGENTS.md"
copy_item "$SRC/workspace/USER.md"              "$DST/workspace/USER.md"
copy_item "$SRC/workspace/IDENTITY.md"          "$DST/workspace/IDENTITY.md"

echo ""
echo "[3/6] Credentials"
copy_item "$SRC/credentials"                    "$DST/credentials"

echo ""
echo "[4/6] Config"
copy_item "$SRC/openclaw.json"                  "$DST/openclaw.json"
copy_item "$SRC/.env"                           "$DST/.env"
copy_item "$SRC/identity"                       "$DST/identity"

echo ""
echo "[5/6] Agents"
copy_item "$SRC/agents"                         "$DST/agents"

echo ""
echo "[6/6] Other state"
copy_item "$SRC/telegram"                       "$DST/telegram"
copy_item "$SRC/devices"                        "$DST/devices"
copy_item "$SRC/cron"                           "$DST/cron"

# ── Fix ownership ─────────────────────────────────────────────

echo ""
echo "Fixing paths in openclaw.json..."
if [[ -f "$DST/openclaw.json" ]]; then
    sed -i 's|/home/yimeng/|/home/ocagent/|g' "$DST/openclaw.json"
    echo "  OK: replaced /home/yimeng/ -> /home/ocagent/ in openclaw.json"
fi

echo ""
echo "Fixing ownership..."
chown -R ocagent:ocagent "$DST"
chmod 700 "$DST"
chmod 700 "$DST/credentials" 2>/dev/null || true
chmod 600 "$DST/.env" 2>/dev/null || true
chmod 600 "$DST/openclaw.json" 2>/dev/null || true

echo ""
echo "=== Done ==="
echo ""
echo "Verify:"
echo "  sudo -u ocagent ls $DST/memory/"
echo "  sudo -u ocagent sqlite3 $DST/memory/main.sqlite 'PRAGMA integrity_check'"
echo ""
echo "Next steps:"
echo "  1. Stop old gateway:  systemctl --user stop openclaw-gateway"
echo "  2. Deploy ClawShell:  sudo bash deploy.sh"
echo "  3. Start new gateway: sudo systemctl start openclaw-gateway"
