#!/usr/bin/env bash
set -uo pipefail

PASS=0
FAIL=0
BOI="boi"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n' "$*"; }

assert_pass() { PASS=$((PASS + 1)); green "  ✓ $*"; }
assert_fail() { FAIL=$((FAIL + 1)); red "  ✗ $*"; }

assert_exit() {
  local expected=$1 actual=$2 desc="$3"
  if [ "$actual" -eq "$expected" ]; then
    assert_pass "$desc (exit $actual)"
  else
    assert_fail "$desc (expected exit $expected, got $actual)"
  fi
}

assert_contains() {
  local output="$1" pattern="$2" desc="$3"
  if echo "$output" | grep -q "$pattern"; then
    assert_pass "$desc"
  else
    assert_fail "$desc — expected '$pattern' in output"
  fi
}

assert_not_contains() {
  local output="$1" pattern="$2" desc="$3"
  if echo "$output" | grep -q "$pattern"; then
    assert_fail "$desc — found '$pattern' (should be absent)"
  else
    assert_pass "$desc"
  fi
}

FIXTURES="$HOME/tests/fixtures"

echo ""
bold "═══════════════════════════════════════════"
bold "  BOI Rust Binary — E2E Tests"
bold "═══════════════════════════════════════════"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "1. Version and help"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI version 2>&1); rc=$?
assert_exit 0 $rc "boi version exits 0"
assert_contains "$out" "boi 1.0.0" "version shows 1.0.0"

out=$($BOI --help 2>&1); rc=$?
assert_exit 0 $rc "boi --help exits 0"
assert_contains "$out" "dispatch" "help shows dispatch"
assert_contains "$out" "status" "help shows status"
assert_contains "$out" "daemon" "help shows daemon"
assert_contains "$out" "workers" "help shows workers"
assert_contains "$out" "doctor" "help shows doctor"
assert_contains "$out" "spec" "help shows spec"
assert_contains "$out" "telemetry" "help shows telemetry"

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "2. Empty queue status"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI status 2>&1); rc=$?
assert_exit 0 $rc "status with empty queue exits 0"

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "3. Dispatch a simple spec"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI dispatch "$FIXTURES/valid-simple.yaml" 2>&1); rc=$?
assert_exit 0 $rc "dispatch simple spec exits 0"
SPEC_ID=$(echo "$out" | grep -o 'q-[0-9]*')
if [ -n "$SPEC_ID" ]; then
  assert_pass "dispatch returned spec ID: $SPEC_ID"
else
  assert_fail "dispatch did not return a spec ID"
  SPEC_ID="q-1"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "4. Status after dispatch"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI status 2>&1); rc=$?
assert_exit 0 $rc "status after dispatch exits 0"
assert_contains "$out" "E2E Test" "status shows dispatched spec title"

out=$($BOI status "$SPEC_ID" 2>&1); rc=$?
assert_exit 0 $rc "status with spec ID exits 0"
assert_contains "$out" "$SPEC_ID" "status by ID shows the spec"

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "5. Dispatch with flags"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI dispatch "$FIXTURES/valid-chain.yaml" --mode discover --max-iter 10 --timeout 5 --project "test-proj" 2>&1); rc=$?
assert_exit 0 $rc "dispatch with flags exits 0"
CHAIN_ID=$(echo "$out" | grep -o 'q-[0-9]*')
if [ -n "$CHAIN_ID" ]; then
  assert_pass "dispatch with flags returned ID: $CHAIN_ID"
else
  assert_fail "dispatch with flags did not return ID"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "6. Dispatch with --after dependency"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI dispatch "$FIXTURES/valid-simple.yaml" --after "$SPEC_ID" 2>&1); rc=$?
assert_exit 0 $rc "dispatch with --after exits 0"
DEP_ID=$(echo "$out" | grep -o 'q-[0-9]*')
if [ -n "$DEP_ID" ]; then
  assert_pass "dispatch with --after returned ID: $DEP_ID"
else
  assert_fail "dispatch with --after did not return ID"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "7. Dry run"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI dispatch "$FIXTURES/valid-simple.yaml" --dry-run 2>&1); rc=$?
assert_exit 0 $rc "dry-run exits 0"
assert_contains "$out" "dry-run\|valid\|Dry" "dry-run indicates validation"

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "8. Cancel"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI cancel "$SPEC_ID" 2>&1); rc=$?
assert_exit 0 $rc "cancel exits 0"
assert_contains "$out" "cancel" "cancel confirms cancellation"

