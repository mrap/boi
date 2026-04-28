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
bold "19. In-flight mutation — add task to queued spec"
# ══════════════════════════════════════════════════════════════════════════════

# Clean state
rm -f /tmp/boi-mut-add-1.txt /tmp/boi-mut-add-2.txt /tmp/boi-mut-add-3.txt
$BOI stop 2>/dev/null || true
rm -f ~/.boi/daemon.pid ~/.boi/daemon.heartbeat

# Dispatch a 2-task spec
out=$($BOI dispatch "$FIXTURES/mutation-add.yaml" 2>&1); rc=$?
assert_exit 0 $rc "dispatch mutation-add spec exits 0"
ADD_ID=$(echo "$out" | grep -o 'q-[0-9]*')
if [ -n "$ADD_ID" ]; then
  assert_pass "mutation-add got ID: $ADD_ID"
else
  assert_fail "mutation-add did not return ID"
  ADD_ID="q-999"
fi

# Before starting daemon, add a 3rd task
out=$($BOI spec "$ADD_ID" add "Write marker 3" --spec "Create /tmp/boi-mut-add-3.txt" --verify "touch /tmp/boi-mut-add-3.txt" 2>&1); rc=$?
assert_exit 0 $rc "spec add task exits 0"
assert_contains "$out" "added" "spec add confirms addition"

# Verify spec shows 3 tasks
out=$($BOI spec "$ADD_ID" 2>&1); rc=$?
assert_exit 0 $rc "spec view after add exits 0"
task_count=$(echo "$out" | grep -c "t-[0-9]")
if [ "$task_count" -ge 3 ]; then
  assert_pass "spec shows 3 tasks after add (found $task_count)"
else
  assert_fail "spec should show 3 tasks after add (found $task_count)"
fi

# Verify total_tasks updated in DB
DB_MUT=$(find ~/.boi -name "boi-rust.db" -o -name "boi.db" -not -path "*/worktrees/*" 2>/dev/null | head -1)
if [ -n "$DB_MUT" ]; then
  total=$(sqlite3 "$DB_MUT" "SELECT total_tasks FROM specs WHERE id = '$ADD_ID';" 2>/dev/null)
  if [ "$total" -eq 3 ]; then
    assert_pass "DB total_tasks = 3 after add"
  else
    assert_fail "DB total_tasks expected 3, got $total"
  fi
fi

# Start daemon, let it complete
$BOI daemon &
DAEMON_PID=$!
sleep 1

WAITED=0
while [ $WAITED -lt 60 ]; do
  s=$($BOI status "$ADD_ID" 2>&1)
  echo "$s" | grep -q "completed" && break
  sleep 2
  WAITED=$((WAITED + 2))
done

kill $DAEMON_PID 2>/dev/null || true
wait $DAEMON_PID 2>/dev/null || true

# Assert spec completed
s=$($BOI status "$ADD_ID" 2>&1)
if echo "$s" | grep -q "completed"; then
  assert_pass "mutation-add spec completed"
else
  assert_fail "mutation-add spec did not complete — status: $(echo "$s" | head -3)"
fi

# Assert original marker files exist (tasks from YAML)
if [ -f /tmp/boi-mut-add-1.txt ]; then
  assert_pass "add test: marker 1 exists"
else
  assert_fail "add test: marker 1 missing"
fi
if [ -f /tmp/boi-mut-add-2.txt ]; then
  assert_pass "add test: marker 2 exists"
else
  assert_fail "add test: marker 2 missing"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "20. In-flight mutation — skip task in queued spec"
# ══════════════════════════════════════════════════════════════════════════════

# Clean state
rm -f /tmp/boi-mut-skip-1.txt /tmp/boi-mut-skip-2.txt /tmp/boi-mut-skip-3.txt
$BOI stop 2>/dev/null || true
rm -f ~/.boi/daemon.pid ~/.boi/daemon.heartbeat

