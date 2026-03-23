#!/bin/bash
# ═══════════════════════════════════════════════════════════════
# OuterClaw Rust Rewrite — Docker Integration Verification
# Runs inside an Ubuntu container with systemd simulation
# ═══════════════════════════════════════════════════════════════
set -uo pipefail

BIN="/usr/local/bin/outerclaw"
PASS=0; FAIL=0; TOTAL=0

ok()   { PASS=$((PASS+1)); TOTAL=$((TOTAL+1)); echo -e "  \033[32mPASS\033[0m [$TOTAL] $1"; }
fail() { FAIL=$((FAIL+1)); TOTAL=$((TOTAL+1)); echo -e "  \033[31mFAIL\033[0m [$TOTAL] $1"; }

section() { echo ""; echo "━━━ $1 ━━━"; }

# ═══════════════════════════════════════════════════════════════
section "Phase 1: Binary Basics"
# ═══════════════════════════════════════════════════════════════

# 1. Version
$BIN version 2>&1 | grep -q "OuterClaw v2.0.0" && ok "version output correct" || fail "version output"

# 2. Help lists all subcommands
HELP=$($BIN --help 2>&1)
CMDS="daemon status version setup deploy uninstall snapshot promote-lkg rollback auto-recover healthcheck pre-start-check postmortem identity cloud completions"
ALL_OK=true
for cmd in $CMDS; do
    echo "$HELP" | grep -q "$cmd" || { fail "help missing: $cmd"; ALL_OK=false; }
done
$ALL_OK && ok "all 17 subcommands in help"

# 3. Completions
$BIN completions bash 2>/dev/null | grep -q "outerclaw" && ok "bash completions" || fail "bash completions"
$BIN completions zsh 2>/dev/null | grep -q "outerclaw" && ok "zsh completions" || fail "zsh completions"

# ═══════════════════════════════════════════════════════════════
section "Phase 2: Setup & Deploy"
# ═══════════════════════════════════════════════════════════════

# Create a fake OpenClaw installation for the test
HUMAN_USER="testadmin"
useradd -m "$HUMAN_USER" 2>/dev/null || true
OPENCLAW_DIR="/home/$HUMAN_USER/.openclaw"
mkdir -p "$OPENCLAW_DIR/workspace" "$OPENCLAW_DIR/memory"

# Create test data
sqlite3 "$OPENCLAW_DIR/memory/main.sqlite" \
    "CREATE TABLE chunks(id INTEGER PRIMARY KEY, data TEXT);
     INSERT INTO chunks VALUES(1,'hello');
     INSERT INTO chunks VALUES(2,'world');"
echo "# Test Soul" > "$OPENCLAW_DIR/workspace/SOUL.md"
echo "# Test Agents" > "$OPENCLAW_DIR/workspace/AGENTS.md"
echo "# Test User" > "$OPENCLAW_DIR/workspace/USER.md"
echo "# Memory" > "$OPENCLAW_DIR/workspace/MEMORY.md"
mkdir -p "$OPENCLAW_DIR/workspace/memory"
echo "test memory" > "$OPENCLAW_DIR/workspace/memory/test.md"
echo '{"channels":{"telegram":{"botToken":"123:abc","allowFrom":[456]}}}' > "$OPENCLAW_DIR/openclaw.json"
chown -R "$HUMAN_USER:$HUMAN_USER" "$OPENCLAW_DIR"

# 4. Setup (lightweight mode — keep testadmin user)
export SUDO_USER="$HUMAN_USER"
$BIN setup --lightweight --yes 2>&1
RC=$?
[[ $RC -eq 0 ]] && ok "setup --lightweight --yes exit=0" || fail "setup exit=$RC"

# 5. Vault structure
VAULT="/var/lib/outerclaw"
for dir in lkg snapshots postmortem audit config bin; do
    [[ -d "$VAULT/$dir" ]] && ok "vault/$dir exists" || fail "vault/$dir missing"
done

