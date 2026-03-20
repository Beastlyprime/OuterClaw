#!/bin/bash
# ═══════════════════════════════════════════════════════════════
# ClawShell — Deployment Script
# Run as: sudo bash deploy.sh
# Idempotent: safe to run multiple times
# ═══════════════════════════════════════════════════════════════
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VAULT="/var/lib/occlawshell"
AGENT_USER="ocagent"
AGENT_HOME="/home/${AGENT_USER}"
OPENCLAW_DIR="${AGENT_HOME}/.openclaw"
WORKSPACE="${OPENCLAW_DIR}/workspace"

echo "╔══════════════════════════════════════════════════════════╗"
echo "║                  ClawShell — Deployment                  ║"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""

# ── Pre-checks ──
if [[ $EUID -ne 0 ]]; then
    echo "ERROR: Must run as root (sudo bash deploy.sh)"
    exit 1
fi

if ! id yimeng &>/dev/null; then
    echo "ERROR: User 'yimeng' not found"
    exit 1
fi

echo "Phase 1: Create Service Users"
echo "─────────────────────────────"
# 1a. Create ocagent — dedicated user for running OpenClaw (no sudo)
if id "${AGENT_USER}" &>/dev/null; then
    echo "  ✓ User '${AGENT_USER}' already exists"
else
    useradd -m \
        -s /bin/bash \
        --comment "OpenClaw Agent (no sudo)" \
        "${AGENT_USER}"
    echo "  ✓ User '${AGENT_USER}' created (login shell, no sudo)"
fi

# 1b. Create occlawshell — system user for running ClawShell (nologin)
if id occlawshell &>/dev/null; then
    echo "  ✓ User 'occlawshell' already exists"
else
    useradd --system \
        --shell /usr/sbin/nologin \
        --home-dir "$VAULT" \
        --create-home \
        --comment "ClawShell Service" \
        occlawshell
    echo "  ✓ User 'occlawshell' created"
fi

# 1c. Data migration check
if [[ -d "/home/yimeng/.openclaw" && ! -d "${OPENCLAW_DIR}" ]]; then
    echo ""
    echo "  ╔══════════════════════════════════════════════════════════╗"
    echo "  ║  DATA MIGRATION REQUIRED                                 ║"
    echo "  ╚══════════════════════════════════════════════════════════╝"
    echo "  Detected /home/yimeng/.openclaw but ${OPENCLAW_DIR} does not exist."
    echo "  Run the following commands to migrate:"
    echo ""
    echo "    systemctl stop openclaw-gateway"
    echo "    sudo cp -a /home/yimeng/.openclaw ${OPENCLAW_DIR}"
    echo "    sudo chown -R ${AGENT_USER}:${AGENT_USER} ${OPENCLAW_DIR}"
    echo "    sudo systemctl start openclaw-gateway"
    echo ""
    echo "  Then re-run this deploy script."
    echo ""
fi

echo ""
echo "Phase 1c: Detect OpenClaw Installation"
echo "───────────────────────────────────────"
# Allow override: OPENCLAW_BIN=/path/to/openclaw sudo bash deploy.sh
if [[ -z "${OPENCLAW_BIN:-}" ]]; then
    # Method 1: Ask ocagent's login shell (preferred — this is who runs OpenClaw)
    OPENCLAW_BIN=$(sudo -u "${AGENT_USER}" bash -lc 'command -v openclaw' 2>/dev/null) || true

    # Method 2: Ask yimeng's login shell (finds nvm-managed installs)
    if [[ -z "$OPENCLAW_BIN" || ! -x "${OPENCLAW_BIN:-}" ]]; then
        OPENCLAW_BIN=$(sudo -u yimeng bash -lc 'command -v openclaw' 2>/dev/null) || true
    fi

    # Method 3: Search common global npm paths
    if [[ -z "$OPENCLAW_BIN" || ! -x "${OPENCLAW_BIN:-}" ]]; then
        for search_dir in "${AGENT_HOME}/.npm-global" /home/yimeng/.nvm /usr/local/lib; do
            if [[ -d "$search_dir" ]]; then
                OPENCLAW_BIN=$(find "$search_dir" -name openclaw -path '*/bin/openclaw' -type f 2>/dev/null | sort -V | tail -1) || true
                [[ -n "$OPENCLAW_BIN" && -x "$OPENCLAW_BIN" ]] && break
            fi
        done
    fi

    # Method 4: System PATH
    if [[ -z "$OPENCLAW_BIN" || ! -x "${OPENCLAW_BIN:-}" ]]; then
        OPENCLAW_BIN=$(command -v openclaw 2>/dev/null) || true
    fi
