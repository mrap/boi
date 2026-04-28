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

# ══════════════════════════════════════════════════════════════════════════════
bold "17. Concurrent spec execution"
# ══════════════════════════════════════════════════════════════════════════════

# Clean any stale state from prior daemon runs
rm -f /tmp/boi-concurrent-a.txt /tmp/boi-concurrent-b.txt /tmp/boi-concurrent-c.txt
$BOI stop 2>/dev/null || true
rm -f ~/.boi/daemon.pid ~/.boi/daemon.heartbeat

# Dispatch 3 specs simultaneously
out_a=$($BOI dispatch "$FIXTURES/concurrent-a.yaml" 2>&1); rc_a=$?
out_b=$($BOI dispatch "$FIXTURES/concurrent-b.yaml" 2>&1); rc_b=$?
out_c=$($BOI dispatch "$FIXTURES/concurrent-c.yaml" 2>&1); rc_c=$?

assert_exit 0 $rc_a "concurrent dispatch A exits 0"
assert_exit 0 $rc_b "concurrent dispatch B exits 0"
assert_exit 0 $rc_c "concurrent dispatch C exits 0"

CONC_A=$(echo "$out_a" | grep -o 'q-[0-9]*')
CONC_B=$(echo "$out_b" | grep -o 'q-[0-9]*')
CONC_C=$(echo "$out_c" | grep -o 'q-[0-9]*')

if [ -n "$CONC_A" ] && [ -n "$CONC_B" ] && [ -n "$CONC_C" ]; then
  assert_pass "all 3 concurrent specs got IDs: $CONC_A, $CONC_B, $CONC_C"
else
  assert_fail "concurrent dispatch did not return 3 IDs"
fi

# Verify all 3 are queued
out=$($BOI status 2>&1)
assert_contains "$out" "Concurrent A" "status shows concurrent spec A"
assert_contains "$out" "Concurrent B" "status shows concurrent spec B"
assert_contains "$out" "Concurrent C" "status shows concurrent spec C"

# Start daemon in background, wait for specs to complete
$BOI daemon &
DAEMON_PID=$!
sleep 1

# Poll for completion of our 3 concurrent specs (max 60s)
WAITED=0
while [ $WAITED -lt 60 ]; do
  conc_done=0
  for cid in "$CONC_A" "$CONC_B" "$CONC_C"; do
    s=$($BOI status "$cid" 2>&1)
    echo "$s" | grep -q "completed" && conc_done=$((conc_done + 1))
  done
  if [ "$conc_done" -ge 3 ]; then
    break
  fi
  sleep 2
  WAITED=$((WAITED + 2))
done

# Stop daemon
kill $DAEMON_PID 2>/dev/null || true
wait $DAEMON_PID 2>/dev/null || true

# Assert all 3 completed (check each by ID)
for label_id in "A:$CONC_A" "B:$CONC_B" "C:$CONC_C"; do
  label="${label_id%%:*}"
  sid="${label_id#*:}"
  s=$($BOI status "$sid" 2>&1)
  if echo "$s" | grep -q "completed"; then
    assert_pass "concurrent spec $label ($sid) completed"
  else
    assert_fail "concurrent spec $label ($sid) did not complete — status: $(echo "$s" | head -3)"
  fi
done

# Assert marker files exist
if [ -f /tmp/boi-concurrent-a.txt ]; then
  assert_pass "marker file A exists"
else
  assert_fail "marker file A missing"
fi
if [ -f /tmp/boi-concurrent-b.txt ]; then
  assert_pass "marker file B exists"
else
  assert_fail "marker file B missing"
fi
if [ -f /tmp/boi-concurrent-c.txt ]; then
  assert_pass "marker file C exists"
else
  assert_fail "marker file C missing"
fi

# Assert no double-dispatch: each spec appears exactly once in the specs table
DB_CONC=$(find ~/.boi -name "boi-rust.db" -o -name "boi.db" -not -path "*/worktrees/*" 2>/dev/null | head -1)
if [ -n "$DB_CONC" ] && [ -n "$CONC_A" ]; then
  for label_id in "A:$CONC_A" "B:$CONC_B" "C:$CONC_C"; do
    label="${label_id%%:*}"
    sid="${label_id#*:}"
    count=$(sqlite3 "$DB_CONC" "SELECT COUNT(*) FROM specs WHERE id = '$sid';" 2>/dev/null)
    status=$(sqlite3 "$DB_CONC" "SELECT status FROM specs WHERE id = '$sid';" 2>/dev/null)
    if [ "$count" -eq 1 ] && [ "$status" = "completed" ]; then
      assert_pass "spec $label ($sid) has exactly 1 DB row, status=completed (no double-dispatch)"
    else
      assert_fail "spec $label ($sid) has $count rows, status=$status (expected 1 row, completed)"
    fi
  done