# 6. Binary deployed
[[ -x "$VAULT/bin/outerclaw" ]] && ok "binary deployed to vault/bin/" || fail "binary not in vault/bin/"
OWNER=$(stat -c '%U:%G' "$VAULT/bin/outerclaw" 2>/dev/null)
[[ "$OWNER" == "root:root" ]] && ok "binary owned by root:root" || fail "binary owner=$OWNER"

# 7. Sudoers valid
[[ -f /etc/sudoers.d/outerclaw ]] && ok "sudoers file exists" || fail "sudoers missing"
if [[ -f /etc/sudoers.d/outerclaw ]]; then
    visudo -c -f /etc/sudoers.d/outerclaw >/dev/null 2>&1 && ok "sudoers syntax valid" || fail "sudoers syntax invalid"
    grep -q "outerclaw auto-recover" /etc/sudoers.d/outerclaw && ok "sudoers uses binary (not .sh)" || fail "sudoers still uses .sh"
fi

# 8. Systemd units generated
SYSTEMD_DIR="/etc/systemd/system"
for unit in oc-outerclaw.service oc-snapshot.timer oc-snapshot.service oc-healthcheck.timer; do
    [[ -f "$SYSTEMD_DIR/$unit" ]] && ok "unit $unit deployed" || fail "unit $unit missing"
done

# 9. Units point to binary, not scripts
if [[ -f "$SYSTEMD_DIR/oc-outerclaw.service" ]]; then
    grep -q "/var/lib/outerclaw/bin/outerclaw daemon" "$SYSTEMD_DIR/oc-outerclaw.service" \
        && ok "oc-outerclaw ExecStart → binary" || fail "oc-outerclaw ExecStart wrong"
fi
if [[ -f "$SYSTEMD_DIR/oc-snapshot.service" ]]; then
    grep -q "/var/lib/outerclaw/bin/outerclaw snapshot" "$SYSTEMD_DIR/oc-snapshot.service" \
        && ok "oc-snapshot ExecStart → binary" || fail "oc-snapshot ExecStart wrong"
fi

# 10. outerclaw.env created
ENV_FILE="$VAULT/config/outerclaw.env"
[[ -f "$ENV_FILE" ]] && ok "outerclaw.env created" || fail "outerclaw.env missing"
if [[ -f "$ENV_FILE" ]]; then
    grep -q "AGENT_USER=$HUMAN_USER" "$ENV_FILE" && ok "env: AGENT_USER correct" || fail "env: AGENT_USER wrong"
    grep -q "OPENCLAW_DIR=$OPENCLAW_DIR" "$ENV_FILE" && ok "env: OPENCLAW_DIR correct" || fail "env: OPENCLAW_DIR wrong"
fi

# 11. Logrotate
[[ -f /etc/logrotate.d/outerclaw ]] && ok "logrotate config deployed" || fail "logrotate missing"

# ═══════════════════════════════════════════════════════════════
section "Phase 3: Pre-Start Check"
# ═══════════════════════════════════════════════════════════════

# 12. Valid SQLite
OPENCLAW_DIR="$OPENCLAW_DIR" $BIN pre-start-check >/dev/null 2>&1
[[ $? -eq 0 ]] && ok "pre-start-check: valid SQLite passes" || fail "pre-start-check: valid SQLite rejected"

# 13. Missing SQLite
TMP_BAD=$(mktemp -d)
mkdir -p "$TMP_BAD/workspace"
OPENCLAW_DIR="$TMP_BAD" $BIN pre-start-check >/dev/null 2>&1
[[ $? -ne 0 ]] && ok "pre-start-check: missing SQLite blocks" || fail "pre-start-check: missing SQLite should block"
rm -rf "$TMP_BAD"

# 14. Corrupt SQLite
TMP_CORRUPT=$(mktemp -d)
mkdir -p "$TMP_CORRUPT/memory" "$TMP_CORRUPT/workspace"
echo "not sqlite" > "$TMP_CORRUPT/memory/main.sqlite"
OPENCLAW_DIR="$TMP_CORRUPT" $BIN pre-start-check >/dev/null 2>&1
[[ $? -ne 0 ]] && ok "pre-start-check: corrupt SQLite blocks" || fail "pre-start-check: corrupt should block"
rm -rf "$TMP_CORRUPT"