# Dispatch a 3-task chain (t-1 → t-2 → t-3)
out=$($BOI dispatch "$FIXTURES/mutation-skip.yaml" 2>&1); rc=$?
assert_exit 0 $rc "dispatch mutation-skip spec exits 0"
SKIP_ID=$(echo "$out" | grep -o 'q-[0-9]*')
if [ -n "$SKIP_ID" ]; then
  assert_pass "mutation-skip got ID: $SKIP_ID"
else
  assert_fail "mutation-skip did not return ID"
  SKIP_ID="q-999"
fi

# Skip t-2 before daemon starts
out=$($BOI spec "$SKIP_ID" skip t-2 2>&1); rc=$?
assert_exit 0 $rc "spec skip t-2 exits 0"
assert_contains "$out" "skipped" "spec skip confirms skip"

# Verify t-2 shows SKIPPED in spec view
out=$($BOI spec "$SKIP_ID" 2>&1); rc=$?
assert_exit 0 $rc "spec view after skip exits 0"
if echo "$out" | grep -i "t-2" | grep -iq "skip"; then
  assert_pass "spec view shows t-2 as SKIPPED"
else
  assert_fail "spec view does not show t-2 as SKIPPED"
fi

# Verify DB state: t-2 is SKIPPED, completed_tasks incremented
DB_SKIP=$(find ~/.boi -name "boi-rust.db" -o -name "boi.db" -not -path "*/worktrees/*" 2>/dev/null | head -1)
if [ -n "$DB_SKIP" ]; then
  t2_status=$(sqlite3 "$DB_SKIP" "SELECT status FROM tasks WHERE spec_id = '$SKIP_ID' AND id = 't-2';" 2>/dev/null)
  if [ "$t2_status" = "SKIPPED" ]; then
    assert_pass "DB shows t-2 status = SKIPPED"
  else
    assert_fail "DB t-2 status expected SKIPPED, got '$t2_status'"
  fi

  completed=$(sqlite3 "$DB_SKIP" "SELECT completed_tasks FROM specs WHERE id = '$SKIP_ID';" 2>/dev/null)
  if [ "$completed" -ge 1 ]; then
    assert_pass "DB completed_tasks >= 1 after skip (got $completed)"
  else
    assert_fail "DB completed_tasks expected >= 1, got $completed"
  fi
fi

# Start daemon, let it complete
$BOI daemon &
DAEMON_PID=$!
sleep 1

WAITED=0
while [ $WAITED -lt 60 ]; do
  s=$($BOI status "$SKIP_ID" 2>&1)
  echo "$s" | grep -q "completed" && break
  sleep 2
  WAITED=$((WAITED + 2))
done

kill $DAEMON_PID 2>/dev/null || true
wait $DAEMON_PID 2>/dev/null || true

# Assert spec completed
s=$($BOI status "$SKIP_ID" 2>&1)
if echo "$s" | grep -q "completed"; then
  assert_pass "mutation-skip spec completed"
else
  assert_fail "mutation-skip spec did not complete — status: $(echo "$s" | head -3)"
fi

# Assert t-1 ran (marker exists)
if [ -f /tmp/boi-mut-skip-1.txt ]; then
  assert_pass "skip test: marker 1 exists (t-1 ran)"
else
  assert_fail "skip test: marker 1 missing (t-1 did not run)"
fi

# Assert t-2 did NOT run (marker should not exist — it was skipped)
if [ ! -f /tmp/boi-mut-skip-2.txt ]; then
  assert_pass "skip test: marker 2 absent (t-2 correctly skipped)"
else
  assert_fail "skip test: marker 2 exists (t-2 ran despite skip)"
fi

