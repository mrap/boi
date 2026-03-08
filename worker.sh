#!/bin/bash
# worker.sh — Execute one iteration of a BOI spec.
#
# Launched by the daemon. Reads the spec.md, generates a prompt,
# runs Claude headlessly in a tmux session, captures output.
# After Claude exits, writes iteration metadata for telemetry.
#
# Usage:
#   bash worker.sh <queue-id> <worktree-path> <spec-path> <iteration>
#
# The worker:
#   1. Reads the spec.md
#   2. Counts PENDING tasks (exits 0 if none)
#   3. Generates a prompt with the full spec + iteration instructions
#   4. Writes a run script for the tmux session
#   5. Launches the run script in a tmux session (tmux -L boi)
#   6. Writes PID file for daemon monitoring
#   7. After Claude exits (inside tmux), the run script:
#      a. Re-reads the spec.md to count tasks
#      b. Writes iteration-{N}.json with metadata
#      c. Writes exit code file

set -uo pipefail

# Constants
BOI_STATE_DIR="${HOME}/.boi"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE_PATH="${SCRIPT_DIR}/templates/worker-prompt.md"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_info() { echo -e "${GREEN}[boi-worker]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[boi-worker]${NC} $1"; }
log_error() { echo -e "${RED}[boi-worker]${NC} $1" >&2; }

usage() {
    echo "Usage: bash worker.sh [--critic|--decompose|--evaluate] <queue-id> <worktree-path> <spec-path> <iteration>"
}

# Parse optional --critic or --decompose or --evaluate flag
CRITIC_MODE=false
DECOMPOSE_MODE=false
EVALUATE_MODE=false
if [[ "${1:-}" == "--critic" ]]; then
    CRITIC_MODE=true
    shift
elif [[ "${1:-}" == "--decompose" ]]; then
    DECOMPOSE_MODE=true
    shift
elif [[ "${1:-}" == "--evaluate" ]]; then
    EVALUATE_MODE=true
    shift
fi

