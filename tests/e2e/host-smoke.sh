#!/usr/bin/env bash
# BOI v2 — host-level smoke test. Dispatches 01-typo-fix through real Goose +
# OpenRouter and drives it to a terminal state. Isolated from v1: everything
# lives under ~/.boi/v2/; the v2 `boi` is symlinked into a scratch dir that is
# on THIS script's PATH only — never your shell's — so zero collision with v1.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BOI_ROOT="$HOME/.boi/v2"
BOI_BIN="$REPO/target/debug/boi"
MODEL="openai/gpt-4.1"
WS="/tmp/boi-v2-host-smoke-ws"
SPEC="/tmp/boi-v2-host-smoke-spec.toml"
DAEMON_LOG="/tmp/boi-v2-host-daemon.log"
BIN_DIR="/tmp/boi-v2-host-smoke-bin"   # holds a `boi` symlink Goose can resolve
ENV_FILE="${BOI_OPENROUTER_ENV:-$HOME/.boi/secrets/openrouter.env}"   # OpenRouter creds
DAEMON_PID=""

say() { printf '\n=== %s ===\n' "$*"; }
die() {
  printf '\nHOST-SMOKE FAIL: %s\n' "$*" >&2
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  exit 1
}

# --- the v2 `boi` must be resolvable as a bare name -------------------------
# The recipe BOI writes declares an MCP extension `cmd: boi`; Goose spawns it
# by bare name. Symlink the v2 binary into a scratch dir and put THAT first on
# PATH — so the daemon, Goose, and `boi mcp-serve` all resolve v2, with no
# collision with v1 (this dir is on the smoke run's PATH only).
mkdir -p "$BIN_DIR"
ln -sf "$BOI_BIN" "$BIN_DIR/boi"
export PATH="$BIN_DIR:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$PATH"

# --- the OpenRouter credential ----------------------------------------------
if [ -f "$ENV_FILE" ]; then
  set -a; . "$ENV_FILE"; set +a
fi

say "preflight"
[ -x "$BOI_BIN" ]                || die "no boi binary at $BOI_BIN — rebuild it"
command -v goose >/dev/null      || die "goose not on PATH"
command -v boi >/dev/null        || die "the boi symlink did not resolve"
[ -n "${OPENROUTER_API_KEY:-}" ] || die "OPENROUTER_API_KEY unset — run: echo 'OPENROUTER_API_KEY=sk-or-...' > $ENV_FILE"
echo "boi=$(command -v boi)  goose=$(goose --version 2>&1 | tr -d ' ')  model=as-configured  key=set(${#OPENROUTER_API_KEY}c)"

# --- 0. clean ~/.boi/v2 (move any leftover state aside) ---------------------
say "provisioning $BOI_ROOT"
[ -e "$BOI_ROOT" ] && mv "$BOI_ROOT" "${BOI_ROOT}.bak.$(date +%s)"
mkdir -p "$BOI_ROOT/phases" "$BOI_ROOT/pipelines"

# --- 1. pipeline: drop the critique_plan provider override ------------------
sed '/\[overrides\.critique_plan\.runtime\]/,$d' \
  "$REPO/tests/fixtures/pipelines/standard.toml" \
  > "$BOI_ROOT/pipelines/standard.toml"

