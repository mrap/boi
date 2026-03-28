#!/bin/bash
# test-upgrade-path.sh — End-to-end test for BOI upgrade path.
#
# Simulates a user who has an OLDER BOI install (pre-portability) and
# runs `install-public.sh --update` to upgrade to the latest version.
#
# Pre-upgrade state simulated:
#   - config.json WITHOUT runtime field (old format, only has workers)
#   - No guardrails.toml
#   - critic/config.json WITHOUT blast-radius check (old default checks only)
#
# After upgrade, verifies:
#   - config.json preserved (workers field still present, not overwritten)
#   - guardrails.toml was created with 'review' in pipeline
#   - critic/config.json has blast-radius AND preserved old checks
#   - boi doctor runs without crash
#   - boi status renders without crash
#
# Usage (Docker — preferred):
#   docker build -f tests/docker/Dockerfile.upgrade-path -t boi-upgrade-test . \
#     && docker run --rm boi-upgrade-test
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

# ── Step 2: Simulate OLD pre-portability state ────────────────────────────────
# Manually create the directory structure that an old BOI install would have
# produced: config.json without runtime field, no guardrails.toml,
# critic/config.json without blast-radius.
echo "==> Simulating pre-portability BOI state"

mkdir -p \
    "${BOI_HOME}/critic/custom" \
    "${BOI_HOME}/queue" \
    "${BOI_HOME}/events" \
    "${BOI_HOME}/logs" \
    "${BOI_HOME}/hooks" \
    "${BOI_HOME}/worktrees" \
    "${BOI_HOME}/projects"

# Old-style config: no "runtime" key, no "workers" int (workers come from
# `boi install --workers N` in a list format, not a bare int in config.json).
# We use a custom "org" field to test that --update preserves existing keys.
cat > "${BOI_HOME}/config.json" << 'EOF'
{
  "version": "1",
  "tool": "boi",
  "org": "acme-corp"
}
EOF

# Old-style critic config: no blast-radius check
cat > "${BOI_HOME}/critic/config.json" << 'EOF'
{
  "enabled": true,
  "trigger": "on_complete",
  "max_passes": 2,
  "checks": ["spec-integrity", "verify-commands", "code-quality", "completeness", "fleet-readiness"],
  "custom_checks_dir": "custom",
  "timeout_seconds": 600
}
EOF

# guardrails.toml intentionally absent (pre-portability state)

echo "==> Pre-upgrade state:"
echo "    config.json keys: $(python3 -c "import json; c=json.load(open('${BOI_HOME}/config.json')); print(list(c.keys()))")"
echo "    guardrails.toml: ABSENT"
echo "    critic checks: $(python3 -c "import json; c=json.load(open('${BOI_HOME}/critic/config.json')); print(','.join(c['checks']))")"

# ── Step 3: Run upgrade ───────────────────────────────────────────────────────
echo ""
echo "==> Running install-public.sh --update --no-symlink --no-plugin"
bash "${BOI_SRC}/install-public.sh" \
    --update \
    --no-symlink \
    --no-plugin
INSTALL_EXIT=$?

if [[ ${INSTALL_EXIT} -ne 0 ]]; then
    echo "[FAIL] install-public.sh --update exited with code ${INSTALL_EXIT}"
    exit 1
fi

echo ""
echo "==> Verifying post-upgrade state"
echo ""

# ── Verify 1: config.json preserved (custom field still present, not reset) ───
# We put a custom "org" field in the old config to verify --update doesn't
# overwrite config.json. If org is preserved, config was not clobbered.
CONFIG_FILE="${BOI_HOME}/config.json"
if [[ -f "${CONFIG_FILE}" ]]; then
    org_val=$(python3 -c "
import json
c = json.load(open('${CONFIG_FILE}'))
print(c.get('org', 'MISSING'))
" 2>/dev/null || echo "MISSING")
    if [[ "${org_val}" == "acme-corp" ]]; then
        pass "config.json preserved existing custom field (org=acme-corp)"
    else
        fail "config.json org='${org_val}' (expected 'acme-corp' — config was overwritten)"
    fi
else
    fail "config.json not found at ${CONFIG_FILE}"
fi

# ── Verify 2: config.json runtime not forcibly added on --update ──────────────
# Spec says: "On --update, if config.json already has a runtime setting, preserve it."
# Old config has no runtime field. After --update, config should still be intact.
# (The installer's seed_runtime_config() returns early on --update if config exists.)
runtime_val=$(python3 -c "
import json
c = json.load(open('${CONFIG_FILE}'))
runtime = c.get('runtime', {}).get('default', 'NOT_SET')
print(runtime)
" 2>/dev/null || echo "ERROR")
if [[ "${runtime_val}" == "NOT_SET" ]] || [[ "${runtime_val}" == "claude" ]]; then
    pass "config.json runtime handling correct after upgrade (${runtime_val})"
else
    fail "config.json runtime unexpected after --update: '${runtime_val}'"
fi

# ── Verify 3: guardrails.toml was created ─────────────────────────────────────
GUARDRAILS="${BOI_HOME}/guardrails.toml"
if [[ -f "${GUARDRAILS}" ]]; then
    if grep -q "review" "${GUARDRAILS}"; then
        pass "guardrails.toml was created with 'review' in pipeline"
    else
        fail "guardrails.toml exists but 'review' not in pipeline"
        echo "    guardrails content: $(cat "${GUARDRAILS}")"
    fi
else
    fail "guardrails.toml was NOT created by --update (expected seed_guardrails to run)"
fi

# ── Verify 4: blast-radius added, old checks preserved ───────────────────────
CRITIC_CONFIG="${BOI_HOME}/critic/config.json"
if [[ -f "${CRITIC_CONFIG}" ]]; then
    check_result=$(python3 - <<'PYEOF' 2>&1
import json, sys

with open("${CRITIC_CONFIG}") as f:
    c = json.load(f)
checks = c.get("checks", [])

errors = []
if "blast-radius" not in checks:
    errors.append("blast-radius not in checks")
if "spec-integrity" not in checks:
    errors.append("spec-integrity was removed (should be preserved)")
if "completeness" not in checks:
    errors.append("completeness was removed (should be preserved)")
if "fleet-readiness" not in checks:
    errors.append("fleet-readiness was removed (should be preserved)")

if errors:
    print("FAIL: " + "; ".join(errors))
else:
    print("ok")
PYEOF
)
    # Expand the config path and re-run (heredoc had literal ${CRITIC_CONFIG})
    check_result=$(python3 - <<PYEOF 2>&1
import json, sys

with open("${CRITIC_CONFIG}") as f:
    c = json.load(f)
checks = c.get("checks", [])

errors = []
if "blast-radius" not in checks:
    errors.append("blast-radius not in checks")
if "spec-integrity" not in checks:
    errors.append("spec-integrity was removed")
if "completeness" not in checks:
    errors.append("completeness was removed")
if "fleet-readiness" not in checks:
    errors.append("fleet-readiness was removed")

if errors:
    print("FAIL: " + "; ".join(errors))
else:
    print("ok")
PYEOF
)
    if [[ "${check_result}" == "ok" ]]; then
        pass "critic/config.json has blast-radius and preserved old checks"
    else
        fail "critic/config.json check failed: ${check_result}"
        echo "    checks found: $(python3 -c "import json; c=json.load(open('${CRITIC_CONFIG}')); print(c.get('checks'))")"
    fi
else
    fail "critic/config.json not found after --update"
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