out=$($BOI status "$SPEC_ID" 2>&1); rc=$?
assert_contains "$out" "cancel" "status shows cancelled"

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "9. Invalid spec rejection"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI dispatch "$FIXTURES/invalid-no-tasks.yaml" 2>&1); rc=$?
if [ "$rc" -ne 0 ]; then
  assert_pass "invalid spec (no tasks) rejected with exit $rc"
else
  assert_fail "invalid spec (no tasks) should be rejected"
fi

out=$($BOI dispatch "$FIXTURES/invalid-circular.yaml" 2>&1); rc=$?
if [ "$rc" -ne 0 ]; then
  assert_pass "invalid spec (circular dep) rejected with exit $rc"
else
  assert_fail "invalid spec (circular dep) should be rejected"
fi

out=$($BOI dispatch "/nonexistent/path.yaml" 2>&1); rc=$?
if [ "$rc" -ne 0 ]; then
  assert_pass "nonexistent spec file rejected with exit $rc"
else
  assert_fail "nonexistent spec file should be rejected"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "10. Spec management"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI dispatch "$FIXTURES/valid-chain.yaml" 2>&1)
MGMT_ID=$(echo "$out" | grep -o 'q-[0-9]*')

out=$($BOI spec "$MGMT_ID" 2>&1); rc=$?
assert_exit 0 $rc "spec view exits 0"
assert_contains "$out" "t-1" "spec shows task t-1"
assert_contains "$out" "t-2" "spec shows task t-2"

out=$($BOI spec "$MGMT_ID" add "New task" --spec "Do something new" --verify "true" 2>&1); rc=$?
assert_exit 0 $rc "spec add exits 0"

out=$($BOI spec "$MGMT_ID" skip t-3 2>&1); rc=$?
assert_exit 0 $rc "spec skip exits 0"

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "11. Config"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI config 2>&1); rc=$?
assert_exit 0 $rc "config exits 0"
assert_contains "$out" "max_workers" "config shows max_workers"
assert_contains "$out" "task_timeout" "config shows task_timeout"

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "12. Doctor"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI doctor 2>&1); rc=$?
assert_exit 0 $rc "doctor exits 0"
assert_contains "$out" "Database accessible" "doctor checks database"

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "13. Log and outputs"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI log "$MGMT_ID" 2>&1); rc=$?
# May be empty (no iterations yet), but should not crash
if [ "$rc" -eq 0 ] || [ "$rc" -eq 1 ]; then
  assert_pass "log command doesn't crash (exit $rc)"
else
  assert_fail "log command crashed with exit $rc"
fi

out=$($BOI outputs "$MGMT_ID" 2>&1); rc=$?
if [ "$rc" -eq 0 ]; then
  assert_pass "outputs command exits 0"
else
  assert_fail "outputs command failed with exit $rc"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "14. Workers"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI workers 2>&1); rc=$?
assert_exit 0 $rc "workers exits 0"

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "15. JSON output"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI status --json 2>&1); rc=$?
assert_exit 0 $rc "status --json exits 0"
if echo "$out" | python3 -c "import json,sys; json.load(sys.stdin)" 2>/dev/null; then
  assert_pass "status --json is valid JSON"
else
  assert_fail "status --json is not valid JSON"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "16. SQLite database integrity"
# ══════════════════════════════════════════════════════════════════════════════

DB_PATH=$(find ~/.boi -name "boi-rust.db" -o -name "boi.db" -not -path "*/worktrees/*" 2>/dev/null | head -1)
if [ -n "$DB_PATH" ]; then
  tables=$(sqlite3 "$DB_PATH" ".tables" 2>/dev/null)
  assert_contains "$tables" "specs" "DB has specs table"
  assert_contains "$tables" "tasks" "DB has tasks table"
  assert_contains "$tables" "iterations" "DB has iterations table"
  assert_contains "$tables" "events" "DB has events table"
  assert_contains "$tables" "workers" "DB has workers table"
  assert_contains "$tables" "processes" "DB has processes table"

  spec_count=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM specs;" 2>/dev/null)
  if [ "$spec_count" -gt 0 ]; then
    assert_pass "specs table has $spec_count entries"
  else
    assert_fail "specs table is empty"
  fi
else
  assert_fail "boi.db not found"
fi

echo ""