fi

if [[ -n "${OPENCLAW_BIN:-}" && -x "$OPENCLAW_BIN" ]]; then
    OPENCLAW_NODE_DIR=$(dirname "$OPENCLAW_BIN")
    echo "  ✓ Found openclaw: $OPENCLAW_BIN"
    echo "  ✓ Node directory: $OPENCLAW_NODE_DIR"
else
    OPENCLAW_NODE_DIR=""
    echo "  ⚠ openclaw binary not found"
    echo "    Set OPENCLAW_BIN=/path/to/openclaw before running deploy.sh"
    echo "    Or install openclaw: npm install -g openclaw"
fi

echo ""
echo "Phase 2: Create Vault Directory Structure"
echo "──────────────────────────────────────────"
for dir in lkg snapshots postmortem audit config bin; do
    mkdir -p "${VAULT}/${dir}"
done
chown -R occlawshell:occlawshell "$VAULT"
chmod -R 700 "$VAULT"
# Vault root: 711 allows traverse (cd) but not listing — ocagent can reach bin/ but
# cannot see vault contents (lkg/, snapshots/, etc. remain 700 = occlawshell only)
chmod 711 "$VAULT"
# bin/ must be 755 so gateway service (ocagent) can execute start-gateway.sh
chmod 755 "${VAULT}/bin"
echo "  ✓ Vault structure created at $VAULT"

echo ""
echo "Phase 3: Grant Read-Only ACL to occlawshell"
echo "───────────────────────────────────────────"
# Home directory traverse — occlawshell needs 'x' to reach .openclaw/
setfacl -m u:occlawshell:x "${AGENT_HOME}"
echo "  ✓ Home directory traverse ACL set"

# .openclaw root traverse
setfacl -m u:occlawshell:rx "${OPENCLAW_DIR}"
echo "  ✓ .openclaw root ACL set"

# Workspace
setfacl -R -m u:occlawshell:rX "${WORKSPACE}"
setfacl -R -d -m u:occlawshell:rX "${WORKSPACE}"
echo "  ✓ Workspace ACL set"

# Memory directory — default ACL ensures recreated SQLite files inherit read permission
if [[ -d "${OPENCLAW_DIR}/memory" ]]; then
    setfacl -R -m u:occlawshell:rX "${OPENCLAW_DIR}/memory"
    setfacl -R -d -m u:occlawshell:rX "${OPENCLAW_DIR}/memory"
    echo "  ✓ Memory directory ACL set (with default ACL for new files)"
fi

# OpenClaw root directory — default ACL for config files (openclaw.json, .env, etc.)
setfacl -m u:occlawshell:rX "${OPENCLAW_DIR}"
setfacl -d -m u:occlawshell:r "${OPENCLAW_DIR}"
# Existing config files need explicit ACL (default ACL only applies to new files)
for cfg in "${OPENCLAW_DIR}/openclaw.json" "${OPENCLAW_DIR}/.env"; do
    if [[ -f "$cfg" ]]; then
        setfacl -m u:occlawshell:r "$cfg"
    fi
done
echo "  ✓ Config ACL set (with default ACL for new files)"

# Logs
if [[ -d "${OPENCLAW_DIR}/logs" ]]; then
    setfacl -R -m u:occlawshell:rX "${OPENCLAW_DIR}/logs"
    setfacl -R -d -m u:occlawshell:rX "${OPENCLAW_DIR}/logs"
    echo "  ✓ Logs ACL set"
fi

