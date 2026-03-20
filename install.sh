#!/bin/bash
# ═══════════════════════════════════════════════════════════════
# ClawShell — One-Click Installer for OpenClaw Users
#
# Usage: curl -fsSL https://clawshell.dev/install.sh | sudo bash
#    or: sudo bash install.sh
#
# What this does:
#   1. Detects existing OpenClaw installation
#   2. Creates isolated users (ocagent + occlawshell)
#   3. Installs Node.js globally if needed
#   4. Installs OpenClaw under ocagent
#   5. Migrates data from current user to ocagent
#   6. Deploys ClawShell (watchdog + backups + alerts)
#   7. Migrates gateway to system service
#   8. Starts all services
#
# Safe to re-run (idempotent). Will not destroy existing data.
# ═══════════════════════════════════════════════════════════════
set -euo pipefail

VERSION="1.0"
AGENT_USER="ocagent"
AGENT_HOME="/home/${AGENT_USER}"
OPENCLAW_DIR="${AGENT_HOME}/.openclaw"
VAULT="/var/lib/occlawshell"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MIN_NODE_MAJOR=20

# ── Colors ────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

ok()   { echo -e "  ${GREEN}✓${NC} $*"; }
warn() { echo -e "  ${YELLOW}⚠${NC} $*"; }
fail() { echo -e "  ${RED}✗${NC} $*"; }
info() { echo -e "  ${BLUE}ℹ${NC} $*"; }
step() { echo -e "\n${BLUE}[$1]${NC} $2"; }

# ── Pre-checks ────────────────────────────────────────────────

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: Run with sudo"
    echo "  sudo bash install.sh"
    exit 1
fi

# Detect the human user (who ran sudo)
HUMAN_USER="${SUDO_USER:-}"
if [[ -z "$HUMAN_USER" || "$HUMAN_USER" == "root" ]]; then
    # Try to find user who owns the OpenClaw installation
    for u in $(getent passwd | awk -F: '$3 >= 1000 && $3 < 60000 {print $1}'); do
        if [[ -d "/home/$u/.openclaw" ]]; then
            HUMAN_USER="$u"
            break
        fi
    done
fi

if [[ -z "$HUMAN_USER" ]]; then
    echo "ERROR: Cannot detect current user. Run with: sudo bash install.sh"
    exit 1
fi

HUMAN_HOME="/home/${HUMAN_USER}"
HUMAN_OPENCLAW="${HUMAN_HOME}/.openclaw"

echo ""
echo -e "${BLUE}╔══════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║         ClawShell Guardian v${VERSION} — Installer              ║${NC}"
echo -e "${BLUE}╚══════════════════════════════════════════════════════════╝${NC}"
echo ""
echo "  Detected user: ${HUMAN_USER}"
echo "  OpenClaw dir:  ${HUMAN_OPENCLAW}"
echo ""

if [[ ! -d "$HUMAN_OPENCLAW" ]]; then
    fail "OpenClaw installation not found at $HUMAN_OPENCLAW"
    echo "  Install OpenClaw first: npm install -g openclaw && openclaw setup"
    exit 1
fi

# ═══════════════════════════════════════════════════════════════
# Step 1: Create service users
# ═══════════════════════════════════════════════════════════════
step "1/9" "Creating service users"

if id "$AGENT_USER" &>/dev/null; then
    ok "User '$AGENT_USER' already exists"
else
    useradd -m -s /bin/bash --comment "OpenClaw Agent (no sudo)" "$AGENT_USER"
    ok "User '$AGENT_USER' created"
fi

if id occlawshell &>/dev/null; then
    ok "User 'occlawshell' already exists"
else
    useradd --system --shell /usr/sbin/nologin \
        --home-dir "$VAULT" --create-home \
        --comment "ClawShell Service" occlawshell
    ok "User 'occlawshell' created"
fi

# ═══════════════════════════════════════════════════════════════
# Step 2: Ensure Node.js is available globally
# ═══════════════════════════════════════════════════════════════
step "2/9" "Checking Node.js"

NODE_BIN=$(command -v node 2>/dev/null || true)
NODE_OK=false

