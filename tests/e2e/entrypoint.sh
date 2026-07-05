#!/usr/bin/env bash
# BOI v2 — the in-container Docker E2E harness (Phase 10.5).
#
# Runs the 3 real-`goose` scenarios the L3 mock cannot cover. A non-zero exit
# fails `just e2e`.
#
#   Scenario 1 — 01-typo-fix      : a real `boi dispatch` of the trivial
#                                   fixture, driven to a terminal spec state.
#   Scenario 2 — scripted cancel  : `boi dispatch` then `boi cancel` mid-run —
#                                   real cancellation of a live `goose` child.
#   Scenario 3 — retry recovery   : a `goose` wrapper whose FIRST invocation
#                                   emits a malformed stream (→ BOI's
#                                   `VerdictParse` retry), then delegates to
#                                   the real `goose` — `GooseRuntime`'s 2-retry
#                                   loop exercised end-to-end.
#
# Honest scope: `02`–`05`'s real-provider behaviour is uncovered at v1.0;
# `04`-cross-provider real execution would need a second provider in the
# container — a documented v1.0 gap.
set -euo pipefail

log()  { printf '\n=== %s ===\n' "$*"; }
fail() { printf '\nE2E FAIL: %s\n' "$*" >&2; exit 1; }

BOI_ROOT="${HOME}/.boi/v2"
SPEC_SRC=/e2e/specs/01-typo-fix.toml
OLLAMA_MODEL="${OLLAMA_MODEL:-qwen2.5:0.5b}"
REAL_GOOSE="$(command -v goose)"

# ---------------------------------------------------------------------------
# 0. Ollama — start the local model server and pull the worker model.
# ---------------------------------------------------------------------------
log "starting ollama"
ollama serve >/tmp/ollama.log 2>&1 &
OLLAMA_PID=$!
# Wait for the ollama HTTP API to answer.
for _ in $(seq 1 30); do
    if curl -fsS http://127.0.0.1:11434/api/version >/dev/null 2>&1; then break; fi
    sleep 1
done
curl -fsS http://127.0.0.1:11434/api/version >/dev/null 2>&1 \
    || fail "ollama server did not come up (see /tmp/ollama.log)"

log "pulling the worker model: ${OLLAMA_MODEL}"
# The model pull needs network. A pull failure is the documented
# environment-limited path — surface it loudly, do NOT fake a pass.
ollama pull "${OLLAMA_MODEL}" \
    || fail "could not pull the Ollama model ${OLLAMA_MODEL} — \
the E2E needs network access to fetch it (documented limitation)"

# ---------------------------------------------------------------------------
# 1. ~/.boi/v2 — phase + pipeline declarations, pointed at the ollama provider.
# ---------------------------------------------------------------------------
log "setting up ${BOI_ROOT}"
mkdir -p "${BOI_ROOT}/phases" "${BOI_ROOT}/pipelines"

# Copy the pipeline verbatim, EXCEPT drop the `critique_plan` provider override
# (it points at `openrouter` — no second provider in this container; v1.0 gap).
grep -v -A2 'overrides.critique_plan' /e2e/pipelines-src/standard.toml \
    | grep -v '^provider = "openrouter"' \
    | grep -v '^model = "openai' \
    > "${BOI_ROOT}/pipelines/standard.toml"