# Verify final DB state: t-1 DONE, t-2 SKIPPED
if [ -n "$DB_SKIP" ]; then
  t1_status=$(sqlite3 "$DB_SKIP" "SELECT status FROM tasks WHERE spec_id = '$SKIP_ID' AND id = 't-1';" 2>/dev/null)
  t2_final=$(sqlite3 "$DB_SKIP" "SELECT status FROM tasks WHERE spec_id = '$SKIP_ID' AND id = 't-2';" 2>/dev/null)
  if [ "$t1_status" = "DONE" ]; then
    assert_pass "skip test DB: t-1 = DONE"
  else
    assert_fail "skip test DB: t-1 expected DONE, got '$t1_status'"
  fi
  if [ "$t2_final" = "SKIPPED" ]; then
    assert_pass "skip test DB: t-2 = SKIPPED (preserved)"
  else
    assert_fail "skip test DB: t-2 expected SKIPPED, got '$t2_final'"
  fi
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "21. In-flight mutation — block task on dependency"
# ══════════════════════════════════════════════════════════════════════════════

# Clean state
rm -f /tmp/boi-mut-block-1.txt /tmp/boi-mut-block-2.txt
$BOI stop 2>/dev/null || true
rm -f ~/.boi/daemon.pid ~/.boi/daemon.heartbeat

# Dispatch a 2-task spec with NO deps between them
out=$($BOI dispatch "$FIXTURES/mutation-block.yaml" 2>&1); rc=$?
assert_exit 0 $rc "dispatch mutation-block spec exits 0"
BLOCK_ID=$(echo "$out" | grep -o 'q-[0-9]*')
if [ -n "$BLOCK_ID" ]; then
  assert_pass "mutation-block got ID: $BLOCK_ID"
else
  assert_fail "mutation-block did not return ID"
  BLOCK_ID="q-999"
fi

# Block t-2 on t-1
out=$($BOI spec "$BLOCK_ID" block t-2 --on t-1 2>&1); rc=$?
assert_exit 0 $rc "spec block t-2 --on t-1 exits 0"
assert_contains "$out" "blocked" "spec block confirms blocking"

# Verify DB shows the dependency
DB_BLOCK=$(find ~/.boi -name "boi-rust.db" -o -name "boi.db" -not -path "*/worktrees/*" 2>/dev/null | head -1)
if [ -n "$DB_BLOCK" ]; then
  deps=$(sqlite3 "$DB_BLOCK" "SELECT depends FROM tasks WHERE spec_id = '$BLOCK_ID' AND id = 't-2';" 2>/dev/null)
  if echo "$deps" | grep -q "t-1"; then
    assert_pass "DB shows t-2 depends on t-1 after block"
  else
    assert_fail "DB t-2 depends expected to contain t-1, got '$deps'"
  fi
fi

# Start daemon, let it complete
$BOI daemon &
DAEMON_PID=$!
sleep 1

WAITED=0
while [ $WAITED -lt 60 ]; do
  s=$($BOI status "$BLOCK_ID" 2>&1)
  echo "$s" | grep -q "completed" && break
  sleep 2
  WAITED=$((WAITED + 2))
done

kill $DAEMON_PID 2>/dev/null || true
wait $DAEMON_PID 2>/dev/null || true

# Assert spec completed
s=$($BOI status "$BLOCK_ID" 2>&1)
if echo "$s" | grep -q "completed"; then
  assert_pass "mutation-block spec completed"
else
  assert_fail "mutation-block spec did not complete — status: $(echo "$s" | head -3)"
fi

# Assert both markers exist (both tasks ran)
if [ -f /tmp/boi-mut-block-1.txt ]; then
  assert_pass "block test: marker 1 exists (t-1 ran)"
else
  assert_fail "block test: marker 1 missing"
fi
if [ -f /tmp/boi-mut-block-2.txt ]; then
  assert_pass "block test: marker 2 exists (t-2 ran)"
else
  assert_fail "block test: marker 2 missing"
fi

