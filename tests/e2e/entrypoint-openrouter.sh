#!/usr/bin/env bash
# BOI v2 — the in-container Docker E2E harness, OpenRouter variant.
#
# Sibling to `entrypoint.sh`. Same 3 scenarios (dispatch / cancel /
# retry-recovery of `01-typo-fix`), but workers run against OpenRouter
# (model = ${MODEL:-anthropic/claude-haiku-4.5}) instead of a local Ollama. A
# cheap-capable E2E for v1.0 confidence — a real cloud model can actually fix
# the typo, so scenario 1 holds the pipeline to the work, not just to "settled".
#
# Required env vars (passed via `docker run --env-file …` / `-e …`):
#   OPENROUTER_API_KEY  (the OpenRouter credential)
#   MODEL               (optional; default `anthropic/claude-haiku-4.5`)
#
# Run via `just e2e-openrouter` — bind-mounts THIS script over the COPYd one.
set -euo pipefail

log()  { printf '\n=== %s ===\n' "$*"; }
fail() { printf '\nE2E FAIL: %s\n' "$*" >&2; exit 1; }

BOI_ROOT="${HOME}/.boi/v2"
SPEC_SRC=/e2e/specs/01-typo-fix.toml
# OpenRouter model IDs use a period in the version suffix (haiku-4.5 not haiku-4-5).
# Haiku-4.5 chosen over gpt-4o-mini: more disciplined at following declared task
# contracts (gpt-4o-mini hallucinated a Python dependency on 2026-05-23 instead of
# using the grep-based verification the task already declared).
MODEL="${MODEL:-anthropic/claude-haiku-4.5}"
REAL_GOOSE="$(command -v goose)"

# ---------------------------------------------------------------------------
# 0. Preflight — credentials + binaries.
# ---------------------------------------------------------------------------
log "preflight"
[ -n "${OPENROUTER_API_KEY:-}" ] \
    || fail "OPENROUTER_API_KEY not set — pass via \`docker run --env-file …\`"
command -v goose >/dev/null \
    || fail "goose not on PATH"
echo "model=${MODEL}  key=set(${#OPENROUTER_API_KEY}c)  goose=$(goose --version 2>&1 | tr -d ' ')"

# ---------------------------------------------------------------------------
# 1. ~/.boi/v2 — phase + pipeline declarations, pointed at openrouter + ${MODEL}.
# ---------------------------------------------------------------------------
log "setting up ${BOI_ROOT}"
mkdir -p "${BOI_ROOT}/phases" "${BOI_ROOT}/pipelines"

# Pipeline: copy verbatim, but drop the `critique_plan` provider override —
# every phase runs on the same openrouter + ${MODEL} here.
grep -v -A2 'overrides.critique_plan' /e2e/pipelines-src/standard.toml \
    | grep -v '^provider = "openrouter"' \
    | grep -v '^model = "openai' \
    > "${BOI_ROOT}/pipelines/standard.toml"

