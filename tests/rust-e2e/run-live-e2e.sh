#!/usr/bin/env bash
# run-live-e2e.sh — Live E2E tests using a real LLM model.
# These tests verify the full pipeline: dispatch → daemon → worker → real Claude → verify.
# Requires: claude CLI installed, ANTHROPIC_API_KEY set (or OpenRouter configured).
#
# Usage:
#   bash tests/rust-e2e/run-live-e2e.sh
#
# Uses --model haiku for cost efficiency. Override with LIVE_E2E_MODEL env var.
set -uo pipefail

PASS=0
FAIL=0
BOI="${BOI_BIN:-$HOME/.boi/bin/boi}"
MODEL="${LIVE_E2E_MODEL:-claude-haiku-4-5-20251001}"
REPO_DIR="$(cd "$(dirname "$0")/../.." && pwd)"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n' "$*"; }

assert_pass() { PASS=$((PASS + 1)); green "  ✓ $*"; }
assert_fail() { FAIL=$((FAIL + 1)); red "  ✗ $*"; }

cleanup_daemon() {
  pkill -f "boi daemon" 2>/dev/null
  sleep 1
  rm -f ~/.boi/daemon.pid
}

cleanup_db() {
  rm -f ~/.boi/boi-rust.db
  cd "$REPO_DIR" && git worktree prune 2>/dev/null
}

wait_for_spec() {
  local spec_id="$1" timeout="${2:-120}"
  local elapsed=0
  until $BOI status 2>/dev/null | grep -qE "✓.*$spec_id|✗.*$spec_id"; do
    sleep 5
    elapsed=$((elapsed + 5))
    if [ "$elapsed" -ge "$timeout" ]; then
      echo "TIMEOUT waiting for $spec_id"
      return 1
    fi
  done
  return 0
}

echo ""
bold "═══════════════════════════════════════════"
bold "  BOI Live E2E Tests (model: $MODEL)"
bold "═══════════════════════════════════════════"
echo ""

# Verify prerequisites
if ! command -v claude &>/dev/null; then
  red "ERROR: claude CLI not found. Install with: npm install -g @anthropic-ai/claude-code"
  exit 1
fi

# Clean state
cleanup_daemon
cleanup_db

# Start daemon
BOI_LOG_LEVEL=info nohup $BOI daemon --foreground > /tmp/boi-live-e2e.log 2>&1 &
DAEMON_PID=$!
sleep 2

if ! kill -0 $DAEMON_PID 2>/dev/null; then
  red "ERROR: daemon failed to start"
  cat /tmp/boi-live-e2e.log
  exit 1
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "1. Single task — file creation"
# ══════════════════════════════════════════════════════════════════════════════

rm -f /tmp/boi-live-t1.txt
cat > /tmp/live-single.yaml <<YAML
title: "Live E2E — Single Task"
mode: execute
workspace: $REPO_DIR
tasks:
  - id: t-1
    title: "Create marker file"
    spec: |
      Write the text "live-e2e-pass" to /tmp/boi-live-t1.txt
    verify: "grep -q 'live-e2e-pass' /tmp/boi-live-t1.txt"
YAML

SPEC_ID=$($BOI dispatch /tmp/live-single.yaml 2>&1)
echo "  dispatched: $SPEC_ID"

if wait_for_spec "$SPEC_ID" 120; then
  if $BOI status 2>/dev/null | grep -q "✓.*$SPEC_ID"; then
    assert_pass "single task spec completed"
    if grep -q "live-e2e-pass" /tmp/boi-live-t1.txt 2>/dev/null; then
      assert_pass "marker file created with correct content"
    else
      assert_fail "marker file missing or wrong content"
    fi
  else
    assert_fail "single task spec failed"
    $BOI log "$SPEC_ID" --debug 2>&1 | tail -10
  fi
else
  assert_fail "single task spec timed out"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "2. Two-task chain with dependency"
# ══════════════════════════════════════════════════════════════════════════════