# Verify ordering via DB timestamps: t-1 started before t-2
if [ -n "$DB_BLOCK" ]; then
  t1_start=$(sqlite3 "$DB_BLOCK" "SELECT started_at FROM tasks WHERE spec_id = '$BLOCK_ID' AND id = 't-1';" 2>/dev/null)
  t2_start=$(sqlite3 "$DB_BLOCK" "SELECT started_at FROM tasks WHERE spec_id = '$BLOCK_ID' AND id = 't-2';" 2>/dev/null)
  if [ -n "$t1_start" ] && [ -n "$t2_start" ]; then
    if [[ "$t1_start" < "$t2_start" || "$t1_start" == "$t2_start" ]]; then
      assert_pass "block test: t-1 started ($t1_start) <= t-2 started ($t2_start)"
    else
      assert_fail "block test: t-1 started ($t1_start) AFTER t-2 ($t2_start) — block violated"
    fi
  else
    assert_fail "block test: could not read timestamps (t1='$t1_start', t2='$t2_start')"
  fi
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "22. Phase loading from TOML"
# ══════════════════════════════════════════════════════════════════════════════

# Verify phases are loaded from ~/.boi/phases/
out=$($BOI phases 2>&1); rc=$?
assert_exit 0 $rc "phases command exits 0"
assert_contains "$out" "execute" "phases shows execute"
assert_contains "$out" "critic" "phases shows critic"
assert_contains "$out" "task-verify" "phases shows task-verify"
# Count: should have at least 5 core phases
count=$(echo "$out" | grep -c "core\|override")
if [ "$count" -ge 5 ]; then
  assert_pass "at least 5 phases loaded from TOML ($count found)"
else
  assert_fail "expected 5+ phases, got $count"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "23. Pipeline config"
# ══════════════════════════════════════════════════════════════════════════════

# Verify pipelines.toml exists and is loaded
# Dispatch with different modes and check they work
out=$($BOI dispatch "$FIXTURES/valid-simple.yaml" --mode discover --dry-run 2>&1); rc=$?
assert_exit 0 $rc "dispatch with --mode discover (dry-run) exits 0"
out=$($BOI dispatch "$FIXTURES/valid-simple.yaml" --mode generate --dry-run 2>&1); rc=$?
assert_exit 0 $rc "dispatch with --mode generate (dry-run) exits 0"

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "24. Cleanup on success"
# ══════════════════════════════════════════════════════════════════════════════

# Clean state
$BOI stop 2>/dev/null || true
rm -f ~/.boi/daemon.pid ~/.boi/daemon.heartbeat

# Create a spec with workspace set so worktree is created
cat > /tmp/cleanup-test.yaml <<YAML
title: "Cleanup Test"
mode: execute
workspace: $HOME/test-repo
tasks:
  - id: t-1
    title: "Simple task"
    status: PENDING
    spec: "echo done"
    verify: "true"
YAML
CLEANUP_ID=$($BOI dispatch /tmp/cleanup-test.yaml 2>&1 | grep -o 'q-[0-9]*')
if [ -n "$CLEANUP_ID" ]; then
  assert_pass "cleanup test dispatched: $CLEANUP_ID"
else
  assert_fail "cleanup test dispatch failed"
  CLEANUP_ID="q-999"
fi

# Start daemon in background
$BOI daemon &
DAEMON_PID=$!
sleep 1

# Wait for completion (mock claude exits 0, verify is "true")
WAITED=0
while [ $WAITED -lt 60 ]; do
  s=$($BOI status "$CLEANUP_ID" 2>&1)
  echo "$s" | grep -q "completed" && break
  sleep 2
  WAITED=$((WAITED + 2))
done

kill $DAEMON_PID 2>/dev/null || true
wait $DAEMON_PID 2>/dev/null || true

# Check spec completed
s=$($BOI status "$CLEANUP_ID" 2>&1)
if echo "$s" | grep -q "completed"; then
  assert_pass "cleanup test spec completed"
