#!/bin/bash
# BOI E2E tests — run inside the Docker container built from the repo root.
# Each test prints PASS or FAIL; exits 0 only when all pass.
set -uo pipefail

PASS=0
FAIL=0

ok()  { echo "PASS: $1"; ((PASS++));  }
nok() { echo "FAIL: $1"; ((FAIL++)); }

# ── Test 1: version ───────────────────────────────────────────────────────────
if boi version 2>&1 | grep -q "^boi "; then
    ok "boi version"
else
    nok "boi version"
fi

# ── Test 2: status (empty queue) ──────────────────────────────────────────────
if boi status 2>&1 | grep -qi "empty\|queue"; then
    ok "boi status (empty queue)"
else
    nok "boi status (empty queue)"
fi

# ── Test 3: dispatch ──────────────────────────────────────────────────────────
SPEC_ID=$(boi dispatch /home/boi/test-spec.yaml 2>&1)
if echo "$SPEC_ID" | grep -qE '^q-[0-9]+$'; then
    ok "boi dispatch (got $SPEC_ID)"
else
    nok "boi dispatch (unexpected output: $SPEC_ID)"
fi

# ── Test 4: status <id> ───────────────────────────────────────────────────────
if boi status "$SPEC_ID" 2>&1 | grep -q "queued"; then
    ok "boi status <id> shows queued"
else
    nok "boi status <id> shows queued"
fi

# ── Test 5: cancel ────────────────────────────────────────────────────────────
if boi cancel "$SPEC_ID" 2>&1 | grep -q "cancelled"; then
    ok "boi cancel"
else
    nok "boi cancel"
fi

# verify status changed to cancelled
if boi status "$SPEC_ID" 2>&1 | grep -q "cancelled"; then
    ok "status reflects cancellation"
else
    nok "status reflects cancellation"
fi

# ── Test 6: hook fires on dispatch ────────────────────────────────────────────
MARKER="/tmp/boi-hook-e2e-fired"
rm -f "$MARKER"
mkdir -p "$HOME/.boi"
cat > "$HOME/.boi/config.yaml" <<EOF
hooks:
  on_dispatch:
    command: "touch $MARKER"
    blocking: true
    timeout: 5
EOF

SPEC_ID2=$(boi dispatch /home/boi/test-spec.yaml 2>&1)
# Give the hook up to 2s
sleep 1
if [[ -f "$MARKER" ]]; then
    ok "on_dispatch hook fires"
else
    nok "on_dispatch hook fires (marker not found)"
fi

# Clean up the hook config so it doesn't interfere with remaining tests
rm -f "$HOME/.boi/config.yaml"

# ── Test 7: config display ────────────────────────────────────────────────────
CONFIG_OUT=$(boi config 2>&1)
if echo "$CONFIG_OUT" | grep -q "max_workers"; then
    ok "boi config shows max_workers"
else
    nok "boi config shows max_workers"
fi

if echo "$CONFIG_OUT" | grep -q "db_path"; then
    ok "boi config shows db_path"
else
    nok "boi config shows db_path"
fi

# Read a single key
if boi config max_workers 2>&1 | grep -qE '^[0-9]+$'; then
    ok "boi config max_workers returns integer"
else
    nok "boi config max_workers returns integer"
fi

# ── Test 8: bash wrapper delegates to Rust binary ────────────────────────────
if /home/boi/boi.sh version 2>&1 | grep -q "^boi "; then
    ok "bash wrapper delegates to Rust binary"
else
    nok "bash wrapper delegates to Rust binary"
fi

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo "Results: $PASS passed, $FAIL failed"
if [ "$FAIL" -eq 0 ]; then
    echo "PASS"
    exit 0
else
    echo "FAIL"
    exit 1
fi