# --- 2. phases: copy verbatim — workers run on their configured provider ----
# The claude_code -> claude-code Goose provider-name translation lives in
# recipe.rs (goose_provider_name); no phase rewrite needed.
cp "$REPO"/tests/fixtures/phases/*.toml "$BOI_ROOT/phases/"

# --- 3. worker prompt templates (G26.1) -------------------------------------
# Real templates ship in tests/fixtures/phases/<name>.md — copy them verbatim.
# A worker phase still without a real template gets a stub (the 01-typo-fix
# happy path never reaches the adjustment / plan_revision phases).
cp "$REPO"/tests/fixtures/phases/*.md "$BOI_ROOT/phases/" 2>/dev/null || true
for tmpl in propose_adjustment review_adjustment plan_revision; do
  [ -f "$BOI_ROOT/phases/$tmpl.md" ] || cat > "$BOI_ROOT/phases/$tmpl.md" <<'PROMPT'
You are a BOI worker. Read the <phase_context> above. Do the smallest correct
thing the task asks for, then emit your WorkerVerdict JSON as your final message.
PROMPT
done
echo "provisioned $(ls "$BOI_ROOT"/phases/*.toml 2>/dev/null | wc -l | tr -d ' ') phase TOMLs + $(ls "$BOI_ROOT"/phases/*.md 2>/dev/null | wc -l | tr -d ' ') prompts"

# --- 4. fresh workspace repo containing the typo ----------------------------
say "creating workspace repo $WS"
rm -rf "$WS"; mkdir -p "$WS"
( cd "$WS" \
  && git init -q -b develop \
  && git config user.email smoke@boi.local \
  && git config user.name "BOI host smoke" \
  && printf 'Please recieve this README.\n' > README.md \
  && git add README.md && git commit -q -m "initial commit" ) \
  || die "workspace git init failed"

# --- 5. spec: point its workspace at the fresh repo -------------------------
sed "s#^workspace = .*#workspace = \"$WS\"#" \
  "$REPO/tests/fixtures/specs/01-typo-fix.toml" > "$SPEC"

# --- 6. start the v2 daemon (inherits PATH incl. the boi symlink + the key) -
say "starting boi v2 daemon"
"$BOI_BIN" daemon > "$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 30); do
  [ -S "$BOI_ROOT/daemon.sock" ] && break
  kill -0 "$DAEMON_PID" 2>/dev/null \
    || { cat "$DAEMON_LOG" >&2; die "daemon exited before binding the socket"; }
  sleep 1
done
[ -S "$BOI_ROOT/daemon.sock" ] || die "daemon never bound $BOI_ROOT/daemon.sock"
echo "daemon up (pid $DAEMON_PID)"

# --- 7. dispatch ------------------------------------------------------------
say "dispatching 01-typo-fix"
DOUT="$("$BOI_BIN" dispatch "$SPEC" 2>&1)" \
  || { printf '%s\n' "$DOUT" >&2; cat "$DAEMON_LOG" >&2; die "boi dispatch failed"; }
printf '%s\n' "$DOUT"
SPEC_ID="$(printf '%s' "$DOUT" | grep -Eo 'S[0-9a-z]{8,}' | head -1)"
[ -n "$SPEC_ID" ] || die "no spec id parsed from dispatch output"
echo "dispatched: $SPEC_ID"

# --- 8. wait for a terminal state -------------------------------------------
say "waiting for $SPEC_ID to settle (<= 15 min)"
DEADLINE=$(( $(date +%s) + 900 ))
STATUS=TIMEOUT
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  DASH="$("$BOI_BIN" dashboard "$SPEC_ID" 2>/dev/null || true)"
  if printf '%s' "$DASH" | grep -Eq "^$SPEC_ID \[done\]"; then STATUS=done; break; fi
  sleep 5
done
echo "settled: $STATUS"

# --- 9. report --------------------------------------------------------------
say "spec status (DB)"
sqlite3 -line "$BOI_ROOT/boi.db" \
  "SELECT status, failure_reason FROM spec_runtime WHERE spec_id='$SPEC_ID';" 2>&1
say "boi dashboard $SPEC_ID"
"$BOI_BIN" dashboard "$SPEC_ID" 2>&1 | head -40 || true
say "boi log $SPEC_ID"
"$BOI_BIN" log "$SPEC_ID" 2>&1 | head -40 || true
say "workspace result — did the typo get fixed?"
echo "README.md now reads:"; cat "$WS/README.md"
echo "--- git branches + log (all) ---"
( cd "$WS" && git --no-pager branch -a 2>&1 && echo --- && git --no-pager log --all --oneline 2>&1 | head -20 )

# --- 10. assertions — the work happened, not just "settled" -----------------
say "assertions"
FAILED=0
STATUS_DB="$(sqlite3 "$BOI_ROOT/boi.db" \
  "SELECT status FROM spec_runtime WHERE spec_id='$SPEC_ID';" 2>/dev/null)"
INTEG_README="$(cd "$WS" && git show "spec/$SPEC_ID/integration:README.md" 2>/dev/null \
  || echo '<missing>')"

# A1 — the WORK: the typo is fixed on the integration branch.
if printf '%s' "$INTEG_README" | grep -q 'receive' \
   && ! printf '%s' "$INTEG_README" | grep -q 'recieve'; then
  echo "A1 PASS — typo fixed on the integration branch"
else
  echo "A1 FAIL — integration README still wrong: [$INTEG_README]"; FAILED=1
fi

# A2 — no phase verdict is a failure.
PHASE_FAILS="$(sqlite3 "$BOI_ROOT/boi.db" \
  "SELECT count(*) FROM phase_runs WHERE spec_id='$SPEC_ID' \
   AND json_extract(verdict,'\$.outcome.type')='fail';" 2>/dev/null)"
if [ "$PHASE_FAILS" = "0" ]; then
  echo "A2 PASS — no phase failed"
else
  echo "A2 FAIL — $PHASE_FAILS phase(s) emitted a fail verdict"; FAILED=1
fi

# A3 — the spec is honestly completed.
if [ "$STATUS_DB" = "completed" ]; then
  echo "A3 PASS — spec status = completed"
else
  echo "A3 FAIL — spec status = $STATUS_DB"; FAILED=1
fi

# --- 11. stop the daemon ----------------------------------------------------
kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
if [ "$FAILED" = "0" ]; then
  echo "HOST E2E: PASS — $SPEC_ID did the work"; exit 0
else
  echo "HOST E2E: FAIL — $SPEC_ID ($STATUS)"; exit 1
fi