else
  assert_fail "could not verify DB state (DB or IDs missing)"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "18. Spec-to-spec dependency (--after)"
# ══════════════════════════════════════════════════════════════════════════════

# Clean state
rm -f /tmp/boi-first-done.txt /tmp/boi-second-done.txt
$BOI stop 2>/dev/null || true
rm -f ~/.boi/daemon.pid ~/.boi/daemon.heartbeat

# Dispatch first spec
out_first=$($BOI dispatch "$FIXTURES/dep-first.yaml" 2>&1); rc=$?
assert_exit 0 $rc "dispatch dep-first exits 0"
FIRST_ID=$(echo "$out_first" | grep -o 'q-[0-9]*')
if [ -n "$FIRST_ID" ]; then
  assert_pass "dep-first got ID: $FIRST_ID"
else
  assert_fail "dep-first did not return ID"
  FIRST_ID="q-999"
fi

# Dispatch second spec with --after dependency on first
out_second=$($BOI dispatch "$FIXTURES/dep-second.yaml" --after "$FIRST_ID" 2>&1); rc=$?
assert_exit 0 $rc "dispatch dep-second --after exits 0"
SECOND_ID=$(echo "$out_second" | grep -o 'q-[0-9]*')
if [ -n "$SECOND_ID" ]; then
  assert_pass "dep-second got ID: $SECOND_ID"
else
  assert_fail "dep-second did not return ID"
  SECOND_ID="q-998"
fi

# Verify dep-second has depends_on set in DB
DB_DEP=$(find ~/.boi -name "boi-rust.db" -o -name "boi.db" -not -path "*/worktrees/*" 2>/dev/null | head -1)
if [ -n "$DB_DEP" ]; then
  dep_val=$(sqlite3 "$DB_DEP" "SELECT depends_on FROM specs WHERE id = '$SECOND_ID';" 2>/dev/null)
  if [ "$dep_val" = "$FIRST_ID" ]; then
    assert_pass "dep-second depends_on is set to $FIRST_ID"
  else
    assert_fail "dep-second depends_on expected '$FIRST_ID', got '$dep_val'"
  fi
fi

# Start daemon, wait for both to complete
$BOI daemon &
DAEMON_PID=$!
sleep 1

WAITED=0
while [ $WAITED -lt 60 ]; do
  dep_done=0
  for did in "$FIRST_ID" "$SECOND_ID"; do
    s=$($BOI status "$did" 2>&1)
    echo "$s" | grep -q "completed" && dep_done=$((dep_done + 1))
  done
  if [ "$dep_done" -ge 2 ]; then
    break
  fi
  sleep 2
  WAITED=$((WAITED + 2))
done

# Stop daemon
kill $DAEMON_PID 2>/dev/null || true
wait $DAEMON_PID 2>/dev/null || true

# Assert both completed
for label_id in "first:$FIRST_ID" "second:$SECOND_ID"; do
  label="${label_id%%:*}"
  sid="${label_id#*:}"
  s=$($BOI status "$sid" 2>&1)
  if echo "$s" | grep -q "completed"; then
    assert_pass "dep-$label ($sid) completed"
  else
    assert_fail "dep-$label ($sid) did not complete — status: $(echo "$s" | head -3)"
  fi
done

# Assert ordering: first must have started before second
# We check started_at timestamps from the DB
if [ -n "$DB_DEP" ]; then
  first_started=$(sqlite3 "$DB_DEP" "SELECT started_at FROM specs WHERE id = '$FIRST_ID';" 2>/dev/null)
  second_started=$(sqlite3 "$DB_DEP" "SELECT started_at FROM specs WHERE id = '$SECOND_ID';" 2>/dev/null)
  if [ -n "$first_started" ] && [ -n "$second_started" ]; then
    if [[ "$first_started" < "$second_started" || "$first_started" == "$second_started" ]]; then
      assert_pass "dep-first started ($first_started) before dep-second ($second_started)"
    else
      assert_fail "dep-first started ($first_started) AFTER dep-second ($second_started) — ordering violated"
    fi
  else
    assert_fail "could not read started_at timestamps (first='$first_started', second='$second_started')"
  fi
fi

# Assert marker files prove ordering
if [ -f /tmp/boi-first-done.txt ]; then
  assert_pass "first-done marker exists"
else
  assert_fail "first-done marker missing"
fi
if [ -f /tmp/boi-second-done.txt ]; then
  assert_pass "second-done marker exists (proves first completed, enabling second's verify)"
else
  assert_fail "second-done marker missing (dep-second verify depends on first-done existing)"
fi

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