# Validate arguments
if [[ $# -lt 4 ]]; then
    log_error "Missing required arguments."
    usage
    exit 2
fi

QUEUE_ID="$1"
WORKTREE_PATH="$2"
SPEC_PATH="$3"
ITERATION="$4"

# Derived paths
QUEUE_DIR="${BOI_STATE_DIR}/queue"
LOG_DIR="${BOI_STATE_DIR}/logs"
LOG_FILE="${LOG_DIR}/${QUEUE_ID}-iter-${ITERATION}.log"
PID_FILE="${QUEUE_DIR}/${QUEUE_ID}.pid"
PROMPT_FILE="${QUEUE_DIR}/${QUEUE_ID}.prompt.md"
RUN_SCRIPT="${QUEUE_DIR}/${QUEUE_ID}.run.sh"
EXIT_FILE="${QUEUE_DIR}/${QUEUE_ID}.exit"
ITERATION_FILE="${QUEUE_DIR}/${QUEUE_ID}.iteration-${ITERATION}.json"
TMUX_SESSION="boi-${QUEUE_ID}"

# Validate prerequisites
validate() {
    if [[ ! -f "${SPEC_PATH}" ]]; then
        log_error "Spec file not found: ${SPEC_PATH}"
        exit 2
    fi

    if [[ ! -d "${WORKTREE_PATH}" ]]; then
        log_error "Worktree path does not exist: ${WORKTREE_PATH}"
        exit 2
    fi

    if [[ "${CRITIC_MODE}" != "true" ]] && [[ "${DECOMPOSE_MODE}" != "true" ]] && [[ "${EVALUATE_MODE}" != "true" ]] && [[ ! -f "${TEMPLATE_PATH}" ]]; then
        log_error "Worker prompt template not found: ${TEMPLATE_PATH}"
        exit 2
    fi

    if ! command -v claude &>/dev/null; then
        log_error "claude CLI not found in PATH."
        exit 2
    fi

    mkdir -p "${LOG_DIR}" "${QUEUE_DIR}"
}

# Count tasks in the spec. Returns "pending done skipped total" on one line.
count_tasks() {
    local spec_file="$1"
    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${spec_file}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.spec_parser import count_boi_tasks
counts = count_boi_tasks(sys.argv[1])
print(f"{counts['pending']} {counts['done']} {counts['skipped']} {counts['total']}")
PYEOF
}

# Generate the worker prompt from template + spec content
generate_prompt() {
    local pending_count="$1"

    if [[ "${CRITIC_MODE}" == "true" ]]; then
        # In critic mode, use the pre-generated critic prompt
        local critic_prompt_file="${QUEUE_DIR}/${QUEUE_ID}.critic-prompt.md"
        if [[ ! -f "${critic_prompt_file}" ]]; then
            log_error "Critic prompt file not found: ${critic_prompt_file}"
            exit 2
        fi

        local critic_template="${SCRIPT_DIR}/templates/critic-worker-prompt.md"
        if [[ ! -f "${critic_template}" ]]; then
            log_error "Critic worker template not found: ${critic_template}"
            exit 2
        fi

        python3 - \
            "${critic_template}" \
            "${critic_prompt_file}" \
            "${PROMPT_FILE}" <<'PYEOF'
import sys, os

template_path, critic_prompt_path, prompt_file = sys.argv[1:4]

with open(template_path, "r") as f:
    template = f.read()

with open(critic_prompt_path, "r") as f:
    critic_prompt = f.read()

result = template.replace("{{CRITIC_PROMPT}}", critic_prompt)

# Write atomically
tmp = prompt_file + ".tmp"
with open(tmp, "w") as f:
    f.write(result)
os.rename(tmp, prompt_file)
PYEOF

        log_info "Critic prompt generated: ${PROMPT_FILE}"
        return
    fi

    if [[ "${DECOMPOSE_MODE}" == "true" ]]; then
        # In decompose mode, use the decomposition prompt template
        local decompose_template="${SCRIPT_DIR}/templates/generate-decompose-prompt.md"
        if [[ ! -f "${decompose_template}" ]]; then
            log_error "Decompose prompt template not found: ${decompose_template}"
            exit 2
        fi

        python3 - \
            "${decompose_template}" \
            "${SPEC_PATH}" \
            "${PROMPT_FILE}" <<'PYEOF'
import sys, os

template_path, spec_path, prompt_file = sys.argv[1:4]

with open(template_path, "r") as f:
    template = f.read()

with open(spec_path, "r") as f:
    spec_content = f.read()

result = template.replace("{{SPEC_CONTENT}}", spec_content)
result = result.replace("{{SPEC_PATH}}", spec_path)

# Write atomically
tmp = prompt_file + ".tmp"
with open(tmp, "w") as f:
    f.write(result)
os.rename(tmp, prompt_file)
PYEOF

        log_info "Decompose prompt generated: ${PROMPT_FILE}"
        return
    fi

    if [[ "${EVALUATE_MODE}" == "true" ]]; then
        # In evaluate mode, use the evaluation prompt template
        local evaluate_template="${SCRIPT_DIR}/templates/evaluate-prompt.md"
        if [[ ! -f "${evaluate_template}" ]]; then
            log_error "Evaluate prompt template not found: ${evaluate_template}"
            exit 2
        fi

        python3 - \
            "${evaluate_template}" \
            "${SPEC_PATH}" \
            "${PROMPT_FILE}" <<'PYEOF'
import sys, os

template_path, spec_path, prompt_file = sys.argv[1:4]

with open(template_path, "r") as f:
    template = f.read()

with open(spec_path, "r") as f:
    spec_content = f.read()

result = template.replace("{{SPEC_CONTENT}}", spec_content)
result = result.replace("{{SPEC_PATH}}", spec_path)

# Write atomically
tmp = prompt_file + ".tmp"
with open(tmp, "w") as f:
    f.write(result)
os.rename(tmp, prompt_file)
PYEOF

        log_info "Evaluate prompt generated: ${PROMPT_FILE}"
        return
    fi

    local queue_entry_file="${QUEUE_DIR}/${QUEUE_ID}.json"

    python3 - \
        "${TEMPLATE_PATH}" \
        "${SPEC_PATH}" \
        "${ITERATION}" \
        "${QUEUE_ID}" \
        "${pending_count}" \
        "${PROMPT_FILE}" \
        "${queue_entry_file}" \
        "${SCRIPT_DIR}/templates/modes" <<'PYEOF'
import sys
import os
import json
import re

template_path, spec_path, iteration, queue_id, pending_count, prompt_file, queue_entry_file, modes_dir = sys.argv[1:9]

with open(template_path, "r") as f:
    template = f.read()

with open(spec_path, "r") as f:
    spec_content = f.read()

# --- Determine mode ---
# Priority: spec header > queue entry > default (execute)
mode = "execute"

# 1. Try queue entry
if os.path.isfile(queue_entry_file):
    with open(queue_entry_file, "r") as f:
        queue_entry = json.load(f)
    mode = queue_entry.get("mode", "execute") or "execute"
else:
    queue_entry = {}

# 2. Check spec header for **Mode:** override
mode_match = re.search(r'^\*\*Mode:\*\*\s*(\w+)', spec_content, re.MULTILINE)
if mode_match:
    spec_mode = mode_match.group(1).strip().lower()
    valid_modes = {"execute", "challenge", "discover", "generate"}
    if spec_mode in valid_modes:
        mode = spec_mode

# --- Load mode fragment ---
mode_file = os.path.join(modes_dir, f"{mode}.md")
if os.path.isfile(mode_file):
    with open(mode_file, "r") as f:
        mode_fragment = f.read()
else:
    # Fallback: use execute mode if file missing
    fallback = os.path.join(modes_dir, "execute.md")
    if os.path.isfile(fallback):
        with open(fallback, "r") as f:
            mode_fragment = f.read()
    else:
        mode_fragment = "## Mode: Execute\n\nExecute the current task as specified.\n"

# --- Handle experiment budget ---
max_budget = queue_entry.get("max_experiment_invocations", 0)
used_budget = queue_entry.get("experiment_invocations_used", 0)
remaining = max(0, max_budget - used_budget)

if max_budget == 0:
    budget_text = "0. Experiments are disabled in this mode."
elif remaining == 0:
    budget_text = "EXHAUSTED. Do not propose alternatives. Implement per spec."
else:
    budget_text = f"{remaining} remaining ({used_budget} of {max_budget} used)"

mode_fragment = mode_fragment.replace("{{EXPERIMENT_BUDGET}}", budget_text)
mode_fragment = mode_fragment.replace("{{QUEUE_ID}}", queue_id)

# --- Load project context ---
project_name = queue_entry.get("project") or ""
project_context = ""
if project_name:
    projects_dir = os.path.expanduser("~/.boi/projects")
    context_file = os.path.join(projects_dir, project_name, "context.md")
    research_file = os.path.join(projects_dir, project_name, "research.md")
    parts = []
    if os.path.isfile(context_file):
        with open(context_file, "r") as f:
            parts.append(f.read().rstrip())
    if os.path.isfile(research_file):
        with open(research_file, "r") as f:
            parts.append(f.read().rstrip())
    if parts:
        project_context = "## Project Context\n\n" + "\n\n".join(parts)

# --- Replace template placeholders ---
# Replace non-content placeholders first, then inject spec content LAST
# so that any {{ }} patterns in the spec content are not processed.
result = template.replace("{{ITERATION}}", iteration)
result = result.replace("{{QUEUE_ID}}", queue_id)
result = result.replace("{{SPEC_PATH}}", spec_path)
result = result.replace("{{PENDING_COUNT}}", pending_count)
result = result.replace("{{MODE_RULES}}", mode_fragment)
result = result.replace("{{PROJECT}}", project_name)
result = result.replace("{{PROJECT_CONTEXT}}", project_context)
result = result.replace("{{SPEC_CONTENT}}", spec_content)

# Write atomically
tmp = prompt_file + ".tmp"
with open(tmp, "w") as f:
    f.write(result)

os.rename(tmp, prompt_file)
PYEOF

    log_info "Prompt generated: ${PROMPT_FILE}"
}

# Generate the run script that executes inside the tmux session.
# This script tracks timing, runs Claude, counts tasks before/after,
# and writes iteration metadata.
generate_run_script() {
    local pre_pending="$1"
    local pre_done="$2"
    local pre_skipped="$3"
    local pre_total="$4"

    cat > "${RUN_SCRIPT}" <<RUNEOF
#!/bin/bash
# Auto-generated BOI worker run script for iteration ${ITERATION}.
# Runs inside a tmux session. Do not edit manually.
set -uo pipefail

# ── Config (baked in at generation time) ──────────────────────────────────
_BOI_SCRIPT_DIR="${SCRIPT_DIR}"
_SPEC_PATH="${SPEC_PATH}"
_QUEUE_ID="${QUEUE_ID}"
_ITERATION="${ITERATION}"
_LOG_FILE="${LOG_FILE}"
_EXIT_FILE="${EXIT_FILE}"
_ITERATION_FILE="${ITERATION_FILE}"
_WORKTREE_PATH="${WORKTREE_PATH}"
_PROMPT_FILE="${PROMPT_FILE}"
_PRE_PENDING=${pre_pending}
_PRE_DONE=${pre_done}
_PRE_SKIPPED=${pre_skipped}
_PRE_TOTAL=${pre_total}

# ── Record start time ────────────────────────────────────────────────────
_START_TIME=\$(date +%s)
_START_ISO=\$(date -u +"%Y-%m-%dT%H:%M:%SZ")

# ── Run Claude ───────────────────────────────────────────────────────────
cd "\${_WORKTREE_PATH}"
env -u CLAUDECODE claude -p "\$(cat "\${_PROMPT_FILE}")" --dangerously-skip-permissions > "\${_LOG_FILE}" 2>&1
_CLAUDE_EXIT=\$?

# ── Record end time ──────────────────────────────────────────────────────
_END_TIME=\$(date +%s)
_DURATION=\$((_END_TIME - _START_TIME))

# ── Count post-iteration tasks ───────────────────────────────────────────
_POST_COUNTS=\$(BOI_SCRIPT_DIR="\${_BOI_SCRIPT_DIR}" python3 - "\${_SPEC_PATH}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.spec_parser import count_boi_tasks
counts = count_boi_tasks(sys.argv[1])
print(f"{counts['pending']} {counts['done']} {counts['skipped']} {counts['total']}")
PYEOF
)

_POST_PENDING=\$(echo "\${_POST_COUNTS}" | awk '{print \$1}')
_POST_DONE=\$(echo "\${_POST_COUNTS}" | awk '{print \$2}')
_POST_SKIPPED=\$(echo "\${_POST_COUNTS}" | awk '{print \$3}')
_POST_TOTAL=\$(echo "\${_POST_COUNTS}" | awk '{print \$4}')

# ── Calculate deltas ─────────────────────────────────────────────────────
_TASKS_COMPLETED=\$((_POST_DONE - _PRE_DONE))
_TASKS_ADDED=\$((_POST_TOTAL - _PRE_TOTAL))
_TASKS_SKIPPED_DELTA=\$((_POST_SKIPPED - _PRE_SKIPPED))

# Clamp to zero
if [[ \${_TASKS_COMPLETED} -lt 0 ]]; then _TASKS_COMPLETED=0; fi
if [[ \${_TASKS_ADDED} -lt 0 ]]; then _TASKS_ADDED=0; fi
if [[ \${_TASKS_SKIPPED_DELTA} -lt 0 ]]; then _TASKS_SKIPPED_DELTA=0; fi

# ── Write iteration metadata ─────────────────────────────────────────────
BOI_SCRIPT_DIR="\${_BOI_SCRIPT_DIR}" python3 - \\
    "\${_ITERATION_FILE}" \\
    "\${_QUEUE_ID}" \\
    "\${_ITERATION}" \\
    "\${_CLAUDE_EXIT}" \\
    "\${_DURATION}" \\
    "\${_START_ISO}" \\
    "\${_PRE_PENDING}" "\${_PRE_DONE}" "\${_PRE_SKIPPED}" "\${_PRE_TOTAL}" \\
    "\${_POST_PENDING}" "\${_POST_DONE}" "\${_POST_SKIPPED}" "\${_POST_TOTAL}" \\
    "\${_TASKS_COMPLETED}" "\${_TASKS_ADDED}" "\${_TASKS_SKIPPED_DELTA}" <<'PYEOF'
import json, sys, os

target = sys.argv[1]
data = {
    "queue_id": sys.argv[2],
    "iteration": int(sys.argv[3]),
    "exit_code": int(sys.argv[4]),
    "duration_seconds": int(sys.argv[5]),
    "started_at": sys.argv[6],
    "pre_counts": {
        "pending": int(sys.argv[7]),
        "done": int(sys.argv[8]),
        "skipped": int(sys.argv[9]),
        "total": int(sys.argv[10]),
    },
    "post_counts": {
        "pending": int(sys.argv[11]),
        "done": int(sys.argv[12]),
        "skipped": int(sys.argv[13]),
        "total": int(sys.argv[14]),
    },
    "tasks_completed": int(sys.argv[15]),
    "tasks_added": int(sys.argv[16]),
    "tasks_skipped": int(sys.argv[17]),
}

tmp = target + ".tmp"
with open(tmp, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
os.rename(tmp, target)
PYEOF

# ── Write exit code ──────────────────────────────────────────────────────
echo "\${_CLAUDE_EXIT}" > "\${_EXIT_FILE}"
RUNEOF

    chmod +x "${RUN_SCRIPT}"
    log_info "Run script generated: ${RUN_SCRIPT}"
}

# Launch the run script in a tmux session
launch_worker() {
    # Clean up stale tmux session
    if tmux -L boi has-session -t "${TMUX_SESSION}" 2>/dev/null; then
        log_warn "Stale tmux session '${TMUX_SESSION}' found, killing it."
        tmux -L boi kill-session -t "${TMUX_SESSION}" 2>/dev/null
    fi

    # Remove stale exit code file
    rm -f "${EXIT_FILE}"

    # Launch in detached tmux session
    tmux -L boi new-session -d -s "${TMUX_SESSION}" bash "${RUN_SCRIPT}"

    sleep 1

    # Get the PID of the bash process inside the tmux session
    local pane_pid
    pane_pid=$(tmux -L boi list-panes -t "${TMUX_SESSION}" -F '#{pane_pid}' 2>/dev/null)

    if [[ -z "${pane_pid}" ]]; then
        log_error "Failed to get PID from tmux session."
        return 1
    fi

    # Write PID file atomically
    local tmp="${PID_FILE}.tmp"
    echo "${pane_pid}" > "${tmp}"
    mv "${tmp}" "${PID_FILE}"

    log_info "Worker launched: tmux session '${TMUX_SESSION}', PID ${pane_pid}"
    log_info "Log file: ${LOG_FILE}"

    return 0
}

# Main
main() {
    log_info "Starting worker for spec ${QUEUE_ID} (iteration ${ITERATION})"
    log_info "Worktree: ${WORKTREE_PATH}"
    log_info "Spec: ${SPEC_PATH}"
    if [[ "${CRITIC_MODE}" == "true" ]]; then
        log_info "Mode: CRITIC"
    elif [[ "${DECOMPOSE_MODE}" == "true" ]]; then
        log_info "Mode: DECOMPOSE"
    elif [[ "${EVALUATE_MODE}" == "true" ]]; then
        log_info "Mode: EVALUATE"
    fi

    validate

    # Count tasks before iteration
    local counts
    counts=$(count_tasks "${SPEC_PATH}")
    local pre_pending pre_done pre_skipped pre_total
    pre_pending=$(echo "${counts}" | awk '{print $1}')
    pre_done=$(echo "${counts}" | awk '{print $2}')
    pre_skipped=$(echo "${counts}" | awk '{print $3}')
    pre_total=$(echo "${counts}" | awk '{print $4}')

    if [[ "${pre_pending}" == "0" ]] && [[ "${CRITIC_MODE}" != "true" ]] && [[ "${DECOMPOSE_MODE}" != "true" ]] && [[ "${EVALUATE_MODE}" != "true" ]]; then
        log_info "No PENDING tasks in spec. Exiting with success."
        # Write exit file so the daemon can detect completion without a PID
        echo "0" > "${EXIT_FILE}"
        exit 0
    fi

    if [[ "${CRITIC_MODE}" == "true" ]]; then
        log_info "Critic mode: reviewing spec with ${pre_done} DONE task(s)."
    elif [[ "${DECOMPOSE_MODE}" == "true" ]]; then
        log_info "Decompose mode: breaking goal into tasks."
    elif [[ "${EVALUATE_MODE}" == "true" ]]; then
        log_info "Evaluate mode: checking Success Criteria against implementation."
    else
        log_info "${pre_pending} PENDING task(s) found."
    fi

    generate_prompt "${pre_pending}"
    generate_run_script "${pre_pending}" "${pre_done}" "${pre_skipped}" "${pre_total}"
    launch_worker

    local rc=$?
    if [[ ${rc} -ne 0 ]]; then
        log_error "Failed to launch worker."
        exit 1
    fi

    log_info "Worker is running. Monitor with: tmux -L boi attach -t ${TMUX_SESSION}"
}

main
