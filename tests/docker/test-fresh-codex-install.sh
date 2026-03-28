#!/bin/bash
# test-fresh-codex-install.sh — End-to-end test for fresh Codex install path.
#
# Simulates: bash install-public.sh --runtime codex
# Verifies all post-install state is correct for a Codex user.
#
# Usage (Docker — preferred):
#   docker build -f tests/docker/Dockerfile.codex-install -t boi-codex-test . \
#     && docker run --rm boi-codex-test
#
set -uo pipefail

# ── Test harness ──────────────────────────────────────────────────────────────
PASS=0
FAIL=0

pass() { echo "[PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "[FAIL] $1"; FAIL=$((FAIL + 1)); }

BOI_HOME="${HOME}/.boi"
BOI_SRC="${BOI_HOME}/src"

# ── Step 1: Pre-seed ~/.boi/src to bypass git clone ───────────────────────────
# install-public.sh skips the git clone when BOI_SRC_DIR/.git already exists
# (and UPDATE_MODE is false / no --update flag). We copy the local source from
# the build context so the test works without network access and before t-8
# pushes the local fixes to GitHub.
echo "==> Pre-seeding ${BOI_SRC} from /boi-src"
mkdir -p "${BOI_HOME}"
cp -r /boi-src "${BOI_SRC}"

# ── Step 2: Run installer ─────────────────────────────────────────────────────
echo "==> Running install-public.sh --runtime codex --no-symlink --no-plugin"
bash "${BOI_SRC}/install-public.sh" \
    --runtime codex \
    --no-symlink \
    --no-plugin
INSTALL_EXIT=$?

if [[ ${INSTALL_EXIT} -ne 0 ]]; then
    echo "[FAIL] install-public.sh exited with code ${INSTALL_EXIT}"
    exit 1
fi

echo ""
echo "==> Verifying post-install state"
echo ""

# ── Verify 1: config.json has runtime = codex ─────────────────────────────────
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
        pass "config.json has runtime=codex"
    else
        fail "config.json runtime='${config_runtime}' (expected 'codex')"
    fi
else
    fail "config.json not found at ${CONFIG_FILE}"
fi

# ── Verify 2: guardrails.toml exists and contains 'review' ───────────────────
GUARDRAILS="${BOI_HOME}/guardrails.toml"
if [[ -f "${GUARDRAILS}" ]]; then
    if grep -q "review" "${GUARDRAILS}"; then
        pass "guardrails.toml exists and contains 'review' in pipeline"
    else
        fail "guardrails.toml exists but 'review' not found in pipeline"
    fi
else
    fail "guardrails.toml not found at ${GUARDRAILS}"
fi

# ── Verify 3: critic/config.json has blast-radius check ──────────────────────
CRITIC_CONFIG="${BOI_HOME}/critic/config.json"
if [[ -f "${CRITIC_CONFIG}" ]]; then
    if grep -q "blast-radius" "${CRITIC_CONFIG}"; then
        pass "critic/config.json contains blast-radius check"
    else
        fail "critic/config.json missing blast-radius check"
    fi
else
    fail "critic/config.json not found at ${CRITIC_CONFIG}"
fi

# ── Verify 4: Runtime abstraction loads and returns codex ────────────────────
runtime_name=$(python3 - <<PYEOF 2>&1
import sys
sys.path.insert(0, "${BOI_SRC}")
try:
    from lib.runtime import get_runtime
    r = get_runtime("codex")
    print(r.name)
except Exception as e:
    print("ERROR: " + str(e))
    sys.exit(1)
PYEOF
)
if [[ "${runtime_name}" == "codex" ]]; then
    pass "Runtime abstraction: get_runtime('codex').name = codex"
else
    fail "Runtime abstraction returned '${runtime_name}' (expected 'codex')"
fi

# ── Verify 5: boi doctor runs without Python/bash crash ──────────────────────
# Expected: some [FAIL] items (codex CLI not installed, no workers, daemon not running)
# Not expected: Python Traceback, SyntaxError, or bash unbound variable errors
doctor_output=$(bash "${BOI_SRC}/boi.sh" doctor 2>&1 || true)
if echo "${doctor_output}" | grep -qE "Traceback|SyntaxError|unbound variable|command not found: python"; then
    fail "boi doctor crashed with unexpected error"
    echo "--- boi doctor output ---"
    echo "${doctor_output}"
    echo "-------------------------"
else
    pass "boi doctor ran without crash"
fi

# Runtime should be reported as codex in doctor output
if echo "${doctor_output}" | grep -qi "codex"; then
    pass "boi doctor references codex runtime"
else
    fail "boi doctor output doesn't mention codex (check runtime detection)"
    echo "--- boi doctor output ---"
    echo "${doctor_output}"
    echo "-------------------------"
fi

# ── Verify 6: boi status renders without Python/bash crash ───────────────────
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