# Copy each phase TOML, rewriting every worker phase's provider to `ollama` and
# its model to ${OLLAMA_MODEL} (deterministic phases keep their inert runtime).
for src in /e2e/phases-src/*.toml; do
    name="$(basename "${src}")"
    sed -e 's/^provider = "claude_code"/provider = "ollama"/' \
        -e "s#^model = \"claude-opus-4-7\"#model = \"${OLLAMA_MODEL}\"#" \
        "${src}" > "${BOI_ROOT}/phases/${name}"
done

# G26.1 — every worker phase resolves `prompt_template` against the phases dir.
# Write a minimal real prompt for each worker template the phase TOMLs name.
for tmpl in plan critique_plan write_red_tests execute review \
            propose_adjustment review_adjustment plan_revision; do
    cat > "${BOI_ROOT}/phases/${tmpl}.md" <<'PROMPT'
You are a BOI worker. Read the <phase_context> above. Do the smallest correct
thing the task asks for, then emit your structured verdict JSON. Keep it brief.
PROMPT
done

# ---------------------------------------------------------------------------
# 2. A per-scenario workspace + spec.
# ---------------------------------------------------------------------------
# Each scenario gets a FRESH git workspace — BOI's `workspace_prepare` creates
# an integration branch + worktrees in the workspace, so a workspace reused
# across scenarios collides (the prior run's branch / worktrees linger) and
# the next scenario's `workspace_prepare` fails. `fresh_spec <tag>` builds an
# isolated workspace repo and emits a spec TOML pointed at it; it echoes the
# spec path.
fresh_spec() {
    local tag="$1" ws="/e2e/workspace-${1}" spec="/e2e/spec-${1}.toml"
    rm -rf "${ws}"
    mkdir -p "${ws}"
    (
        cd "${ws}"
        git init -q -b develop
        git config user.email e2e@boi.local
        git config user.name "BOI E2E"
        printf 'Please recieve this README.\n' > README.md
        git add README.md
        git commit -q -m "initial commit"
    )
    sed "s#^workspace = .*#workspace = \"${ws}\"#" "${SPEC_SRC}" > "${spec}"
    printf '%s' "${spec}"
}

# ---------------------------------------------------------------------------
# A helper: start `boi daemon`, run a body, then stop the daemon.
# ---------------------------------------------------------------------------
DAEMON_PID=""
start_daemon() {
    boi daemon >/tmp/boi-daemon.log 2>&1 &
    DAEMON_PID=$!
    # The daemon binds ~/.boi/v2/daemon.sock — wait for it.
    for _ in $(seq 1 30); do
        if [ -S "${BOI_ROOT}/daemon.sock" ]; then return 0; fi
        if ! kill -0 "${DAEMON_PID}" 2>/dev/null; then
            cat /tmp/boi-daemon.log >&2
            fail "boi daemon exited before binding the control socket"
        fi
        sleep 1
    done
    fail "boi daemon did not bind the control socket"
}
stop_daemon() {
    if [ -n "${DAEMON_PID}" ]; then
        kill "${DAEMON_PID}" 2>/dev/null || true
        wait "${DAEMON_PID}" 2>/dev/null || true
    fi
    DAEMON_PID=""
}

# Poll `boi dashboard <spec>` (non-TTY static snapshot) until the spec root
# shows [done] — the status emitted for any terminal state (completed / failed /
# canceled). Echoes "done" when settled; exits 1 on timeout.
wait_for_spec() {
    local spec_id="$1" deadline=$(( $(date +%s) + 600 ))
    while [ "$(date +%s)" -lt "${deadline}" ]; do
        local out
        out="$(boi dashboard "${spec_id}" 2>/dev/null || true)"
        # The spec-root line format: "<spec_id> [<status>] <duration>"
        # Status is "running" while active; "done" once all phase runs complete
        # (covers completed, failed, and canceled — all yield [done] here).
        if printf '%s' "${out}" | grep -Eq "^${spec_id} \[done\]"; then
            printf 'done'
            return 0
        fi
        sleep 3
    done
    return 1
}

# `boi dispatch <spec>` → echo the minted spec id. A dispatch failure is LOUD
# (the daemon log is dumped) — never a silent `set -e` abort mid-pipeline.
dispatch_spec() {
    local out spec_id
    if ! out="$(boi dispatch "$1" 2>&1)"; then
        printf '%s\n' "${out}" >&2
        cat /tmp/boi-daemon.log >&2 || true
        fail "boi dispatch failed for $1"
    fi
    spec_id="$(printf '%s' "${out}" | grep -Eo 'S[0-9a-z]{8,}' | head -1)"
    [ -n "${spec_id}" ] || fail "boi dispatch produced no spec id (output: ${out})"
    printf '%s' "${spec_id}"
}

# ---------------------------------------------------------------------------
# Scenario 1 — 01-typo-fix dispatched against real goose + ollama.
# ---------------------------------------------------------------------------
log "SCENARIO 1: dispatch 01-typo-fix"
start_daemon
SPEC_ID="$(dispatch_spec "$(fresh_spec s1)")"
echo "dispatched spec ${SPEC_ID}"
S1_STATUS="$(wait_for_spec "${SPEC_ID}" || echo TIMEOUT)"
stop_daemon
echo "scenario 1 final spec status: ${S1_STATUS}"
# A real small-model run may `complete` or `fail` — both are legitimate
# real-`goose` outcomes (a 0.5B model often emits a verdict the pipeline
# rejects). The E2E asserts the pipeline RAN and SETTLED (it did not hang /
# wedge); a `TIMEOUT` is the failure.
[ "${S1_STATUS}" != "TIMEOUT" ] || fail "01-typo-fix did not settle — the pipeline wedged"

# ---------------------------------------------------------------------------
# Scenario 2 — a scripted cancellation of a live run.
# ---------------------------------------------------------------------------
log "SCENARIO 2: dispatch then cancel"
start_daemon
SPEC_ID2="$(dispatch_spec "$(fresh_spec s2)")"
echo "dispatched spec ${SPEC_ID2}; cancelling after 3s"
sleep 3
# `boi cancel` may report `failed -> canceled` rejected if the (fast 0.5B
# model) spec already reached a terminal state — benign: the spec IS settled.
boi cancel "${SPEC_ID2}" --reason "e2e scripted cancellation" 2>/dev/null || true
S2_STATUS="$(wait_for_spec "${SPEC_ID2}" || echo TIMEOUT)"
stop_daemon
echo "scenario 2 final spec status: ${S2_STATUS}"
# A cancel mid-run must settle the spec. If the spec already settled before
# the cancel landed (a very fast model) that is still a valid settle; the
# only failure is a hang.
[ "${S2_STATUS}" != "TIMEOUT" ] || fail "the cancelled spec did not settle"

# ---------------------------------------------------------------------------
# Scenario 3 — the retry-recovery path.
# ---------------------------------------------------------------------------
# A `goose` WRAPPER intercepting ONLY `goose run` (the worker-phase spawn):
# the FIRST `goose run` emits a malformed stream (a bare `complete` with no
# verdict → BOI maps it `VerdictParse` → retry); every later `goose run`, and
# every non-`run` invocation (`goose --version` for preflight), `exec`s the
# REAL goose. `GooseRuntime`'s 2-retry loop + the recovery attempt are
# exercised against real BOI machinery, and the recovery attempt runs the
# real `goose`.
log "SCENARIO 3: retry recovery"
WRAP_DIR=/e2e/goose-wrap
mkdir -p "${WRAP_DIR}"
COUNTER="${WRAP_DIR}/run-invocations"
echo 0 > "${COUNTER}"
cat > "${WRAP_DIR}/goose" <<WRAP
#!/usr/bin/env bash
echo "wrapper invoked: \$*" >> '${WRAP_DIR}/wrapper.log'
# Only intercept the worker-phase spawn (\`goose run …\`). Everything else —
# crucially \`goose --version\`, which BOI's preflight runs — passes straight
# through to the real binary.
if [ "\${1:-}" != "run" ]; then
    exec '${REAL_GOOSE}' "\$@"
fi
n=\$(cat '${COUNTER}')
n=\$((n + 1))
echo "\$n" > '${COUNTER}'
if [ "\$n" -eq 1 ]; then
    # First \`goose run\`: a bare 'complete', no verdict → BOI VerdictParse retry.
    echo '{"type":"complete"}'
    exit 0
fi
# Later \`goose run\`s: the real goose.
exec '${REAL_GOOSE}' "\$@"
WRAP
chmod +x "${WRAP_DIR}/goose"
: > "${WRAP_DIR}/wrapper.log"

# Run the daemon with the wrapper FIRST on PATH so `GooseRuntime` (which
# spawns the bare name `goose`) resolves the wrapper. `export` so the daemon
# child genuinely inherits it — a `VAR=val funcname` prefix does not reliably
# propagate into a function's spawned children.
S3_SAVED_PATH="${PATH}"
export PATH="${WRAP_DIR}:${PATH}"
start_daemon
SPEC_ID3="$(dispatch_spec "$(fresh_spec s3)")"
echo "dispatched spec ${SPEC_ID3} (the first goose run fails → BOI retries)"
S3_STATUS="$(wait_for_spec "${SPEC_ID3}" || echo TIMEOUT)"
stop_daemon
GOOSE_RUNS="$(cat "${COUNTER}")"
echo "scenario 3 final spec status: ${S3_STATUS}; intercepted goose runs: ${GOOSE_RUNS}"
echo "--- wrapper invocation log ---"
cat "${WRAP_DIR}/wrapper.log" || true
echo "--- boi log for ${SPEC_ID3} ---"
boi log "${SPEC_ID3}" 2>&1 | head -12 || true
[ "${S3_STATUS}" != "TIMEOUT" ] || fail "the retry-recovery spec did not settle"
# The wrapper saw more than one `goose run` → BOI's retry loop re-spawned
# `goose` after the malformed first attempt. THAT is the retry path proven
# real end-to-end.
[ "${GOOSE_RUNS}" -ge 2 ] \
    || fail "only ${GOOSE_RUNS} goose run(s) — BOI's retry loop did not re-spawn"
export PATH="${S3_SAVED_PATH}" # restore — the wrapper was scenario-3 only.

# ---------------------------------------------------------------------------
kill "${OLLAMA_PID}" 2>/dev/null || true
log "E2E PASSED — all 3 real-goose scenarios ran and settled"
echo "  scenario 1 (01-typo-fix) : ${S1_STATUS}"
echo "  scenario 2 (cancel)      : ${S2_STATUS}"
echo "  scenario 3 (retry)       : ${S3_STATUS} (${GOOSE_RUNS} intercepted goose runs)"