# Phase TOMLs: rewrite every worker phase's provider → `openrouter`, model →
# ${MODEL}. The deterministic phases keep their inert runtime.
for src in /e2e/phases-src/*.toml; do
    name="$(basename "${src}")"
    sed -e 's/^provider = "claude_code"/provider = "openrouter"/' \
        -e "s#^model = \"claude-opus-4-7\"#model = \"${MODEL}\"#" \
        "${src}" > "${BOI_ROOT}/phases/${name}"
done

# G26.1 — every worker phase resolves `prompt_template` against the phases
# dir. Copy the REAL hardened templates from `tests/fixtures/phases/*.md`
# (the Dockerfile mounts them at /e2e/phases-src). The Ollama-era sibling
# overwrites these with a minimal stub because qwen2.5:0.5b cannot follow
# them anyway; a capable cloud model needs (and uses) the real prompts —
# specifically `execute.md`'s post-fabrication-fix language ("forces real
# tool use, forbids fabricated verdicts"). Stub only the few that ship
# without a `.md` (the adjustment / plan-revision branches are not on the
# 01-typo-fix happy path).
cp /e2e/phases-src/*.md "${BOI_ROOT}/phases/" 2>/dev/null || true
for tmpl in plan critique_plan write_red_tests execute review \
            propose_adjustment review_adjustment plan_revision; do
    [ -f "${BOI_ROOT}/phases/${tmpl}.md" ] && continue
    cat > "${BOI_ROOT}/phases/${tmpl}.md" <<'PROMPT'
You are a BOI worker. Read the <phase_context> above. Do the smallest correct
thing the task asks for, then emit your structured verdict JSON. Keep it brief.
PROMPT
done
echo "--- ${BOI_ROOT}/phases (post-provision) ---"
ls -la "${BOI_ROOT}/phases/" | head -25
echo "execute.md size: $(wc -c < "${BOI_ROOT}/phases/execute.md" 2>/dev/null || echo MISSING) bytes"

# ---------------------------------------------------------------------------
# 2. A per-scenario workspace + spec. Same logic as entrypoint.sh.
# ---------------------------------------------------------------------------
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
# Daemon helpers — identical to entrypoint.sh.
# ---------------------------------------------------------------------------
DAEMON_PID=""
start_daemon() {
    RUST_LOG="${RUST_LOG:-info,boi=debug}" boi daemon >/tmp/boi-daemon.log 2>&1 &
    DAEMON_PID=$!
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

wait_for_spec() {
    # Settle on EITHER dashboard `[done]` (no active phases) OR `boi log`
    # showing a `canceled` spec status. Both are terminal.
    local spec_id="$1" deadline=$(( $(date +%s) + 600 ))
    while [ "$(date +%s)" -lt "${deadline}" ]; do
        local dash log
        dash="$(boi dashboard "${spec_id}" 2>/dev/null || true)"
        if printf '%s' "${dash}" | grep -Eq "^${spec_id} \[done\]"; then
            printf 'done'
            return 0
        fi
        log="$(boi log "${spec_id}" 2>/dev/null || true)"
        if printf '%s' "${log}" | grep -Eq "spec ${spec_id} (canceled|failed)"; then
            printf 'canceled'
            return 0
        fi
        sleep 3
    done
    return 1
}

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
# Scenario 1 — 01-typo-fix on openrouter + ${MODEL}.
#
# Holds the pipeline to the WORK, not just "settled": a capable model can
# actually fix the typo, so the integration branch must show "receive".
# ---------------------------------------------------------------------------
log "SCENARIO 1: dispatch 01-typo-fix"
start_daemon
SPEC_ID="$(dispatch_spec "$(fresh_spec s1)")"
echo "dispatched spec ${SPEC_ID}"
S1_STATUS="$(wait_for_spec "${SPEC_ID}" || echo TIMEOUT)"
stop_daemon
echo "scenario 1 final spec status: ${S1_STATUS}"
[ "${S1_STATUS}" != "TIMEOUT" ] || fail "01-typo-fix did not settle — the pipeline wedged"

echo "--- boi log ${SPEC_ID} ---"
boi log "${SPEC_ID}" 2>&1 | head -40 || true
echo "--- boi dashboard ${SPEC_ID} ---"
boi dashboard "${SPEC_ID}" 2>&1 | head -25 || true
echo "--- workspace state ---"
(cd /e2e/workspace-s1 && git --no-pager log --all --oneline 2>&1 | head -10; echo "branches:"; git --no-pager branch -a 2>&1) || true
echo "--- /tmp/boi-daemon.log (FULL) ---"
cat /tmp/boi-daemon.log 2>&1 || echo MISSING
echo "--- end daemon log ---"
echo "--- verdict bodies for spec ${SPEC_ID} ---"
boi_db="$(find "${BOI_ROOT}" -maxdepth 3 -name '*.db' -o -name 'boi.sqlite*' 2>/dev/null | head -1)"
echo "boi db = ${boi_db}"
sqlite3 "${boi_db}" ".schema phase_runs" 2>&1 | head -25 || true
sqlite3 "${boi_db}" \
  "SELECT phase, phase_iteration, provider, synopsis, verdict FROM phase_runs WHERE spec_id = '${SPEC_ID}' ORDER BY started_at;" 2>&1 || echo "sqlite query failed"
echo "--- goose sessions.db tables + last few rows ---"
goose_db="${HOME}/.local/share/goose/sessions/sessions.db"
sqlite3 "${goose_db}" ".tables" 2>&1 || true
sqlite3 "${goose_db}" ".schema messages" 2>&1 | head -10 || true
sqlite3 -separator '|' "${goose_db}" "SELECT id, role, length(content_json), substr(content_json, 1, 6000) FROM messages ORDER BY id DESC LIMIT 8;" 2>&1 || true
echo "--- recipes written by BOI ---"
find "${BOI_ROOT}/recipes/" -type f 2>&1 | head -20 || true
echo "--- first written recipe ---"
first_recipe="$(find "${BOI_ROOT}/recipes/" -type f -name '*.yaml' 2>/dev/null | head -1)"
[ -n "${first_recipe}" ] && { echo "=== ${first_recipe} (FULL) ==="; cat "${first_recipe}" 2>&1; echo "=== end recipe ==="; } || echo "no .yaml recipes found"

INTEG_README="$(cd /e2e/workspace-s1 \
    && git show "spec/${SPEC_ID}/integration:README.md" 2>/dev/null \
    || echo '<missing>')"
if printf '%s' "${INTEG_README}" | grep -q 'receive' \
   && ! printf '%s' "${INTEG_README}" | grep -q 'recieve'; then
    echo "A1 PASS — typo fixed on the integration branch"
else
    echo "A1 FAIL — integration README still wrong: [${INTEG_README}]"
    fail "scenario 1: the work did not happen"
fi

# ---------------------------------------------------------------------------
# Scenario 2 — scripted cancellation of a live run.
# ---------------------------------------------------------------------------
log "SCENARIO 2: dispatch then cancel"
start_daemon
SPEC_ID2="$(dispatch_spec "$(fresh_spec s2)")"
echo "dispatched spec ${SPEC_ID2}; cancelling after 3s"
sleep 3
# A spec that already reached a terminal state will reject the cancel
# benignly — the only failure is a hang.
boi cancel "${SPEC_ID2}" --reason "e2e scripted cancellation" 2>/dev/null || true
S2_STATUS="$(wait_for_spec "${SPEC_ID2}" || echo TIMEOUT)"
stop_daemon
echo "scenario 2 final spec status: ${S2_STATUS}"
[ "${S2_STATUS}" != "TIMEOUT" ] || fail "the cancelled spec did not settle"

# ---------------------------------------------------------------------------
# Scenario 3 — retry-recovery via a goose wrapper. Identical mechanism to
# entrypoint.sh: the wrapper hijacks only `goose run`, emits malformed JSON
# on the first invocation (→ BOI VerdictParse retry), and `exec`s the real
# goose afterwards. Provider-agnostic.
# ---------------------------------------------------------------------------
log "SCENARIO 3: retry recovery"
WRAP_DIR=/e2e/goose-wrap
mkdir -p "${WRAP_DIR}"
COUNTER="${WRAP_DIR}/run-invocations"
echo 0 > "${COUNTER}"
cat > "${WRAP_DIR}/goose" <<WRAP
#!/usr/bin/env bash
echo "wrapper invoked: \$*" >> '${WRAP_DIR}/wrapper.log'
if [ "\${1:-}" != "run" ]; then
    exec '${REAL_GOOSE}' "\$@"
fi
n=\$(cat '${COUNTER}')
n=\$((n + 1))
echo "\$n" > '${COUNTER}'
if [ "\$n" -eq 1 ]; then
    echo '{"type":"complete"}'
    exit 0
fi
exec '${REAL_GOOSE}' "\$@"
WRAP
chmod +x "${WRAP_DIR}/goose"
: > "${WRAP_DIR}/wrapper.log"

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
[ "${GOOSE_RUNS}" -ge 2 ] \
    || fail "only ${GOOSE_RUNS} goose run(s) — BOI's retry loop did not re-spawn"
export PATH="${S3_SAVED_PATH}"

# ---------------------------------------------------------------------------
log "E2E PASSED — all 3 scenarios ran and settled on openrouter/${MODEL}"
echo "  scenario 1 (01-typo-fix, work-asserted) : ${S1_STATUS}"
echo "  scenario 2 (cancel)                     : ${S2_STATUS}"
echo "  scenario 3 (retry)                      : ${S3_STATUS} (${GOOSE_RUNS} intercepted goose runs)"