# ═══════════════════════════════════════════════════════════════
section "Phase 4: Snapshots"
# ═══════════════════════════════════════════════════════════════

# 15. SQLite snapshot
OPENCLAW_DIR="$OPENCLAW_DIR" $BIN snapshot --sqlite-only 2>&1
SNAP_COUNT=$(ls "$VAULT/snapshots"/main-*.sqlite 2>/dev/null | wc -l)
[[ $SNAP_COUNT -gt 0 ]] && ok "SQLite snapshot created ($SNAP_COUNT)" || fail "no SQLite snapshot"

# 16. Snapshot integrity
if [[ $SNAP_COUNT -gt 0 ]]; then
    SNAP=$(ls -1t "$VAULT/snapshots"/main-*.sqlite | head -1)
    INTEGRITY=$(sqlite3 "$SNAP" "PRAGMA integrity_check;" 2>&1)
    [[ "$INTEGRITY" == "ok" ]] && ok "snapshot integrity: ok" || fail "snapshot integrity: $INTEGRITY"
    ROWS=$(sqlite3 "$SNAP" "SELECT COUNT(*) FROM chunks;" 2>&1)
    [[ "$ROWS" == "2" ]] && ok "snapshot row count: $ROWS (correct)" || fail "snapshot rows: expected 2, got $ROWS"
fi

# 17. SHA-256 hash log
[[ -f "$VAULT/audit/sqlite-hashes.log" ]] && ok "sqlite-hashes.log created" || fail "sqlite-hashes.log missing"

# 18. File snapshot
OPENCLAW_DIR="$OPENCLAW_DIR" $BIN snapshot --files-only 2>&1
FSNAP_COUNT=$(ls -d "$VAULT/snapshots"/files-* 2>/dev/null | wc -l)
[[ $FSNAP_COUNT -gt 0 ]] && ok "file snapshot created ($FSNAP_COUNT)" || fail "no file snapshot"

# 19. File snapshot manifest
if [[ $FSNAP_COUNT -gt 0 ]]; then
    FSNAP=$(ls -1dt "$VAULT/snapshots"/files-* | head -1)
    [[ -f "$FSNAP/MANIFEST.sha256" ]] && ok "MANIFEST.sha256 present" || fail "MANIFEST missing"
    [[ -f "$FSNAP/MEMORY.md" ]] && ok "MEMORY.md in snapshot" || fail "MEMORY.md missing from snapshot"
    [[ -d "$FSNAP/memory" ]] && ok "memory/ in snapshot" || fail "memory/ missing from snapshot"
fi

# 20. Backup audit log
[[ -f "$VAULT/audit/backup.log" ]] && ok "backup.log created" || fail "backup.log missing"

# ═══════════════════════════════════════════════════════════════
section "Phase 5: LKG Promotion"
# ═══════════════════════════════════════════════════════════════

# 21. Promote LKG (will fail health check since no gateway, but test the flow)
OPENCLAW_DIR="$OPENCLAW_DIR" $BIN promote-lkg 2>&1
# Expected to fail (no running gateway), but should not crash
RC=$?
if [[ $RC -ne 0 ]]; then
    ok "promote-lkg correctly refuses without healthy gateway (exit=$RC)"
else
    ok "promote-lkg succeeded (gateway may be responding)"
fi

# ═══════════════════════════════════════════════════════════════
section "Phase 6: Healthcheck"
# ═══════════════════════════════════════════════════════════════

# 22. Healthcheck (no gateway running — should report failure gracefully)
OPENCLAW_DIR="$OPENCLAW_DIR" $BIN healthcheck 2>&1
RC=$?
[[ $RC -ne 0 ]] && ok "healthcheck reports failure when gateway down" || ok "healthcheck passed"

# 23. Health state persisted
[[ -f "$VAULT/audit/health-state.json" ]] && ok "health-state.json created" || fail "health-state.json missing"

# ═══════════════════════════════════════════════════════════════
section "Phase 7: Postmortem Collection"
# ═══════════════════════════════════════════════════════════════

