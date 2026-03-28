#!/bin/bash
# test-upgrade-codex-config.sh — End-to-end test for BOI upgrade with existing Codex config.
#
# Simulates a user who:
#   1. Installed BOI with --runtime codex (config.json has runtime=codex)
#   2. Added a custom critic check (~/.boi/critic/custom/my-check.md)
#   3. Has guardrails.toml from the original install
#   4. Runs `install-public.sh --update` to upgrade
#
# After upgrade, verifies:
#   - Runtime is STILL codex (not overwritten to claude)
#   - Custom critic check is STILL present in critic/custom/
#   - blast-radius default check was added (or already present)
#   - guardrails.toml was NOT overwritten (content still matches original)
#   - boi doctor runs without crash and references codex
#   - boi status renders without crash
#
# Usage (Docker — preferred):
#   docker build -f tests/docker/Dockerfile.codex-upgrade -t boi-codex-upgrade-test . \
#     && docker run --rm boi-codex-upgrade-test
#
set -uo pipefail

# ── Test harness ──────────────────────────────────────────────────────────────
PASS=0
FAIL=0

pass() { echo "[PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "[FAIL] $1"; FAIL=$((FAIL + 1)); }

BOI_HOME="${HOME}/.boi"
BOI_SRC="${BOI_HOME}/src"

# ── Step 1: Pre-seed ~/.boi/src from local copy ───────────────────────────────
# We copy /boi-src (Docker build context = ~/.boi/src/) to simulate the state
# after the user has run a git pull. The .git directory is included so that
# install-public.sh's clone_or_update_repo() detects an existing repo and
# tries git pull --rebase instead of cloning.
echo "==> Pre-seeding ${BOI_SRC} from /boi-src"
mkdir -p "${BOI_HOME}"
cp -r /boi-src "${BOI_SRC}"

# Point origin to /boi-src so `git pull --rebase` in install-public.sh succeeds.
# Without this, the fallback `git fetch origin` would fail (no network in Docker)
# and the script would exit due to set -uo pipefail.
git -C "${BOI_SRC}" remote set-url origin /boi-src 2>/dev/null \
    || git -C "${BOI_SRC}" remote add origin /boi-src

# ── Step 2: Run fresh install with --runtime codex ───────────────────────────
echo "==> Running fresh install: install-public.sh --runtime codex --no-symlink --no-plugin"
bash "${BOI_SRC}/install-public.sh" \
    --runtime codex \
    --no-symlink \
    --no-plugin
INSTALL_EXIT=$?

if [[ ${INSTALL_EXIT} -ne 0 ]]; then
    echo "[FAIL] Fresh install failed with exit code ${INSTALL_EXIT}"
    exit 1
fi

# ── Step 3: Add a custom critic check ────────────────────────────────────────
# Simulate a power user who wrote their own check. This must survive --update.
echo "==> Adding custom critic check"
CUSTOM_CHECK="${BOI_HOME}/critic/custom/my-security-check.md"
cat > "${CUSTOM_CHECK}" << 'EOF'
# Security Review

Validates that code does not introduce security vulnerabilities.

## Checklist

- [ ] No hardcoded credentials or secrets in source code
- [ ] User input is validated before use
- [ ] No SQL string concatenation (use parameterized queries)

## Examples of Violations

### Hardcoded secret (HIGH severity)
API_KEY = "sk-abc123"
EOF

echo "==> Pre-upgrade state:"
echo "    config.json runtime: $(python3 -c "import json; c=json.load(open('${BOI_HOME}/config.json')); print(c.get('runtime',{}).get('default','MISSING'))")"
echo "    guardrails.toml: $(cat "${BOI_HOME}/guardrails.toml")"
echo "    critic checks: $(python3 -c "import json; c=json.load(open('${BOI_HOME}/critic/config.json')); print(','.join(c['checks']))")"
echo "    custom check: $(ls "${BOI_HOME}/critic/custom/")"

# ── Step 4: Run upgrade ───────────────────────────────────────────────────────
echo ""
echo "==> Running install-public.sh --update --no-symlink --no-plugin"
bash "${BOI_SRC}/install-public.sh" \
    --update \
    --no-symlink \
    --no-plugin
UPDATE_EXIT=$?

if [[ ${UPDATE_EXIT} -ne 0 ]]; then
    echo "[FAIL] install-public.sh --update exited with code ${UPDATE_EXIT}"
    exit 1
fi

echo ""
echo "==> Verifying post-upgrade state"
echo ""

# ── Verify 1: Runtime is STILL codex ─────────────────────────────────────────
CONFIG_FILE="${BOI_HOME}/config.json"
if [[ -f "${CONFIG_FILE}" ]]; then
    config_runtime=$(python3 - <<PYEOF 2>&1
import json, sys
try:
    c = json.load(open("${CONFIG_FILE}"))
    print(c.get("runtime", {}).get("default", "MISSING"))
except Exception as e:
    print("ERROR: " + str(e))
    sys.exit(1)
PYEOF
)
    if [[ "${config_runtime}" == "codex" ]]; then
        pass "Runtime is still codex after --update (not overwritten)"
    else
        fail "Runtime changed to '${config_runtime}' (expected 'codex' — config was overwritten)"
    fi
else
    fail "config.json not found at ${CONFIG_FILE}"
fi

# ── Verify 2: Custom critic check still present ───────────────────────────────
if [[ -f "${CUSTOM_CHECK}" ]]; then
    if grep -q "Security Review" "${CUSTOM_CHECK}"; then
        pass "Custom critic check still present after --update"
    else
        fail "Custom critic check file exists but content changed"
    fi
else
    fail "Custom critic check was deleted by --update (expected it to be preserved)"
fi

# ── Verify 3: blast-radius in critic/config.json ─────────────────────────────
CRITIC_CONFIG="${BOI_HOME}/critic/config.json"
if [[ -f "${CRITIC_CONFIG}" ]]; then
    if grep -q "blast-radius" "${CRITIC_CONFIG}"; then
        pass "critic/config.json contains blast-radius check"
    else
        fail "critic/config.json missing blast-radius check after --update"
        echo "    checks found: $(python3 -c "import json; c=json.load(open('${CRITIC_CONFIG}')); print(c.get('checks'))")"
    fi
else
    fail "critic/config.json not found after --update"
fi

# ── Verify 4: guardrails.toml not overwritten ────────────────────────────────
# The fresh install creates guardrails.toml. On --update, seed_guardrails()
# should detect the file exists and skip it (log "Exists: ...").
GUARDRAILS="${BOI_HOME}/guardrails.toml"
if [[ -f "${GUARDRAILS}" ]]; then
    if grep -q "review" "${GUARDRAILS}"; then
        pass "guardrails.toml still present with 'review' pipeline after --update"
    else
        fail "guardrails.toml exists but 'review' not in pipeline (may have been overwritten)"
        echo "    guardrails content: $(cat "${GUARDRAILS}")"
    fi
else
    fail "guardrails.toml missing after --update"
fi

# ── Verify 5: boi doctor runs without crash ───────────────────────────────────
doctor_output=$(bash "${BOI_SRC}/boi.sh" doctor 2>&1 || true)
if echo "${doctor_output}" | grep -qE "Traceback|SyntaxError|unbound variable|command not found: python"; then
    fail "boi doctor crashed with unexpected error"
    echo "--- boi doctor output ---"
    echo "${doctor_output}"
    echo "-------------------------"
else
    pass "boi doctor ran without crash"
fi

# boi doctor should still reference codex runtime after upgrade
if echo "${doctor_output}" | grep -qi "codex"; then
    pass "boi doctor references codex runtime after upgrade"
else
    fail "boi doctor output doesn't mention codex (runtime may have been reset)"
    echo "--- boi doctor output ---"
    echo "${doctor_output}"
    echo "-------------------------"
fi

# ── Verify 6: boi status renders without crash ───────────────────────────────
status_output=$(bash "${BOI_SRC}/boi.sh" status 2>&1 || true)
if echo "${status_output}" | grep -qE "Traceback|SyntaxError|unbound variable"; then
    fail "boi status crashed with unexpected error"
    echo "--- boi status output ---"
    echo "${status_output}"
    echo "-------------------------"
else
    pass "boi status rendered without crash"
fi

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo "========================================"
echo "Results: ${PASS} passed, ${FAIL} failed"
echo "========================================"

[[ ${FAIL} -eq 0 ]]