else
  assert_fail "cleanup test spec did not complete — status: $(echo "$s" | head -3)"
fi

# Check worktree was cleaned up
if [ ! -d "$HOME/.boi/worktrees/$CLEANUP_ID" ]; then
  assert_pass "worktree cleaned up after success"
else
  assert_fail "worktree NOT cleaned up after success"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "25. Cleanup skipped on failure"
# ══════════════════════════════════════════════════════════════════════════════

# Clean state
$BOI stop 2>/dev/null || true
rm -f ~/.boi/daemon.pid ~/.boi/daemon.heartbeat

# Use fallback phases (no TOML) so task-verify runs the verify command directly
# instead of spawning mock claude (which always approves).
SAVED_PHASES_DIR="${BOI_PHASES_DIR:-}"
unset BOI_PHASES_DIR

# Create a spec that will fail verify
cat > /tmp/fail-test.yaml <<YAML
title: "Fail Test"
mode: execute
workspace: $HOME/test-repo
tasks:
  - id: t-1
    title: "Will fail verify"
    status: PENDING
    spec: "echo hello"
    verify: "false"
YAML
FAIL_ID=$($BOI dispatch /tmp/fail-test.yaml 2>&1 | grep -o 'q-[0-9]*')
if [ -n "$FAIL_ID" ]; then
  assert_pass "fail test dispatched: $FAIL_ID"
else
  assert_fail "fail test dispatch failed"
  FAIL_ID="q-999"
fi

$BOI daemon &
DAEMON_PID=$!
sleep 1

# Wait for spec to reach terminal state (failed or completed).
# With fallback phases (task-verify requires_claude=false), the verify "false"
# command runs directly and fails, triggering requeue→retry→fail cycle.
WAITED=0
while [ $WAITED -lt 60 ]; do
  s=$($BOI status "$FAIL_ID" 2>&1)
  echo "$s" | grep -q "failed\|completed" && break
  sleep 2
  WAITED=$((WAITED + 2))
done

kill $DAEMON_PID 2>/dev/null || true
wait $DAEMON_PID 2>/dev/null || true

# Restore BOI_PHASES_DIR
if [ -n "$SAVED_PHASES_DIR" ]; then
  export BOI_PHASES_DIR="$SAVED_PHASES_DIR"
fi

# The spec should have failed
s=$($BOI status "$FAIL_ID" 2>&1)
if echo "$s" | grep -q "failed"; then
  assert_pass "fail test spec reached failed state"
else
  assert_fail "fail test spec did not fail — status: $(echo "$s" | head -3)"
fi

# Check worktree was preserved (cleanup_on_failure defaults to false)
if [ -d "$HOME/.boi/worktrees/$FAIL_ID" ]; then
  assert_pass "worktree preserved after failure (cleanup_on_failure=false)"
else
  assert_fail "worktree was cleaned up on failure — should be preserved"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "26. User phase override"
# ══════════════════════════════════════════════════════════════════════════════

# Create a user phase that overrides execute
mkdir -p $HOME/.boi/phases
cat > $HOME/.boi/phases/execute.phase.toml <<TOML
name = "execute"
description = "Custom execute override"
[worker]
runtime = "claude"
timeout = 300
TOML
out=$($BOI phases 2>&1)
assert_contains "$out" "override" "execute shows as override after user phase"
# Clean up
rm -f $HOME/.boi/phases/execute.phase.toml

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "27. Phases detail view"
# ══════════════════════════════════════════════════════════════════════════════

out=$($BOI phases execute 2>&1); rc=$?
assert_exit 0 $rc "phases execute exits 0"
assert_contains "$out" "execute" "phases detail displays phase name"
# Nonexistent phase
out=$($BOI phases nonexistent 2>&1); rc=$?
if [ "$rc" -ne 0 ]; then
  assert_pass "phases nonexistent exits non-zero"
else
  assert_fail "phases nonexistent should exit non-zero"
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