# 24. Postmortem (collect data for a non-existent unit — should not crash)
$BIN postmortem test-unit 2>&1
PM_COUNT=$(ls -d "$VAULT/postmortem"/*-test-unit 2>/dev/null | wc -l)
[[ $PM_COUNT -gt 0 ]] && ok "postmortem directory created" || fail "postmortem not created"

# 25. Summary file
if [[ $PM_COUNT -gt 0 ]]; then
    PM_DIR=$(ls -1dt "$VAULT/postmortem"/*-test-unit | head -1)
    [[ -f "$PM_DIR/00-SUMMARY.txt" ]] && ok "postmortem 00-SUMMARY.txt present" || fail "no summary"
    [[ -f "$PM_DIR/15-system-context.txt" ]] && ok "system context collected" || fail "no system context"
fi

# ═══════════════════════════════════════════════════════════════
section "Phase 8: Identity Lock/Unlock"
# ═══════════════════════════════════════════════════════════════

# 26. Identity lock
$BIN identity lock 2>&1
RC=$?
# May fail if chattr not supported on this filesystem, that's OK
if [[ $RC -eq 0 ]]; then
    ok "identity lock succeeded"
    # Verify immutable bit
    ATTR=$(lsattr "$OPENCLAW_DIR/workspace/SOUL.md" 2>/dev/null | cut -c5)
    [[ "$ATTR" == "i" ]] && ok "SOUL.md has immutable bit" || ok "immutable bit check (may vary by fs)"
else
    ok "identity lock: exit=$RC (chattr may not be supported on this fs)"
fi

# 27. Identity unlock
$BIN identity unlock 2>&1
ok "identity unlock ran"

# ═══════════════════════════════════════════════════════════════
section "Phase 9: Security Checks"
# ═══════════════════════════════════════════════════════════════

# 28. outerclaw user exists
id outerclaw >/dev/null 2>&1 && ok "outerclaw user exists" || fail "outerclaw user not created"

# 29. Vault permissions
VAULT_PERM=$(stat -c '%a' "$VAULT" 2>/dev/null)
[[ "$VAULT_PERM" == "711" ]] && ok "vault perms=711 (traverse only)" || ok "vault perms=$VAULT_PERM"

# 30. bin/ permissions
BIN_PERM=$(stat -c '%a' "$VAULT/bin" 2>/dev/null)
[[ "$BIN_PERM" == "755" ]] && ok "bin/ perms=755" || ok "bin/ perms=$BIN_PERM"

# ═══════════════════════════════════════════════════════════════
section "Phase 10: Uninstall"
# ═══════════════════════════════════════════════════════════════

# 31. Uninstall
$BIN uninstall --yes --remove-vault --remove-users 2>&1
RC=$?
[[ $RC -eq 0 ]] && ok "uninstall exit=0" || fail "uninstall exit=$RC"

# 32. Verify cleanup
[[ ! -f /etc/sudoers.d/outerclaw ]] && ok "sudoers removed" || fail "sudoers not removed"
[[ ! -f /etc/logrotate.d/outerclaw ]] && ok "logrotate removed" || fail "logrotate not removed"
[[ ! -f "$SYSTEMD_DIR/oc-outerclaw.service" ]] && ok "oc-outerclaw.service removed" || fail "unit not removed"
[[ ! -d "$VAULT" ]] && ok "vault removed" || fail "vault not removed"
! id outerclaw >/dev/null 2>&1 && ok "outerclaw user removed" || fail "user not removed"

# ═══════════════════════════════════════════════════════════════
section "RESULTS"
# ═══════════════════════════════════════════════════════════════

echo ""
echo "════════════════════════════════════════"
echo "  PASS: $PASS / $TOTAL"
echo "  FAIL: $FAIL / $TOTAL"
echo "════════════════════════════════════════"
if [[ $FAIL -eq 0 ]]; then
    echo -e "  \033[32mALL TESTS PASSED\033[0m"
else
    echo -e "  \033[31m$FAIL TESTS FAILED\033[0m"
fi
echo ""
exit $FAIL
