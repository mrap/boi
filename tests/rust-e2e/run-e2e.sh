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

# Wait for a background PID to exit (up to $2 seconds, default 15).
wait_for_exit() {
  local pid=$1 timeout_secs=${2:-15}
  local max_ticks=$(( timeout_secs * 2 ))
  local tick=0
  while kill -0 "$pid" 2>/dev/null && [ "$tick" -lt "$max_ticks" ]; do
    sleep 0.5
    tick=$((tick + 1))
  done
}

# Stop daemon by PID: SIGTERM, wait, cleanup files
stop_daemon() {
  local pid=$1
  kill "$pid" 2>/dev/null
  wait_for_exit "$pid" 15
  if kill -0 "$pid" 2>/dev/null; then
    kill -9 "$pid" 2>/dev/null
  fi
  rm -f ~/.boi/daemon.pid ~/.boi/daemon.heartbeat
}

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

# ══════════════════════════════════════════════════════════════════════════════
bold "17. Daemon starts and creates PID file"
# ══════════════════════════════════════════════════════════════════════════════

rm -f ~/.boi/daemon.pid ~/.boi/daemon.heartbeat
$BOI daemon foreground &
DAEMON_PID=$!
sleep 3

if [ -f ~/.boi/daemon.pid ]; then
  assert_pass "daemon creates PID file"
  WRITTEN_PID=$(cat ~/.boi/daemon.pid)
  if [ "$WRITTEN_PID" = "$DAEMON_PID" ]; then
    assert_pass "PID file contains correct PID ($DAEMON_PID)"
  else
    assert_fail "PID file contains $WRITTEN_PID, expected $DAEMON_PID"
  fi
else
  assert_fail "daemon did not create PID file"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "18. Daemon heartbeat updates"
# ══════════════════════════════════════════════════════════════════════════════

HEARTBEAT1=$(cat ~/.boi/daemon.heartbeat 2>/dev/null || echo "none")
sleep 6
HEARTBEAT2=$(cat ~/.boi/daemon.heartbeat 2>/dev/null || echo "none")

if [ "$HEARTBEAT1" != "$HEARTBEAT2" ] && [ "$HEARTBEAT2" != "none" ]; then
  assert_pass "heartbeat file updates over time"
else
  assert_fail "heartbeat not updating (was: $HEARTBEAT1, now: $HEARTBEAT2)"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "19. Daemon picks up dispatched spec"
# ══════════════════════════════════════════════════════════════════════════════

cat > /tmp/daemon-e2e-spec.yaml <<'SPEC'
title: "Daemon E2E — Pickup Test"
mode: execute
tasks:
  - id: t-1
    title: "No-op task"
    status: PENDING
    spec: "Say hello"
    verify: "true"
SPEC

DAEMON_SPEC_OUT=$($BOI dispatch /tmp/daemon-e2e-spec.yaml 2>&1); rc=$?
assert_exit 0 $rc "dispatch during daemon run exits 0"
DAEMON_SPEC_ID=$(echo "$DAEMON_SPEC_OUT" | grep -o 'S[0-9]*')

if [ -n "$DAEMON_SPEC_ID" ]; then
  assert_pass "got spec ID: $DAEMON_SPEC_ID"

  # Wait up to 30s for daemon to pick it up (status != queued)
  PICKED_UP=false
  for i in $(seq 1 15); do
    SPEC_STATUS=$(sqlite3 ~/.boi/boi-rust.db "SELECT status FROM specs WHERE id='$DAEMON_SPEC_ID'" 2>/dev/null)
    if [ -n "$SPEC_STATUS" ] && [ "$SPEC_STATUS" != "queued" ]; then
      PICKED_UP=true
      break
    fi
    sleep 2
  done

  if [ "$PICKED_UP" = true ]; then
    assert_pass "daemon picked up spec (status: $SPEC_STATUS)"
  else
    assert_fail "daemon did not pick up spec within 30s (status: $SPEC_STATUS)"
  fi
else
  assert_fail "dispatch did not return spec ID"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "20. Daemon stops cleanly on SIGTERM"
# ══════════════════════════════════════════════════════════════════════════════

# Give worker time to finish before stopping
sleep 3

kill $DAEMON_PID 2>/dev/null
wait_for_exit $DAEMON_PID 15

if ! kill -0 $DAEMON_PID 2>/dev/null; then
  assert_pass "daemon process exited after SIGTERM"
else
  assert_fail "daemon still running after SIGTERM"
  kill -9 $DAEMON_PID 2>/dev/null
fi

if [ ! -f ~/.boi/daemon.pid ]; then
  assert_pass "PID file cleaned up after shutdown"
else
  assert_fail "PID file still exists after shutdown"
  rm -f ~/.boi/daemon.pid
fi

if [ ! -f ~/.boi/daemon.heartbeat ]; then
  assert_pass "heartbeat file cleaned up after shutdown"
else
  assert_fail "heartbeat file still exists after shutdown"
  rm -f ~/.boi/daemon.heartbeat
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "21. Stale spec recovery on daemon restart"
# ══════════════════════════════════════════════════════════════════════════════

# Insert a spec stuck in "assigning" state (simulating a crash)
sqlite3 ~/.boi/boi-rust.db "INSERT OR REPLACE INTO specs (id, title, status, priority, queued_at) VALUES ('stale-test', 'Stale Recovery Test', 'assigning', 100, datetime('now'))"

PRE_STATUS=$(sqlite3 ~/.boi/boi-rust.db "SELECT status FROM specs WHERE id='stale-test'" 2>/dev/null)
if [ "$PRE_STATUS" = "assigning" ]; then
  assert_pass "inserted stale spec with status=assigning"
else
  assert_fail "failed to insert stale spec (status=$PRE_STATUS)"
fi

# Capture daemon stderr to verify recovery message
$BOI daemon foreground 2>/tmp/daemon-recovery.log &
DAEMON_PID2=$!
sleep 2

if grep -q "recovered.*stuck spec" /tmp/daemon-recovery.log; then
  assert_pass "daemon logged stuck spec recovery"
else
  assert_fail "daemon did not log stuck spec recovery"
fi

stop_daemon $DAEMON_PID2

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "22. Daemon rejects second instance"
# ══════════════════════════════════════════════════════════════════════════════

rm -f ~/.boi/daemon.pid ~/.boi/daemon.heartbeat
$BOI daemon foreground &
DAEMON_PID3=$!
sleep 2

out=$($BOI daemon foreground 2>&1); rc=$?
if [ $rc -ne 0 ]; then
  assert_pass "second daemon instance rejected (exit $rc)"
else
  assert_fail "second daemon instance was not rejected"
fi

stop_daemon $DAEMON_PID3

echo ""

# ══════════════════════════════════════════════════════════════════════════════
# Summary
# ══════════════════════════════════════════════════════════════════════════════

echo ""
bold "═══════════════════════════════════════════"
total=$((PASS + FAIL))
if [ $FAIL -eq 0 ]; then
  green "  ALL $total TESTS PASSED"
else
  red "  $FAIL/$total TESTS FAILED"
fi
bold "═══════════════════════════════════════════"
echo ""

exit $FAIL