if [[ -n "$NODE_BIN" ]]; then
    NODE_VER=$("$NODE_BIN" --version 2>/dev/null | sed 's/v//' | cut -d. -f1)
    if [[ "$NODE_VER" -ge "$MIN_NODE_MAJOR" ]]; then
        ok "Node.js v$("$NODE_BIN" --version) at $NODE_BIN"
        NODE_OK=true
    else
        warn "Node.js v$("$NODE_BIN" --version) is too old (need v${MIN_NODE_MAJOR}+)"
    fi
fi

# Check if ocagent can access node
if $NODE_OK; then
    if ! sudo -u "$AGENT_USER" "$NODE_BIN" --version &>/dev/null; then
        warn "Node.js exists but ocagent cannot access it (permission issue)"
        # Check if it's in a user-local path (nvm etc)
        if [[ "$NODE_BIN" == *"/.nvm/"* || "$NODE_BIN" == */home/*  ]]; then
            info "Node.js is in a user-local path, installing globally..."
            NODE_OK=false
        fi
    fi
fi

if ! $NODE_OK; then
    info "Installing Node.js v22 via NodeSource..."
    if command -v apt-get &>/dev/null; then
        curl -fsSL https://deb.nodesource.com/setup_22.x | bash - >/dev/null 2>&1
        apt-get install -y nodejs >/dev/null 2>&1
    elif command -v dnf &>/dev/null; then
        curl -fsSL https://rpm.nodesource.com/setup_22.x | bash - >/dev/null 2>&1
        dnf install -y nodejs >/dev/null 2>&1
    elif command -v yum &>/dev/null; then
        curl -fsSL https://rpm.nodesource.com/setup_22.x | bash - >/dev/null 2>&1
        yum install -y nodejs >/dev/null 2>&1
    else
        fail "Cannot install Node.js: unsupported package manager"
        echo "  Install Node.js v${MIN_NODE_MAJOR}+ manually, then re-run this script."
        exit 1
    fi
    NODE_BIN=$(command -v node)
    ok "Node.js v$(node --version) installed"
fi

# ═══════════════════════════════════════════════════════════════
# Step 3: Install OpenClaw under ocagent
# ═══════════════════════════════════════════════════════════════
step "3/9" "Installing OpenClaw under ${AGENT_USER}"

OPENCLAW_BIN=""
# Check if ocagent already has openclaw
OPENCLAW_BIN=$(sudo -u "$AGENT_USER" bash -lc 'command -v openclaw' 2>/dev/null) || true

if [[ -n "$OPENCLAW_BIN" && -x "$OPENCLAW_BIN" ]]; then
    OC_VER=$(sudo -u "$AGENT_USER" "$OPENCLAW_BIN" --version 2>/dev/null | head -1 || echo "unknown")
    ok "OpenClaw already installed: $OC_VER"
else
    info "Installing openclaw via npm..."
    # Set up npm global dir for ocagent
    sudo -u "$AGENT_USER" bash -c '
        mkdir -p ~/.npm-global
        npm config set prefix ~/.npm-global
        echo "export PATH=~/.npm-global/bin:\$PATH" >> ~/.bashrc
        export PATH=~/.npm-global/bin:$PATH
        npm install -g openclaw 2>&1 | tail -1
    '
    OPENCLAW_BIN=$(sudo -u "$AGENT_USER" bash -lc 'command -v openclaw' 2>/dev/null) || true
    if [[ -z "$OPENCLAW_BIN" ]]; then
        # Try common paths
        OPENCLAW_BIN="${AGENT_HOME}/.npm-global/bin/openclaw"
    fi
    if [[ -x "$OPENCLAW_BIN" ]]; then
        ok "OpenClaw installed at $OPENCLAW_BIN"
    else
        fail "Failed to install OpenClaw"
        echo "  Try manually: sudo -iu $AGENT_USER npm install -g openclaw"
        exit 1
    fi
fi

# ═══════════════════════════════════════════════════════════════
# Step 4: Migrate data from current user to ocagent
# ═══════════════════════════════════════════════════════════════
step "4/9" "Migrating OpenClaw data"

if [[ -d "${OPENCLAW_DIR}/memory" && -f "${OPENCLAW_DIR}/memory/main.sqlite" ]]; then
    ok "Data already exists at $OPENCLAW_DIR (skipping migration)"
else
    if [[ ! -d "$HUMAN_OPENCLAW" ]]; then
        warn "No source data to migrate"
    else
        info "Copying data from ${HUMAN_OPENCLAW} to ${OPENCLAW_DIR}..."

        # Initialize OpenClaw under ocagent first (creates directory structure)
        sudo -u "$AGENT_USER" bash -lc "openclaw setup --yes --non-interactive" 2>/dev/null || true

        copy_if_exists() {
            local src="$1" dst="$2"
            if [[ -e "$src" ]]; then
                mkdir -p "$(dirname "$dst")"
                cp -a "$src" "$dst"
            fi
        }

        # Core data
        copy_if_exists "${HUMAN_OPENCLAW}/memory"                        "${OPENCLAW_DIR}/memory"
        copy_if_exists "${HUMAN_OPENCLAW}/workspace/MEMORY.md"           "${OPENCLAW_DIR}/workspace/MEMORY.md"
        copy_if_exists "${HUMAN_OPENCLAW}/workspace/memory"              "${OPENCLAW_DIR}/workspace/memory"
        copy_if_exists "${HUMAN_OPENCLAW}/workspace/SOUL.md"             "${OPENCLAW_DIR}/workspace/SOUL.md"
        copy_if_exists "${HUMAN_OPENCLAW}/workspace/AGENTS.md"           "${OPENCLAW_DIR}/workspace/AGENTS.md"
        copy_if_exists "${HUMAN_OPENCLAW}/workspace/USER.md"             "${OPENCLAW_DIR}/workspace/USER.md"
        copy_if_exists "${HUMAN_OPENCLAW}/workspace/IDENTITY.md"         "${OPENCLAW_DIR}/workspace/IDENTITY.md"

        # Credentials & config
        copy_if_exists "${HUMAN_OPENCLAW}/credentials"                   "${OPENCLAW_DIR}/credentials"
        copy_if_exists "${HUMAN_OPENCLAW}/openclaw.json"                 "${OPENCLAW_DIR}/openclaw.json"
        copy_if_exists "${HUMAN_OPENCLAW}/.env"                          "${OPENCLAW_DIR}/.env"
        copy_if_exists "${HUMAN_OPENCLAW}/identity"                      "${OPENCLAW_DIR}/identity"

        # Agent state
        copy_if_exists "${HUMAN_OPENCLAW}/agents"                        "${OPENCLAW_DIR}/agents"
        copy_if_exists "${HUMAN_OPENCLAW}/telegram"                      "${OPENCLAW_DIR}/telegram"
        copy_if_exists "${HUMAN_OPENCLAW}/devices"                       "${OPENCLAW_DIR}/devices"
        copy_if_exists "${HUMAN_OPENCLAW}/cron"                          "${OPENCLAW_DIR}/cron"

        # Fix paths in config files
        if [[ -f "${OPENCLAW_DIR}/openclaw.json" ]]; then
            sed -i "s|/home/${HUMAN_USER}/|/home/${AGENT_USER}/|g" "${OPENCLAW_DIR}/openclaw.json"
        fi
        if [[ -f "${OPENCLAW_DIR}/cron/jobs.json" ]]; then
            sed -i "s|/home/${HUMAN_USER}/|/home/${AGENT_USER}/|g" "${OPENCLAW_DIR}/cron/jobs.json"
        fi

        # Fix ownership & permissions
        chown -R "${AGENT_USER}:${AGENT_USER}" "$OPENCLAW_DIR"
        chmod 700 "$OPENCLAW_DIR"
        chmod 700 "${OPENCLAW_DIR}/credentials" 2>/dev/null || true
        chmod 600 "${OPENCLAW_DIR}/.env" 2>/dev/null || true
        chmod 600 "${OPENCLAW_DIR}/openclaw.json" 2>/dev/null || true

        ok "Data migrated and paths fixed"
    fi
fi

# ═══════════════════════════════════════════════════════════════
# Step 5: Deploy ClawShell (vault, scripts, config)
# ═══════════════════════════════════════════════════════════════
step "5/9" "Deploying ClawShell"

# Vault structure
for dir in lkg snapshots postmortem audit config bin; do
    mkdir -p "${VAULT}/${dir}"
done
chown -R occlawshell:occlawshell "$VAULT"
chmod -R 700 "$VAULT"
chmod 711 "$VAULT"
chmod 755 "${VAULT}/bin"
ok "Vault structure at $VAULT"

# Deploy scripts
for script in "${SCRIPT_DIR}"/scripts/*.sh; do
    [[ -f "$script" ]] && cp "$script" "${VAULT}/bin/"
done
[[ -f "${SCRIPT_DIR}/clawshell.py" ]] && cp "${SCRIPT_DIR}/clawshell.py" "${VAULT}/bin/"
chown -R occlawshell:occlawshell "${VAULT}/bin/"
chmod 755 "${VAULT}/bin/"*.sh
chmod 644 "${VAULT}/bin/clawshell.py" 2>/dev/null || true
ok "Scripts deployed"

# Generate start-gateway.sh
OPENCLAW_NODE_DIR=$(dirname "$OPENCLAW_BIN" 2>/dev/null || true)
if [[ -n "$OPENCLAW_NODE_DIR" ]]; then
    cat > "${VAULT}/bin/start-gateway.sh" << GWEOF
#!/bin/bash
export PATH="${OPENCLAW_NODE_DIR}:\$PATH"
exec openclaw gateway --port "\${OPENCLAW_GATEWAY_PORT:-18789}"
GWEOF
    chown occlawshell:occlawshell "${VAULT}/bin/start-gateway.sh"
    chmod 755 "${VAULT}/bin/start-gateway.sh"
    ok "Generated start-gateway.sh"
else
    warn "start-gateway.sh not generated (openclaw path unknown)"
fi

# ═══════════════════════════════════════════════════════════════
# Step 6: ACLs, sudoers, immutable files
# ═══════════════════════════════════════════════════════════════
step "6/9" "Setting permissions"

# ACLs for occlawshell to read ocagent's data
setfacl -m u:occlawshell:x "${AGENT_HOME}"
setfacl -m u:occlawshell:rx "${OPENCLAW_DIR}"
if [[ -d "${OPENCLAW_DIR}/workspace" ]]; then
    setfacl -R -m u:occlawshell:rX "${OPENCLAW_DIR}/workspace"
    setfacl -R -d -m u:occlawshell:rX "${OPENCLAW_DIR}/workspace"
fi
if [[ -d "${OPENCLAW_DIR}/memory" ]]; then
    setfacl -R -m u:occlawshell:rX "${OPENCLAW_DIR}/memory"
    setfacl -R -d -m u:occlawshell:rX "${OPENCLAW_DIR}/memory"
fi
setfacl -d -m u:occlawshell:r "${OPENCLAW_DIR}"
for cfg in "${OPENCLAW_DIR}/openclaw.json" "${OPENCLAW_DIR}/.env"; do
    [[ -f "$cfg" ]] && setfacl -m u:occlawshell:r "$cfg"
done
ok "ACLs configured"

# Sudoers
cat > /etc/sudoers.d/occlawshell << 'SUDEOF'
occlawshell ALL=(root) NOPASSWD: /usr/bin/systemctl restart openclaw-gateway.service
occlawshell ALL=(root) NOPASSWD: /usr/bin/systemctl start oc-identity-lock.service
occlawshell ALL=(root) NOPASSWD: /usr/bin/systemctl start oc-identity-unlock.service
occlawshell ALL=(root) NOPASSWD: /var/lib/occlawshell/bin/auto-recover.sh
SUDEOF
chmod 440 /etc/sudoers.d/occlawshell
if visudo -c -f /etc/sudoers.d/occlawshell >/dev/null 2>&1; then
    ok "Sudoers rules deployed"
else
    fail "Invalid sudoers syntax, removing"
    rm -f /etc/sudoers.d/occlawshell
fi

# Immutable identity files
for f in SOUL.md AGENTS.md USER.md; do
    fpath="${OPENCLAW_DIR}/workspace/${f}"
    if [[ -f "$fpath" ]]; then
        chattr +i "$fpath" 2>/dev/null && ok "${f} set immutable" || warn "${f}: chattr failed"
    fi
done

# ═══════════════════════════════════════════════════════════════
# Step 7: Deploy systemd services
# ═══════════════════════════════════════════════════════════════
step "7/9" "Installing systemd services"

SYSTEMD_DIR="/etc/systemd/system"
for unit in "${SCRIPT_DIR}"/systemd/*.{service,timer}; do
    [[ -f "$unit" ]] || continue
    cp "$unit" "${SYSTEMD_DIR}/"
    ok "Deployed $(basename "$unit")"
done
systemctl daemon-reload
ok "systemd reloaded"

# Logrotate
if [[ -f "${SCRIPT_DIR}/logrotate/occlawshell" ]]; then
    cp "${SCRIPT_DIR}/logrotate/occlawshell" /etc/logrotate.d/occlawshell
    chmod 644 /etc/logrotate.d/occlawshell
    ok "Logrotate configured"
fi

# ═══════════════════════════════════════════════════════════════
# Step 8: Environment config
# ═══════════════════════════════════════════════════════════════
step "8/9" "Configuring ClawShell"

CLAWSHELL_ENV="${VAULT}/config/clawshell.env"
if [[ ! -f "$CLAWSHELL_ENV" ]]; then
    cat > "$CLAWSHELL_ENV" << ENVEOF
# ClawShell Configuration
# Telegram alerts auto-read from OpenClaw's openclaw.json (no config needed)
# To enable two-way Telegram bot, create a dedicated bot via @BotFather and set:
# CLAWSHELL_TG_TOKEN=
# CLAWSHELL_TG_CHAT=
GATEWAY_PORT=18789
AGENT_USER=${AGENT_USER}
OPENCLAW_DIR=${OPENCLAW_DIR}
OPENCLAW_BIN=${OPENCLAW_BIN:-}
ENVEOF
    chown occlawshell:occlawshell "$CLAWSHELL_ENV"
    chmod 600 "$CLAWSHELL_ENV"
    ok "clawshell.env created"
else
    ok "clawshell.env already exists (kept)"
fi

# ═══════════════════════════════════════════════════════════════
# Step 9: Migrate gateway & start services
# ═══════════════════════════════════════════════════════════════
step "9/9" "Starting services"

# Stop old user-level gateway if running
if sudo -u "$HUMAN_USER" systemctl --user is-active openclaw-gateway &>/dev/null 2>&1; then
    info "Stopping old gateway (user service)..."
    sudo -u "$HUMAN_USER" systemctl --user stop openclaw-gateway 2>/dev/null || true
    sudo -u "$HUMAN_USER" systemctl --user disable openclaw-gateway 2>/dev/null || true
    ok "Old user-level gateway stopped and disabled"
fi

# Start system-level gateway
systemctl enable openclaw-gateway.service 2>/dev/null || true
systemctl start openclaw-gateway.service 2>/dev/null || true
if systemctl is-active openclaw-gateway &>/dev/null; then
    ok "Gateway running (system service, User=${AGENT_USER})"
else
    warn "Gateway failed to start — check: journalctl -u openclaw-gateway -n 20"
fi

# Start ClawShell
systemctl enable --now oc-clawshell.service 2>/dev/null || true
if systemctl is-active oc-clawshell &>/dev/null; then
    ok "ClawShell watchdog running"
else
    warn "ClawShell failed to start — check: journalctl -u oc-clawshell -n 20"
fi

# Start timers
for timer in oc-snapshot.timer oc-healthcheck.timer oc-lkg-promote.timer; do
    systemctl enable --now "$timer" 2>/dev/null || true
done
ok "Snapshot, healthcheck, and LKG timers started"

# ═══════════════════════════════════════════════════════════════
# Done
# ═══════════════════════════════════════════════════════════════
echo ""
echo -e "${GREEN}╔══════════════════════════════════════════════════════════╗${NC}"
echo -e "${GREEN}║         ClawShell v${VERSION} — Installation Complete          ║${NC}"
echo -e "${GREEN}╚══════════════════════════════════════════════════════════╝${NC}"
echo ""
echo "  Three-user isolation active:"
echo "    ${HUMAN_USER}     — Admin (sudo)"
echo "    ${AGENT_USER}      — OpenClaw (no sudo)"
echo "    occlawshell  — ClawShell (limited sudo)"
echo ""
echo "  Services:"
echo "    Gateway:     sudo systemctl status openclaw-gateway"
echo "    Watchdog:    sudo systemctl status oc-clawshell"
echo "    Snapshots:   sudo systemctl list-timers oc-*"
echo ""
echo "  Status:        sudo python3 ${VAULT}/bin/clawshell.py --status"
echo "  Telegram:      alerts auto-configured from OpenClaw"
echo ""
echo "  Your original data at ${HUMAN_OPENCLAW} is preserved (not deleted)."
echo "  Remove it manually after confirming everything works."
echo ""