echo ""
echo "Phase 4: Deploy Scripts & ClawShell Daemon"
echo "──────────────────────────────────────────"
for script in "${SCRIPT_DIR}"/scripts/*.sh; do
    if [[ -f "$script" ]]; then
        cp "$script" "${VAULT}/bin/"
        echo "  ✓ Deployed $(basename "$script")"
    fi
done
# Deploy clawshell.py daemon
if [[ -f "${SCRIPT_DIR}/clawshell.py" ]]; then
    cp "${SCRIPT_DIR}/clawshell.py" "${VAULT}/bin/"
    echo "  ✓ Deployed clawshell.py"
else
    echo "  ⊘ clawshell.py not found in ${SCRIPT_DIR}"
fi
chown -R occlawshell:occlawshell "${VAULT}/bin/"
chmod 755 "${VAULT}/bin/"*.sh
chmod 644 "${VAULT}/bin/clawshell.py" 2>/dev/null || true
echo "  ✓ All scripts deployed"

# Generate gateway start script (uses detected paths, avoids hardcoding)
if [[ -n "${OPENCLAW_NODE_DIR:-}" ]]; then
    cat > "${VAULT}/bin/start-gateway.sh" << GWEOF
#!/bin/bash
# Auto-generated by deploy.sh at $(date -Iseconds)
# Re-run deploy.sh after updating Node.js or OpenClaw to refresh paths
export PATH="${OPENCLAW_NODE_DIR}:\$PATH"
exec openclaw gateway --port "\${OPENCLAW_GATEWAY_PORT:-18789}"
GWEOF
    chown occlawshell:occlawshell "${VAULT}/bin/start-gateway.sh"
    chmod 755 "${VAULT}/bin/start-gateway.sh"
    echo "  ✓ Generated start-gateway.sh (node: ${OPENCLAW_NODE_DIR})"
else
    echo "  ⊘ start-gateway.sh not generated (openclaw not found)"
fi

echo ""
echo "Phase 5: Immutable Identity Files"
echo "──────────────────────────────────"
# Identity files in workspace
for f in SOUL.md AGENTS.md USER.md; do
    FPATH="${WORKSPACE}/${f}"
    if [[ -f "$FPATH" ]]; then
        if lsattr "$FPATH" 2>/dev/null | grep -q 'i'; then
            echo "  ✓ ${f} already immutable"
        else
            chattr +i "$FPATH"
            echo "  ✓ ${f} set immutable"
        fi
    else
        echo "  ⊘ ${f} not found (skipped)"
    fi
done

echo "  ℹ️  To edit identity files: sudo chattr -i <file>, edit, sudo chattr +i <file>"

echo ""
echo "Phase 5b: Sudoers for Auto-Recovery"
echo "────────────────────────────────────"
SUDOERS_FILE="/etc/sudoers.d/occlawshell"
cat > "$SUDOERS_FILE" << 'SUDEOF'
# ClawShell: narrowly scoped privileged operations
# restart gateway, auto-recover from LKG, lock/unlock identity files
occlawshell ALL=(root) NOPASSWD: /usr/bin/systemctl restart openclaw-gateway.service
occlawshell ALL=(root) NOPASSWD: /var/lib/occlawshell/bin/auto-recover.sh
occlawshell ALL=(root) NOPASSWD: /var/lib/occlawshell/bin/identity-lock.sh
SUDEOF
chmod 440 "$SUDOERS_FILE"
if visudo -c -f "$SUDOERS_FILE" >/dev/null 2>&1; then
    echo "  ✓ Sudoers rules deployed (restart + auto-recover only)"
else
    echo "  ✗ ERROR: Invalid sudoers syntax, removing"
    rm -f "$SUDOERS_FILE"
fi

echo ""
echo "Phase 6: Deploy systemd Services"
echo "─────────────────────────────────"
SYSTEMD_DIR="/etc/systemd/system"
for unit in "${SCRIPT_DIR}"/systemd/*.{service,timer}; do
    if [[ -f "$unit" ]]; then
        BASENAME=$(basename "$unit")
        # Don't overwrite gateway service without explicit flag
        if [[ "$BASENAME" == "openclaw-gateway.service" ]]; then
            echo "  ⊘ ${BASENAME} — skipped (manual migration required)"
            echo "    To migrate: sudo cp ${unit} ${SYSTEMD_DIR}/"
            echo "    Then: systemctl --user stop openclaw-gateway && sudo systemctl daemon-reload && sudo systemctl enable --now openclaw-gateway"
            continue
        fi
        cp "$unit" "${SYSTEMD_DIR}/"
        echo "  ✓ Deployed ${BASENAME}"
    fi
done

systemctl daemon-reload
echo "  ✓ systemd daemon reloaded"

echo ""
echo "Phase 7: Deploy Logrotate Config"
echo "─────────────────────────────────"
LOGROTATE_SRC="${SCRIPT_DIR}/logrotate/occlawshell"
if [[ -f "$LOGROTATE_SRC" ]]; then
    cp "$LOGROTATE_SRC" /etc/logrotate.d/occlawshell
    chmod 644 /etc/logrotate.d/occlawshell
    echo "  ✓ Logrotate config deployed to /etc/logrotate.d/occlawshell"
else
    echo "  ⊘ logrotate/occlawshell not found in ${SCRIPT_DIR}"
fi

echo ""
echo "Phase 8: ClawShell Environment Config"
echo "─────────────────────────────────────"
CLAWSHELL_ENV="${VAULT}/config/clawshell.env"

configure_env() {
    local token=""
    local chat=""
    local port="18789"

    # Try to read existing port if file exists
    if [[ -f "$CLAWSHELL_ENV" ]]; then
        port=$(grep "GATEWAY_PORT=" "$CLAWSHELL_ENV" | cut -d'=' -f2 || echo "18789")
    fi

    echo "Configure Telegram Notifications for ClawShell?"
    read -p "  Enable Telegram alerts? [y/N] " -n 1 -r
    echo ""
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        while [[ -z "$token" ]]; do
            read -p "  Enter Bot Token: " token
        done
        while [[ -z "$chat" ]]; do
            read -p "  Enter Chat ID: " chat
        done
    fi

    cat > "$CLAWSHELL_ENV" << ENVEOF
# ClawShell Telegram Bot Config (env vars: CLAWSHELL_TG_*)
CLAWSHELL_TG_TOKEN=${token}
CLAWSHELL_TG_CHAT=${chat}
# Gateway config
GATEWAY_PORT=${port}
# Three-user model: ocagent runs OpenClaw, occlawshell runs ClawShell
AGENT_USER=${AGENT_USER}
OPENCLAW_DIR=${OPENCLAW_DIR}
# OpenClaw binary path (auto-detected by deploy.sh, used by auto-recover.sh)
OPENCLAW_BIN=${OPENCLAW_BIN:-}
ENVEOF
    chown occlawshell:occlawshell "$CLAWSHELL_ENV"
    chmod 600 "$CLAWSHELL_ENV"
}

if [[ ! -f "$CLAWSHELL_ENV" ]]; then
    echo "  Creating new clawshell.env..."
    configure_env
    echo "  ✓ clawshell.env created"
else
    echo "  clawshell.env already exists."
    read -p "  Re-configure Telegram settings? [y/N] " -n 1 -r
    echo ""
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        configure_env
        echo "  ✓ clawshell.env updated"
    else
        echo "  ✓ Kept existing config"
    fi
fi

echo ""
echo "Phase 9: Enable Services & Timers"
echo "───────────────────────────────────"
# Enable timers
for timer in oc-snapshot.timer oc-healthcheck.timer oc-lkg-promote.timer; do
    systemctl enable "$timer" 2>/dev/null && echo "  ✓ Enabled $timer" || echo "  ⊘ $timer not available"
done

# Don't auto-start clawshell until config is ready
echo "  ℹ️  ClawShell daemon NOT auto-started."
echo "  ℹ️  After configuring clawshell.env, run:"
echo "      sudo systemctl enable --now oc-clawshell.service"
echo "      sudo systemctl start oc-snapshot.timer"
echo "      sudo systemctl start oc-healthcheck.timer"

echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "║          Deployment Complete                             ║"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""
echo "Verification commands:"
echo "  sudo -u ${AGENT_USER} ls /var/lib/occlawshell/     # Should fail (Permission denied)"
echo "  sudo -u ${AGENT_USER} sudo -l                      # Should show: no sudo privileges"
echo "  sudo -u occlawshell cat ${WORKSPACE}/MEMORY.md | head -1  # Should work"
echo "  sudo -u ${AGENT_USER} echo x >> ${WORKSPACE}/SOUL.md  # Should fail (immutable)"
echo "  sudo -u ${AGENT_USER} kill \$(pgrep -u occlawshell)    # Should fail (Operation not permitted)"
echo "  systemctl list-timers | grep oc-             # Should show timers"
echo ""
echo "Next steps:"
echo "  1. Edit ${VAULT}/config/clawshell.env with Telegram bot credentials"
echo "  2. Test: sudo -u occlawshell ${VAULT}/bin/snapshot-sqlite.sh"
echo "  3. Start: sudo systemctl enable --now oc-clawshell.service"
echo "  4. Start timers: sudo systemctl start oc-snapshot.timer oc-healthcheck.timer oc-lkg-promote.timer"
echo "  5. (Optional) Migrate gateway to system service for full isolation"
echo ""