rm -f /tmp/boi-live-chain-1.txt /tmp/boi-live-chain-2.txt
cat > /tmp/live-chain.yaml <<YAML
title: "Live E2E — Task Chain"
mode: execute
workspace: $REPO_DIR
tasks:
  - id: t-1
    title: "Write first file"
    spec: |
      Write "chain-step-1" to /tmp/boi-live-chain-1.txt
    verify: "grep -q 'chain-step-1' /tmp/boi-live-chain-1.txt"
  - id: t-2
    title: "Write second file"
    depends: [t-1]
    spec: |
      Write "chain-step-2" to /tmp/boi-live-chain-2.txt
    verify: "grep -q 'chain-step-2' /tmp/boi-live-chain-2.txt"
YAML

CHAIN_ID=$($BOI dispatch /tmp/live-chain.yaml 2>&1)
echo "  dispatched: $CHAIN_ID"

if wait_for_spec "$CHAIN_ID" 180; then
  if $BOI status 2>/dev/null | grep -q "✓.*$CHAIN_ID"; then
    assert_pass "two-task chain completed"
    if grep -q "chain-step-1" /tmp/boi-live-chain-1.txt 2>/dev/null; then
      assert_pass "chain task 1 marker correct"
    else
      assert_fail "chain task 1 marker missing"
    fi
    if grep -q "chain-step-2" /tmp/boi-live-chain-2.txt 2>/dev/null; then
      assert_pass "chain task 2 marker correct"
    else
      assert_fail "chain task 2 marker missing"
    fi
  else
    assert_fail "two-task chain failed"
    $BOI log "$CHAIN_ID" --debug 2>&1 | tail -10
  fi
else
  assert_fail "two-task chain timed out"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "3. Code modification — edit a real file"
# ══════════════════════════════════════════════════════════════════════════════

cat > /tmp/live-code-edit.yaml <<YAML
title: "Live E2E — Code Edit"
mode: execute
workspace: $REPO_DIR
tasks:
  - id: t-1
    title: "Add a comment to README"
    spec: |
      Add the line "# LIVE-E2E-MARKER" to the end of README.md
    verify: "grep -q 'LIVE-E2E-MARKER' README.md"
YAML

EDIT_ID=$($BOI dispatch /tmp/live-code-edit.yaml 2>&1)
echo "  dispatched: $EDIT_ID"

if wait_for_spec "$EDIT_ID" 120; then
  if $BOI status 2>/dev/null | grep -q "✓.*$EDIT_ID"; then
    assert_pass "code edit spec completed"
  else
    assert_fail "code edit spec failed"
    $BOI log "$EDIT_ID" --debug 2>&1 | tail -10
  fi
else
  assert_fail "code edit spec timed out"
fi

# Clean up the edit so we don't pollute the repo
cd "$REPO_DIR" && git checkout -- README.md 2>/dev/null

echo ""

# ══════════════════════════════════════════════════════════════════════════════
bold "4. Failure preserves worktree"
# ══════════════════════════════════════════════════════════════════════════════

cat > /tmp/live-fail.yaml <<YAML
title: "Live E2E — Expected Failure"
mode: execute
workspace: $REPO_DIR
tasks:
  - id: t-1
    title: "Task that will fail verify"
    spec: |
      Write "hello" to /tmp/boi-live-fail.txt
    verify: "grep -q 'impossible-string-never-matches' /tmp/boi-live-fail.txt"
YAML

FAIL_ID=$($BOI dispatch /tmp/live-fail.yaml 2>&1)
echo "  dispatched: $FAIL_ID"

if wait_for_spec "$FAIL_ID" 120; then
  if $BOI status 2>/dev/null | grep -q "✗.*$FAIL_ID"; then
    assert_pass "expected failure detected"
    if [ -d "$HOME/.boi/worktrees/$FAIL_ID" ]; then
      assert_pass "worktree preserved after failure"
    else
      assert_fail "worktree cleaned up on failure — should be preserved"
    fi
  else
    assert_fail "spec should have failed but didn't"
  fi
else
  assert_fail "failure test timed out"
fi

echo ""

# ══════════════════════════════════════════════════════════════════════════════
# Cleanup
# ══════════════════════════════════════════════════════════════════════════════

cleanup_daemon
rm -f /tmp/boi-live-*.txt /tmp/live-*.yaml

echo ""
bold "═══════════════════════════════════════════"
total=$((PASS + FAIL))
if [ $FAIL -eq 0 ]; then
  green "  ALL $total LIVE TESTS PASSED"
else
  red "  $FAIL/$total LIVE TESTS FAILED"
fi
bold "═══════════════════════════════════════════"
echo ""

exit $FAIL
