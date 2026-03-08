#!/bin/bash
# boi.sh — CLI entry point for BOI (Beginning of Infinity).
#
# Routes subcommands to their implementations:
#   install   — Run install.sh (create worktrees, write config)
#   dispatch  — Submit a spec to the queue, start daemon
#   queue     — Show spec queue with status, iteration count, priority
#   status    — Show workers + current spec assignments + iteration progress
#   log       — Show logs for a spec (current or latest iteration)
#   cancel    — Cancel a queued/running spec
#   review    — Review experiment proposals on a paused spec
#   stop      — Stop daemon and all workers
#   workers   — Show worktree/worker availability
#   telemetry — Show per-iteration breakdown for a spec
#   doctor    — Check prerequisites and environment health
#   dashboard — Live-updating queue progress
#   spec      — Live spec management (add, skip, reorder, block, edit tasks)
#   project   — Manage BOI projects (create, list, status, context, delete)
#   do        — Natural language to BOI commands via LLM
#
# Usage:
#   boi dispatch --spec spec.md [--priority N] [--max-iter N] [--mode MODE]
#   boi dispatch --tasks tasks.md
#   boi queue
#   boi status [--watch]
#   boi log <queue-id> [--full]
#   boi cancel <queue-id>
#   boi stop
#   boi workers
#   boi telemetry <queue-id>
#   boi dashboard
#   boi purge [--all] [--dry-run]
#   boi install [--workers N]
#   boi --version

set -uo pipefail

# Constants
BOI_VERSION="0.1.0"
BOI_STATE_DIR="${HOME}/.boi"
BOI_CONFIG="${BOI_STATE_DIR}/config.json"
QUEUE_DIR="${BOI_STATE_DIR}/queue"
EVENTS_DIR="${BOI_STATE_DIR}/events"
LOG_DIR="${BOI_STATE_DIR}/logs"
PID_FILE="${BOI_STATE_DIR}/daemon.pid"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

# ─── Helpers ─────────────────────────────────────────────────────────────────

die() {
    echo -e "${RED}Error:${NC} $1" >&2
    exit 1
}

warn() {
    echo -e "${YELLOW}Warning:${NC} $1"
}

info() {
    echo -e "${GREEN}boi:${NC} $1"
}

require_config() {
    if [[ ! -f "${BOI_CONFIG}" ]]; then
        die "BOI not installed. Run 'boi install' first to create worktrees and config."
    fi
}

require_daemon() {
    if [[ ! -f "${PID_FILE}" ]]; then
        return 1
    fi
    local pid
    pid=$(cat "${PID_FILE}")
    if kill -0 "${pid}" 2>/dev/null; then
        return 0
    fi
    return 1
}

# ─── Subcommand: install ─────────────────────────────────────────────────────

cmd_install() {
    exec bash "${SCRIPT_DIR}/install.sh" "$@"
}

# ─── Subcommand: dispatch ────────────────────────────────────────────────────

cmd_dispatch() {
    local spec_file=""
    local tasks_file=""
    local priority=100
    local max_iter=30
    local worktree=""
    local timeout=""
    local no_critic=false
    local mode=""
    local dry_run=false
    local project=""
    local experiment_budget=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --spec)
                [[ -z "${2:-}" ]] && die "--spec requires a file path"
                spec_file="$2"
                shift 2
                ;;
            --tasks)
                [[ -z "${2:-}" ]] && die "--tasks requires a file path"
                tasks_file="$2"
                shift 2
                ;;
            --priority)
                [[ -z "${2:-}" ]] && die "--priority requires a number"
                priority="$2"
                shift 2
                ;;
            --max-iter)
                [[ -z "${2:-}" ]] && die "--max-iter requires a number"
                max_iter="$2"
                shift 2
                ;;
            --checkout|--worktree)
                [[ -z "${2:-}" ]] && die "--worktree requires a path"
                worktree="$2"
                shift 2
                ;;
            --timeout)
                [[ -z "${2:-}" ]] && die "--timeout requires seconds"
                timeout="$2"
                shift 2
                ;;
            --no-critic)
                no_critic=true
                shift
                ;;
            --mode|-m)
                [[ -z "${2:-}" ]] && die "--mode requires a value (execute, challenge, discover, generate)"
                mode="$2"
                shift 2
                ;;
            --project)
                [[ -z "${2:-}" ]] && die "--project requires a project name"
                project="$2"
                shift 2
                ;;
            --experiment-budget)
                [[ -z "${2:-}" ]] && die "--experiment-budget requires a number"
                experiment_budget="$2"
                shift 2
                ;;
            --dry-run)
                dry_run=true
                shift
                ;;
            -h|--help)
                echo "Usage: boi dispatch --spec <spec.md> [--priority N] [--max-iter N] [--mode MODE] [--experiment-budget N] [--worktree <path>] [--timeout SECONDS] [--no-critic] [--project <name>] [--dry-run]"
                echo "       boi dispatch --tasks <tasks.md>"
                echo ""
                echo "Options:"
                echo "  --spec FILE       Dispatch a self-evolving spec"
                echo "  --tasks FILE      Dispatch from a tasks.md file (backward compat)"
                echo "  --priority N      Queue priority (lower = higher, default: 100)"
                echo "  --max-iter N      Maximum iterations (default: 30)"
                echo "  --mode, -m MODE   Worker mode: execute (e), challenge (c), discover (d), generate (g). Default: execute"
                echo "  --experiment-budget N  Max experiment proposals (default: per mode)"
                echo "  --worktree PATH   Pin to a specific worktree"
                echo "  --timeout SECS    Per-iteration timeout in seconds (default: from config or 1800)"
                echo "  --no-critic       Skip critic validation when spec completes"
                echo "  --project NAME    Associate with a BOI project"
                echo "  --dry-run         Validate and show what would be dispatched without enqueueing"
                exit 0
                ;;
            *)
                die "Unknown option: $1. Use 'boi dispatch --help' for usage."
                ;;
        esac
    done

    # Resolve mode aliases and validate
    if [[ -n "${mode}" ]]; then
        case "${mode}" in
            execute|e)   mode="execute" ;;
            challenge|c) mode="challenge" ;;
            discover|d)  mode="discover" ;;
            generate|g)  mode="generate" ;;
            *)
                die "Invalid mode '${mode}'. Valid modes: execute (e), challenge (c), discover (d), generate (g)."
                ;;
        esac
    else
        mode="execute"
    fi

    require_config

    # Validate project exists if specified
    if [[ -n "${project}" ]]; then
        if [[ ! -f "${BOI_STATE_DIR}/projects/${project}/project.json" ]]; then
            die "Project '${project}' does not exist. Create it with 'boi project create ${project}'."
        fi
    fi

    if [[ -z "${spec_file}" ]] && [[ -z "${tasks_file}" ]]; then
        die "Provide --spec <file.md> or --tasks <file.md>. Use 'boi dispatch --help'."
    fi

    local input_file="${spec_file:-${tasks_file}}"

    if [[ ! -f "${input_file}" ]]; then
        die "File not found: ${input_file}"
    fi

    mkdir -p "${QUEUE_DIR}" "${EVENTS_DIR}" "${LOG_DIR}"

    # If --tasks was used, convert to spec format first
    if [[ -n "${tasks_file}" ]]; then
        local converted_spec
        converted_spec=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${tasks_file}" "${QUEUE_DIR}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.spec_parser import convert_tasks_to_spec
from pathlib import Path

tasks_file = sys.argv[1]
queue_dir = sys.argv[2]

# Generate a spec file next to the queue
base = Path(tasks_file).stem
output = os.path.join(queue_dir, f"{base}-converted.spec.md")
count = convert_tasks_to_spec(tasks_file, output)
print(output)
PYEOF
        )

        if [[ $? -ne 0 ]] || [[ -z "${converted_spec}" ]]; then
            die "Failed to convert tasks file to spec format."
        fi

        info "Converted tasks.md to spec format: ${converted_spec}"
        input_file="${converted_spec}"
    fi

    # Validate the spec before enqueueing
    local validation_output
    validation_output=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${input_file}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.spec_validator import auto_validate_file

filepath = sys.argv[1]
result = auto_validate_file(filepath)

if not result.valid:
    print("INVALID", file=sys.stderr)
    for error in result.errors:
        print(f"  ERROR: {error}", file=sys.stderr)
    sys.exit(1)

# Print warnings to stderr but don't fail
for warning in result.warnings:
    print(f"  WARN: {warning}", file=sys.stderr)

# Print summary to stdout
print(result.summary())
PYEOF
    )

    if [[ $? -ne 0 ]]; then
        echo -e "${RED}Spec validation failed:${NC}" >&2
        echo "${validation_output}" >&2
        die "Fix the spec and try again."
    fi

    info "Spec validated: ${validation_output}"

    # Dry-run mode: show what would happen and exit
    if [[ "${dry_run}" == "true" ]]; then
        info "[dry-run] Would dispatch spec: ${input_file}"
        info "[dry-run] Mode: ${mode}"
        info "[dry-run] Priority: ${priority}"
        info "[dry-run] Max iterations: ${max_iter}"
        if [[ -n "${worktree}" ]]; then
            info "[dry-run] Worktree: ${worktree}"
        fi
        if [[ -n "${timeout}" ]]; then
            info "[dry-run] Timeout: ${timeout}s"
        fi
        info "[dry-run] No critic: ${no_critic}"
        if [[ -n "${experiment_budget}" ]]; then
            info "[dry-run] Experiment budget: ${experiment_budget}"
        fi
        if [[ -n "${project}" ]]; then
            info "[dry-run] Project: ${project}"
        fi
        exit 0
    fi

    # Enqueue the spec
    local worktree_arg=""
    if [[ -n "${worktree}" ]]; then
        worktree_arg="${worktree}"
    fi

    local result
    result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${input_file}" "${QUEUE_DIR}" "${priority}" "${max_iter}" "${worktree_arg}" "${timeout}" "${no_critic}" "${mode}" "${project}" "${experiment_budget}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.queue import enqueue, DuplicateSpecError, get_experiment_budget
from lib.spec_parser import count_boi_tasks

spec_path = sys.argv[1]
queue_dir = sys.argv[2]
priority = int(sys.argv[3])
max_iter = int(sys.argv[4])
checkout = sys.argv[5] if len(sys.argv) > 5 and sys.argv[5] else None
timeout_str = sys.argv[6] if len(sys.argv) > 6 and sys.argv[6] else None
no_critic = sys.argv[7] == "true" if len(sys.argv) > 7 else False
mode = sys.argv[8] if len(sys.argv) > 8 and sys.argv[8] else "execute"
project_name = sys.argv[9] if len(sys.argv) > 9 and sys.argv[9] else None
experiment_budget_str = sys.argv[10] if len(sys.argv) > 10 and sys.argv[10] else None

# Count tasks in spec
counts = count_boi_tasks(spec_path)

try:
    entry = enqueue(
        queue_dir=queue_dir,
        spec_path=spec_path,
        priority=priority,
        max_iterations=max_iter,
        checkout=checkout,
        project=project_name,
    )
except DuplicateSpecError as e:
    print(json.dumps({"error": "duplicate", "message": str(e)}))
    sys.exit(2)

# Update task counts
entry["tasks_total"] = counts["total"]
entry["tasks_done"] = counts["done"]

# Set mode
entry["mode"] = mode

# Set experiment budget (user override or mode default)
if experiment_budget_str:
    entry["max_experiment_invocations"] = int(experiment_budget_str)
else:
    entry["max_experiment_invocations"] = get_experiment_budget(mode)
entry["experiment_invocations_used"] = 0

# Set phase based on spec type
from lib.spec_validator import is_generate_spec
from pathlib import Path as _Path
_spec_content = _Path(entry["spec_path"]).read_text(encoding="utf-8")
if is_generate_spec(_spec_content):
    entry["phase"] = "decompose"
else:
    entry["phase"] = "execute"

# Set per-spec timeout if provided
if timeout_str:
    entry["worker_timeout_seconds"] = int(timeout_str)

# Set no-critic flag if provided
if no_critic:
    entry["no_critic"] = True

# Re-write with updated counts
from lib.queue import _write_entry
_write_entry(queue_dir, entry)

print(json.dumps({"id": entry["id"], "tasks": counts["total"], "pending": counts["pending"], "mode": mode, "phase": entry.get("phase", "execute")}))
PYEOF
    )

    local enqueue_exit=$?

    if [[ ${enqueue_exit} -eq 2 ]]; then
        # Duplicate spec error
        local dup_msg
        dup_msg=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['message'])")
        die "${dup_msg}"
    fi

    if [[ ${enqueue_exit} -ne 0 ]]; then
        die "Failed to enqueue spec."
    fi

    local queue_id
    local task_count
    local pending_count
    local enqueued_mode
    queue_id=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['id'])")
    task_count=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['tasks'])")
    pending_count=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['pending'])")
    enqueued_mode=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('mode','execute'))")

    info "Spec queued: ${queue_id} (${pending_count}/${task_count} tasks pending, priority ${priority}, mode ${enqueued_mode})"

    # Start daemon if not already running
    if require_daemon; then
        info "Daemon already running. It will pick up the new spec."
    else
        info "Starting daemon..."
        nohup bash "${SCRIPT_DIR}/daemon.sh" > "${LOG_DIR}/daemon-startup.log" 2>&1 < /dev/null &
        sleep 1
        if require_daemon; then
            info "Daemon started."
        else
            warn "Daemon may not have started. Check ${LOG_DIR}/daemon-startup.log"
        fi
    fi

    echo ""
    info "Monitor progress with: boi status"
}

# ─── Subcommand: queue ──────────────────────────────────────────────────────

cmd_queue() {
    local json_mode=false
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --json) json_mode=true; shift ;;
            -h|--help)
                echo "Usage: boi queue [--json]"
                exit 0
                ;;
            *) die "Unknown option: $1" ;;
        esac
    done

    require_config

    cmd_queue_inner "${json_mode}"
}

# ─── Subcommand: status ──────────────────────────────────────────────────────

cmd_status() {
    local watch_mode=false
    local json_mode=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --watch) watch_mode=true; shift ;;
            --json) json_mode=true; shift ;;
            -h|--help)
                echo "Usage: boi status [--watch] [--json]"
                echo ""
                echo "Options:"
                echo "  --watch   Auto-refresh every 2s"
                echo "  --json    Output machine-readable JSON"
                exit 0
                ;;
            *) die "Unknown option: $1" ;;
        esac
    done

    require_config

    if [[ "${watch_mode}" == "true" ]]; then
        if [[ -f "${SCRIPT_DIR}/dashboard.sh" ]]; then
            exec bash "${SCRIPT_DIR}/dashboard.sh"
        else
            # Fallback: loop with clear + queue display
            while true; do
                clear
                cmd_queue_inner "${json_mode}"
                sleep 2
            done
        fi
    fi

    cmd_queue_inner "${json_mode}"

    # Check daemon heartbeat staleness
    local heartbeat_file="${BOI_STATE_DIR}/daemon-heartbeat"
    if [[ -f "${heartbeat_file}" ]]; then
        local heartbeat_ts
        heartbeat_ts=$(cat "${heartbeat_file}" 2>/dev/null || true)
        if [[ -n "${heartbeat_ts}" ]]; then
            local stale
            stale=$(python3 -c "
from datetime import datetime, timezone, timedelta
try:
    hb = datetime.fromisoformat('${heartbeat_ts}'.replace('Z', '+00:00'))
    now = datetime.now(timezone.utc)
    if (now - hb) > timedelta(seconds=30):
        print('stale')
    else:
        print('ok')
except Exception:
    print('unknown')
" 2>/dev/null || echo "unknown")
            if [[ "${stale}" == "stale" ]]; then
                echo ""
                warn "Daemon may be stuck. Heartbeat is stale (last: ${heartbeat_ts})."
            fi
        fi
    elif require_daemon 2>/dev/null; then
        # Daemon running but no heartbeat file (pre-heartbeat version)
        :
    fi
}

# Inner queue display used by both cmd_queue and cmd_status
cmd_queue_inner() {
    local json_mode="${1:-false}"

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${BOI_CONFIG}" "${json_mode}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.status import build_queue_status, format_queue_table, format_queue_json

queue_dir = sys.argv[1]
config_path = sys.argv[2]
json_mode = sys.argv[3] == "True" or sys.argv[3] == "true"

config = None
if os.path.isfile(config_path):
    try:
        with open(config_path) as f:
            config = json.load(f)
    except Exception:
        pass

status_data = build_queue_status(queue_dir, config)

if json_mode:
    print(format_queue_json(status_data))
else:
    print(format_queue_table(status_data))
PYEOF
}

# ─── Subcommand: log ─────────────────────────────────────────────────────────

cmd_log() {
    local queue_id=""
    local full_mode=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --full) full_mode=true; shift ;;
            -h|--help)
                echo "Usage: boi log <queue-id> [--full]"
                echo ""
                echo "Options:"
                echo "  --full    Show full output (default: tail last 50 lines)"
                exit 0
                ;;
            -*)
                die "Unknown option: $1"
                ;;
            *)
                if [[ -z "${queue_id}" ]]; then
                    queue_id="$1"
                else
                    die "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    require_config

    if [[ -z "${queue_id}" ]]; then
        die "Queue ID required. Usage: boi log <queue-id> [--full]"
    fi

    # Find the latest iteration log
    local latest_log=""
    local latest_iter=0

    for log_file in "${LOG_DIR}/${queue_id}"-iter-*.log; do
        if [[ -f "${log_file}" ]]; then
            local iter_num
            iter_num=$(echo "${log_file}" | sed -n "s/.*-iter-\([0-9]*\)\.log/\1/p")
            if [[ "${iter_num}" -gt "${latest_iter}" ]]; then
                latest_iter="${iter_num}"
                latest_log="${log_file}"
            fi
        fi
    done

    if [[ -z "${latest_log}" ]]; then
        # Check if spec exists in queue
        if [[ -f "${QUEUE_DIR}/${queue_id}.json" ]]; then
            die "No log found for '${queue_id}'. The spec exists but hasn't started running yet."
        else
            die "Unknown spec '${queue_id}'. Use 'boi queue' to see queued specs."
        fi
    fi

    echo -e "${DIM}Log: ${latest_log} (iteration ${latest_iter})${NC}"
    echo ""

    if [[ "${full_mode}" == "true" ]]; then
        cat "${latest_log}"
    else
        echo -e "${DIM}Showing last 50 lines. Use --full for complete output.${NC}"
        echo ""
        tail -n 50 "${latest_log}"
    fi
}

# ─── Subcommand: cancel ──────────────────────────────────────────────────────

cmd_cancel() {
    local queue_id=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help)
                echo "Usage: boi cancel <queue-id>"
                exit 0
                ;;
            -*)
                die "Unknown option: $1"
                ;;
            *)
                if [[ -z "${queue_id}" ]]; then
                    queue_id="$1"
                else
                    die "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    require_config

    if [[ -z "${queue_id}" ]]; then
        die "Queue ID required. Usage: boi cancel <queue-id>"
    fi

    # Cancel in queue
    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.queue import cancel

queue_dir = sys.argv[1]
queue_id = sys.argv[2]

try:
    cancel(queue_dir, queue_id)
    print(f"Spec '{queue_id}' canceled.")
except ValueError as e:
    print(f"Error: {e}", file=sys.stderr)
    sys.exit(1)
PYEOF

    # Kill any tmux session for this spec
    local tmux_session="boi-${queue_id}"
    if tmux -L boi has-session -t "${tmux_session}" 2>/dev/null; then
        info "Killing worker session: ${tmux_session}"
        tmux -L boi kill-session -t "${tmux_session}" 2>/dev/null
    fi

    info "Spec '${queue_id}' canceled."
}

# ─── Subcommand: review ─────────────────────────────────────────────────────

cmd_review() {
    local queue_id=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help)
                echo "Usage: boi review <queue-id>"
                echo ""
                echo "Review EXPERIMENT_PROPOSED tasks in a spec that is paused for review."
                echo ""
                echo "For each experiment, you can:"
                echo "  [a] Adopt   — Mark task DONE, tag experiment [ADOPTED]"
                echo "  [r] Reject  — Reset task to PENDING, tag experiment [REJECTED]"
                echo "  [d] Defer   — Keep spec paused for later review"
                echo "  [v] View    — Show full experiment details"
                exit 0
                ;;
            -*)
                die "Unknown option: $1"
                ;;
            *)
                if [[ -z "${queue_id}" ]]; then
                    queue_id="$1"
                else
                    die "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    require_config

    if [[ -z "${queue_id}" ]]; then
        die "Queue ID required. Usage: boi review <queue-id>"
    fi

    # Get experiments for review
    local experiments_json
    experiments_json=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" <<'PYEOF'
import json
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.review import get_experiments_for_review

queue_dir = sys.argv[1]
queue_id = sys.argv[2]

result = get_experiments_for_review(queue_dir, queue_id)
print(json.dumps(result))
PYEOF
)

    local valid
    valid=$(echo "${experiments_json}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('valid', False))")

    if [[ "${valid}" != "True" ]]; then
        local error
        error=$(echo "${experiments_json}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('error', 'Unknown error'))")
        die "${error}"
    fi

    # Parse experiments list
    local experiment_count
    experiment_count=$(echo "${experiments_json}" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('experiments', [])))")

    echo -e "${BOLD}BOI Experiment Review${NC} — ${queue_id}"
    echo -e "${DIM}${experiment_count} experiment(s) pending review${NC}"
    echo ""

    local deferred=false

    # Iterate over experiments
    local i=0
    while [[ ${i} -lt ${experiment_count} ]]; do
        local task_id title experiment_content
        task_id=$(echo "${experiments_json}" | python3 -c "import sys,json; print(json.load(sys.stdin)['experiments'][${i}]['task_id'])")
        title=$(echo "${experiments_json}" | python3 -c "import sys,json; print(json.load(sys.stdin)['experiments'][${i}]['title'])")
        experiment_content=$(echo "${experiments_json}" | python3 -c "import sys,json; print(json.load(sys.stdin)['experiments'][${i}].get('experiment_content', '(no experiment details)'))")

        echo -e "${BOLD}━━━ ${task_id}: ${title} ━━━${NC}"
        echo ""

        # Show experiment summary (first 10 lines)
        if [[ -n "${experiment_content}" && "${experiment_content}" != "(no experiment details)" ]]; then
            echo -e "${DIM}Experiment summary:${NC}"
            echo "${experiment_content}" | head -n 10
            local line_count
            line_count=$(echo "${experiment_content}" | wc -l)
            if [[ ${line_count} -gt 10 ]]; then
                echo -e "${DIM}  ... (${line_count} lines total, use [v] to view all)${NC}"
            fi
        else
            echo -e "${DIM}(No experiment details found in spec)${NC}"
        fi
        echo ""

        # Prompt for action
        local action=""
        while [[ -z "${action}" ]]; do
            echo -en "  ${YELLOW}[a]${NC}dopt  ${YELLOW}[r]${NC}eject  ${YELLOW}[d]${NC}efer  ${YELLOW}[v]${NC}iew  > "
            read -r action

            case "${action}" in
                a|adopt)
                    # Try to merge experiment bookmark if it exists
                    local bookmark_name="experiment-${queue_id}-${task_id}"
                    if command -v git >/dev/null 2>&1; then
                        if git branch --list "${bookmark_name}" 2>/dev/null | grep -q "${bookmark_name}"; then
                            info "Merging experiment branch: ${bookmark_name}"
                            git merge "${bookmark_name}" 2>/dev/null || warn "Could not merge experiment branch (may need manual merge)"
                            # Delete branch after merge
                            git branch -d "${bookmark_name}" 2>/dev/null || true
                        fi
                    fi

                    # Adopt in spec
                    local adopt_result
                    adopt_result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" "${task_id}" "${EVENTS_DIR}" <<'PYEOF'
import json, os, sys
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.review import adopt_experiment
result = adopt_experiment(sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4])
print(json.dumps(result))
PYEOF
)
                    local adopt_ok
                    adopt_ok=$(echo "${adopt_result}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('success', False))")
                    if [[ "${adopt_ok}" == "True" ]]; then
                        echo -e "  ${GREEN}✓${NC} Adopted ${task_id}"
                    else
                        local adopt_err
                        adopt_err=$(echo "${adopt_result}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('error', 'unknown'))")
                        warn "Failed to adopt: ${adopt_err}"
                    fi
                    ;;

                r|reject)
                    echo -n "  Reason (optional, press Enter to skip): "
                    local reason=""
                    read -r reason

                    local reject_result
                    reject_result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" "${task_id}" "${EVENTS_DIR}" "${reason}" <<'PYEOF'
import json, os, sys
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.review import reject_experiment
result = reject_experiment(sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5] if len(sys.argv) > 5 else "")
print(json.dumps(result))
PYEOF
)
                    local reject_ok
                    reject_ok=$(echo "${reject_result}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('success', False))")
                    if [[ "${reject_ok}" == "True" ]]; then
                        # Delete experiment bookmark if it exists
                        local bookmark_name="experiment-${queue_id}-${task_id}"
                        if command -v git >/dev/null 2>&1; then
                            git branch -d "${bookmark_name}" 2>/dev/null || true
                        fi
                        echo -e "  ${RED}✗${NC} Rejected ${task_id}"
                    else
                        local reject_err
                        reject_err=$(echo "${reject_result}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('error', 'unknown'))")
                        warn "Failed to reject: ${reject_err}"
                    fi
                    ;;

                d|defer)
                    echo -e "  ${YELLOW}—${NC} Deferred ${task_id}"
                    deferred=true
                    ;;

                v|view)
                    echo ""
                    echo -e "${BOLD}Full experiment details:${NC}"
                    echo "${experiment_content}"
                    echo ""
                    # Re-prompt
                    action=""
                    continue
                    ;;

                *)
                    echo -e "  ${RED}Invalid choice.${NC} Use [a]dopt, [r]eject, [d]efer, or [v]iew."
                    action=""
                    continue
                    ;;
            esac
        done

        echo ""
        i=$((i + 1))
    done

    # Finalize review
    if [[ "${deferred}" == "true" ]]; then
        info "Review paused. Some experiments deferred. Run 'boi review ${queue_id}' to continue."
    else
        local finalize_result
        finalize_result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" "${EVENTS_DIR}" <<'PYEOF'
import json, os, sys
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.review import finalize_review
result = finalize_review(sys.argv[1], sys.argv[2], sys.argv[3])
print(json.dumps(result))
PYEOF
)
        local requeued
        requeued=$(echo "${finalize_result}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('requeued', False))")

        if [[ "${requeued}" == "True" ]]; then
            info "All experiments reviewed. Spec '${queue_id}' requeued for processing."
        else
            local remaining
            remaining=$(echo "${finalize_result}" | python3 -c "import sys,json; print(json.load(sys.stdin).get('remaining_experiments', 0))")
            info "${remaining} experiment(s) still pending. Run 'boi review ${queue_id}' to continue."
        fi
    fi
}

# ─── Subcommand: purge ───────────────────────────────────────────────────────

cmd_purge() {
    local all_mode=false
    local dry_run=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --all) all_mode=true; shift ;;
            --dry-run) dry_run=true; shift ;;
            -h|--help)
                echo "Usage: boi purge [--all] [--dry-run]"
                echo ""
                echo "Remove completed, failed, and canceled specs from the queue."
                echo ""
                echo "Options:"
                echo "  --all       Remove ALL specs (including queued/running)"
                echo "  --dry-run   Show what would be removed without deleting"
                exit 0
                ;;
            *)
                die "Unknown option: $1. Use 'boi purge --help' for usage."
                ;;
        esac
    done

    require_config

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${LOG_DIR}" "${all_mode}" "${dry_run}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.queue import purge

queue_dir = sys.argv[1]
log_dir = sys.argv[2]
all_mode = sys.argv[3] == "true"
dry_run = sys.argv[4] == "true"

if all_mode:
    statuses = ["queued", "running", "requeued", "completed", "failed", "canceled"]
else:
    statuses = ["completed", "failed", "canceled"]

results = purge(queue_dir, log_dir, statuses=statuses, dry_run=dry_run)

if not results:
    print("Nothing to purge.")
    sys.exit(0)

prefix = "[dry-run] Would remove" if dry_run else "Purged"
for r in results:
    file_count = len(r["files_removed"])
    print(f"{prefix}: {r['id']} ({r['status']}) — {file_count} file(s)")

if not dry_run:
    total = sum(len(r["files_removed"]) for r in results)
    print(f"\nRemoved {len(results)} spec(s), {total} file(s) total.")
PYEOF
}

# ─── Subcommand: stop ────────────────────────────────────────────────────────

cmd_stop() {
    require_config

    # Kill all worker tmux sessions
    local killed=0
    local sessions
    sessions=$(tmux -L boi list-sessions -F '#{session_name}' 2>/dev/null || true)
    if [[ -n "${sessions}" ]]; then
        while IFS= read -r session; do
            if [[ "${session}" == boi-* ]]; then
                info "Killing worker session: ${session}"
                tmux -L boi kill-session -t "${session}" 2>/dev/null
                killed=$((killed + 1))
            fi
        done <<< "${sessions}"
    fi

    if [[ ${killed} -gt 0 ]]; then
        info "Killed ${killed} worker session(s)."
    else
        info "No active worker sessions found."
    fi

    # Stop the daemon
    if [[ -f "${PID_FILE}" ]]; then
        local pid
        pid=$(cat "${PID_FILE}")
        if kill -0 "${pid}" 2>/dev/null; then
            info "Stopping daemon (PID ${pid})..."
            kill "${pid}" 2>/dev/null
            local waited=0
            while kill -0 "${pid}" 2>/dev/null && [[ ${waited} -lt 10 ]]; do
                sleep 1
                waited=$((waited + 1))
            done
            if kill -0 "${pid}" 2>/dev/null; then
                warn "Daemon did not stop gracefully. Sending SIGKILL."
                kill -9 "${pid}" 2>/dev/null
            fi
            info "Daemon stopped."
        else
            info "Daemon not running (stale PID file). Cleaning up."
        fi
        rm -f "${PID_FILE}"
    else
        info "Daemon not running."
    fi
}

# ─── Subcommand: workers ─────────────────────────────────────────────────────

cmd_workers() {
    local json_mode=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --json) json_mode=true; shift ;;
            -h|--help)
                echo "Usage: boi workers [--json]"
                exit 0
                ;;
            *) die "Unknown option: $1" ;;
        esac
    done

    require_config

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${BOI_CONFIG}" "${json_mode}" <<'PYEOF'
import json, sys, os, subprocess

config_path = sys.argv[1]
json_mode = sys.argv[2] == "True"

with open(config_path) as f:
    config = json.load(f)

workers = config.get("workers", [])
if not workers:
    if json_mode:
        print("[]")
    else:
        print("No workers configured. Run 'boi install' to set up worktrees.")
    sys.exit(0)

# Get active tmux sessions
try:
    result = subprocess.run(
        ["tmux", "-L", "boi", "list-sessions", "-F", "#{session_name}"],
        capture_output=True, text=True, timeout=5
    )
    active_sessions = set(result.stdout.strip().split("\n")) if result.stdout.strip() else set()
except Exception:
    active_sessions = set()

GREEN = "\033[0;32m"
YELLOW = "\033[1;33m"
RED = "\033[0;31m"
NC = "\033[0m"

if not json_mode:
    print(f"{'WORKER':<10}  {'WORKTREE':<50}  {'HEALTH'}")
    print(f"{'------':<10}  {'--------':<50}  {'------'}")

healthy_count = 0
json_output = []

for w in workers:
    wid = w.get("id", "?")
    path = w.get("worktree_path", w.get("checkout_path", "?"))
    worktree_exists = os.path.isdir(path)

    worker_json = {
        "id": wid,
        "worktree_path": path,
        "status": "missing",
    }

    if not worktree_exists:
        status_str = f"{RED}missing{NC}"
        worker_json["status"] = "missing"
    else:
        has_valid_worktree = os.path.isdir(path)
        if not has_valid_worktree:
            status_str = f"{YELLOW}unhealthy{NC}"
            worker_json["status"] = "unhealthy"
        else:
            # Check for active tmux session
            busy = any(s == f"boi-{wid}" or s.startswith(f"boi-q-") for s in active_sessions)
            if busy:
                status_str = f"{YELLOW}busy{NC}"
                worker_json["status"] = "busy"
            else:
                status_str = f"{GREEN}idle{NC}"
                worker_json["status"] = "idle"
            healthy_count += 1

    json_output.append(worker_json)

    if not json_mode:
        print(f"{wid:<10}  {path:<50}  {status_str}")

if json_mode:
    print(json.dumps(json_output, indent=2))
else:
    print()
    if healthy_count == len(workers):
        print(f"{GREEN}All {len(workers)} worker(s) healthy.{NC}")
    elif healthy_count > 0:
        print(f"{YELLOW}{healthy_count}/{len(workers)} worker(s) healthy.{NC}")
    else:
        print(f"{RED}No healthy workers. Run 'boi install' to fix.{NC}")
PYEOF
}

# ─── Subcommand: telemetry ───────────────────────────────────────────────────

cmd_telemetry() {
    local queue_id=""
    local json_mode=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --json) json_mode=true; shift ;;
            -h|--help)
                echo "Usage: boi telemetry <queue-id> [--json]"
                exit 0
                ;;
            -*)
                die "Unknown option: $1"
                ;;
            *)
                if [[ -z "${queue_id}" ]]; then
                    queue_id="$1"
                else
                    die "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    if [[ -z "${queue_id}" ]]; then
        die "Queue ID required. Usage: boi telemetry <queue-id>"
    fi

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" "${json_mode}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.status import build_telemetry, format_telemetry_table, format_telemetry_json

queue_dir = sys.argv[1]
queue_id = sys.argv[2]
json_mode = sys.argv[3] == "True" or sys.argv[3] == "true"

telemetry = build_telemetry(queue_dir, queue_id)
if telemetry is None:
    print(f"Error: Unknown spec '{queue_id}'. Use 'boi queue' to see queued specs.", file=sys.stderr)
    sys.exit(1)

if json_mode:
    print(format_telemetry_json(telemetry))
else:
    print(format_telemetry_table(telemetry))
PYEOF
}

# ─── Subcommand: dashboard ───────────────────────────────────────────────────

cmd_dashboard() {
    if [[ -f "${SCRIPT_DIR}/dashboard.sh" ]]; then
        exec bash "${SCRIPT_DIR}/dashboard.sh"
    else
        die "Dashboard not yet implemented."
    fi
}

# ─── Subcommand: doctor ──────────────────────────────────────────────────────

cmd_doctor() {
    echo -e "${BOLD}BOI Doctor${NC}"
    echo ""

    local pass_count=0
    local fail_count=0
    local warn_count=0

    _doctor_pass() {
        echo -e "  ${GREEN}[PASS]${NC} $1"
        pass_count=$((pass_count + 1))
    }

    _doctor_fail() {
        echo -e "  ${RED}[FAIL]${NC} $1"
        if [[ -n "${2:-}" ]]; then
            echo -e "         Fix: $2"
        fi
        fail_count=$((fail_count + 1))
    }

    _doctor_warn() {
        echo -e "  ${YELLOW}[WARN]${NC} $1"
        if [[ -n "${2:-}" ]]; then
            echo -e "         Fix: $2"
        fi
        warn_count=$((warn_count + 1))
    }

    # 1. tmux installed
    local tmux_ver
    if tmux_ver=$(tmux -V 2>/dev/null); then
        _doctor_pass "tmux installed (${tmux_ver})"
    else
        _doctor_fail "tmux not installed" "Install tmux: sudo apt install tmux"
    fi

    # 2. claude CLI installed
    if command -v claude >/dev/null 2>&1; then
        _doctor_pass "claude CLI installed"
    else
        _doctor_fail "claude CLI not installed" "Install claude: see https://docs.anthropic.com/claude-code"
    fi

    # 3. Python 3.10+
    local py_ver
    if py_ver=$(python3 --version 2>/dev/null); then
        local py_minor
        py_minor=$(python3 -c "import sys; print(sys.version_info.minor)" 2>/dev/null || echo "0")
        local py_major
        py_major=$(python3 -c "import sys; print(sys.version_info.major)" 2>/dev/null || echo "0")
        if [[ "${py_major}" -ge 3 ]] && [[ "${py_minor}" -ge 10 ]]; then
            _doctor_pass "${py_ver} (>= 3.10 required)"
        else
            _doctor_fail "${py_ver} (>= 3.10 required)" "Upgrade Python to 3.10+"
        fi
    else
        _doctor_fail "python3 not found" "Install Python 3.10+"
    fi

    # 4. State directory exists
    if [[ -d "${BOI_STATE_DIR}" ]]; then
        _doctor_pass "State directory exists (~/.boi/)"
    else
        _doctor_fail "State directory missing (~/.boi/)" "Run 'boi install' to set up BOI"
    fi

    # 5. Config exists and is valid JSON
    if [[ -f "${BOI_CONFIG}" ]]; then
        if python3 -c "import json; json.load(open('${BOI_CONFIG}'))" 2>/dev/null; then
            # 6. Workers configured
            local worker_count
            worker_count=$(python3 -c "
import json
with open('${BOI_CONFIG}') as f:
    c = json.load(f)
print(len(c.get('workers', [])))
" 2>/dev/null || echo "0")
            if [[ "${worker_count}" -gt 0 ]]; then
                _doctor_pass "Config valid (${worker_count} workers)"
            else
                _doctor_fail "Config valid but no workers configured" "Run 'boi install --workers N' to add workers"
            fi

            # 7 & 8. Check each worker worktree
            local worker_results
            worker_results=$(python3 -c "
import json, os
with open('${BOI_CONFIG}') as f:
    c = json.load(f)
for w in c.get('workers', []):
    wid = w.get('id', '?')
    path = w.get('worktree_path', w.get('checkout_path', '?'))
    if not os.path.isdir(path):
        print(f'FAIL|{wid}|{path}|directory not found')
    else:
        print(f'PASS|{wid}|{path}|healthy')
" 2>/dev/null || true)
            if [[ -n "${worker_results}" ]]; then
                while IFS='|' read -r status wid path detail; do
                    case "${status}" in
                        PASS) _doctor_pass "Worker ${wid}: ${path} (${detail})" ;;
                        FAIL) _doctor_fail "Worker ${wid}: ${path} (${detail})" "Run 'boi install' to recreate worktrees" ;;
                        WARN) _doctor_warn "Worker ${wid}: ${path} (${detail})" "Check the worktree directory" ;;
                    esac
                done <<< "${worker_results}"
            fi
        else
            _doctor_fail "Config file is invalid JSON" "Delete ~/.boi/config.json and run 'boi install'"
        fi
    else
        _doctor_fail "Config file missing (~/.boi/config.json)" "Run 'boi install' to create config"
    fi

    # 9. Daemon PID file check
    if [[ -f "${PID_FILE}" ]]; then
        local daemon_pid
        daemon_pid=$(cat "${PID_FILE}" 2>/dev/null || true)
        if [[ -n "${daemon_pid}" ]] && kill -0 "${daemon_pid}" 2>/dev/null; then
            _doctor_pass "Daemon running (PID ${daemon_pid})"
        else
            _doctor_warn "Daemon not running (stale PID file)" "Run 'boi dispatch' to start the daemon, or 'boi start-daemon'"
        fi
    else
        _doctor_warn "Daemon not running" "Run 'boi dispatch' to start the daemon, or 'boi start-daemon'"
    fi

    # 10. Daemon heartbeat check
    local heartbeat_file="${BOI_STATE_DIR}/daemon-heartbeat"
    if [[ -f "${heartbeat_file}" ]]; then
        local heartbeat_ts
        heartbeat_ts=$(cat "${heartbeat_file}" 2>/dev/null || true)
        if [[ -n "${heartbeat_ts}" ]]; then
            local heartbeat_status
            heartbeat_status=$(python3 -c "
from datetime import datetime, timezone, timedelta
try:
    hb = datetime.fromisoformat('${heartbeat_ts}'.replace('Z', '+00:00'))
    now = datetime.now(timezone.utc)
    if (now - hb) > timedelta(seconds=30):
        print('stale')
    else:
        print('ok')
except Exception:
    print('unknown')
" 2>/dev/null || echo "unknown")
            case "${heartbeat_status}" in
                ok) _doctor_pass "Daemon heartbeat recent (${heartbeat_ts})" ;;
                stale) _doctor_warn "Daemon heartbeat stale (last: ${heartbeat_ts})" "Restart daemon: 'boi stop && boi dispatch --spec <spec>'" ;;
                *) _doctor_warn "Daemon heartbeat unreadable" "Check ~/.boi/daemon-heartbeat" ;;
            esac
        fi
    elif require_daemon 2>/dev/null; then
        # Daemon running but no heartbeat (pre-heartbeat version)
        _doctor_warn "No heartbeat file (daemon may be pre-heartbeat version)" ""
    fi

    # 11. Critic configuration
    local critic_config="${BOI_STATE_DIR}/critic/config.json"
    if [[ -f "${critic_config}" ]]; then
        local critic_status
        critic_status=$(python3 -c "
import json, os, sys
try:
    with open('${critic_config}') as f:
        cfg = json.load(f)
    enabled = cfg.get('enabled', True)
    max_passes = cfg.get('max_passes', 2)
    checks = cfg.get('checks', [])
    custom_dir = os.path.join('${BOI_STATE_DIR}', 'critic', cfg.get('custom_checks_dir', 'custom'))
    custom_count = 0
    if os.path.isdir(custom_dir):
        custom_count = len([f for f in os.listdir(custom_dir) if f.endswith('.md')])
    prompt_override = os.path.isfile(os.path.join('${BOI_STATE_DIR}', 'critic', 'prompt.md'))
    parts = []
    parts.append('enabled' if enabled else 'disabled')
    parts.append(f'max_passes={max_passes}')
    parts.append(f'{len(checks)} default checks')
    if custom_count > 0:
        parts.append(f'{custom_count} custom checks')
    if prompt_override:
        parts.append('custom prompt')
    print('PASS|' + ', '.join(parts))
except Exception as e:
    print(f'WARN|invalid config: {e}')
" 2>/dev/null || echo "WARN|could not read config")
        local critic_level="${critic_status%%|*}"
        local critic_detail="${critic_status#*|}"
        case "${critic_level}" in
            PASS) _doctor_pass "Critic: ${critic_detail}" ;;
            *) _doctor_warn "Critic: ${critic_detail}" "Check ~/.boi/critic/config.json" ;;
        esac
    else
        _doctor_warn "Critic not configured" "Run 'boi install' or create ~/.boi/critic/config.json"
    fi

    echo ""
    echo -e "Results: ${GREEN}${pass_count} passed${NC}, ${RED}${fail_count} failed${NC}, ${YELLOW}${warn_count} warning(s)${NC}"
}

# ─── Subcommand: critic ──────────────────────────────────────────────────────

cmd_critic() {
    if [[ $# -eq 0 ]]; then
        _critic_usage
        exit 0
    fi

    local subcommand="$1"
    shift

    case "${subcommand}" in
        status)    _critic_status "$@" ;;
        run)       _critic_run "$@" ;;
        disable)   _critic_disable "$@" ;;
        enable)    _critic_enable "$@" ;;
        checks)    _critic_checks "$@" ;;
        benchmark) _critic_benchmark "$@" ;;
        -h|--help|help) _critic_usage; exit 0 ;;
        *)
            die "Unknown critic subcommand: ${subcommand}. Use 'boi critic --help' for usage."
            ;;
    esac
}

_critic_usage() {
    echo -e "${BOLD}BOI Critic${NC} — Work quality validation"
    echo ""
    echo "Usage: boi critic <subcommand>"
    echo ""
    echo "Subcommands:"
    echo "  status           Show critic config, active checks, pass counts"
    echo "  run <queue-id>   Manually trigger critic on a spec"
    echo "  disable          Disable the critic"
    echo "  enable           Enable the critic"
    echo "  checks           List all active checks (default + custom)"
    echo "  benchmark        Run performance benchmark against test specs"
    echo ""
    echo "Examples:"
    echo "  boi critic status       # show current configuration"
    echo "  boi critic run q-001    # trigger critic on a specific spec"
    echo "  boi critic disable      # turn off critic"
    echo "  boi critic enable       # turn on critic"
    echo "  boi critic checks       # list active checks"
    echo "  boi critic benchmark    # run performance benchmark"
}

_critic_status() {
    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${BOI_STATE_DIR}" "${QUEUE_DIR}" "${SCRIPT_DIR}" <<'PYEOF'
import json
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.critic_config import load_critic_config, get_active_checks

state_dir = sys.argv[1]
queue_dir = sys.argv[2]
boi_dir = sys.argv[3]

config = load_critic_config(state_dir)

enabled = config.get("enabled", True)
trigger = config.get("trigger", "on_complete")
max_passes = config.get("max_passes", 2)
checks_dir = os.path.join(boi_dir, "templates", "checks")

checks = get_active_checks(config, checks_dir, state_dir)
default_count = sum(1 for c in checks if c["source"] == "default")
custom_count = sum(1 for c in checks if c["source"] == "custom")

# Check for custom prompt override
prompt_override = os.path.isfile(os.path.join(state_dir, "critic", "prompt.md"))

print("BOI Critic")
print()
print(f"  Enabled: {'yes' if enabled else 'no'}")
print(f"  Trigger: {trigger}")
print(f"  Max passes: {max_passes}")

if custom_count > 0:
    print(f"  Active checks: {len(checks)} ({default_count} default + {custom_count} custom)")
else:
    print(f"  Active checks: {len(checks)}")

for check in checks:
    label = "default" if check["source"] == "default" else "custom"
    print(f"    [{label}] {check['name']}")

if prompt_override:
    print()
    print("  Prompt: custom override (~/.boi/critic/prompt.md)")

# Show critic pass counts for active specs
entries = []
if os.path.isdir(queue_dir):
    for fname in sorted(os.listdir(queue_dir)):
        if not fname.endswith(".json"):
            continue
        fpath = os.path.join(queue_dir, fname)
        try:
            with open(fpath) as f:
                entry = json.load(f)
            passes = entry.get("critic_passes", 0)
            if passes > 0:
                entries.append((entry.get("id", "?"), entry.get("status", "?"), passes))
        except (json.JSONDecodeError, OSError):
            continue

if entries:
    print()
    print("  Critic passes:")
    for qid, status, passes in entries:
        print(f"    {qid} ({status}): {passes} pass(es)")
PYEOF
}

_critic_run() {
    local queue_id=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help)
                echo "Usage: boi critic run <queue-id>"
                echo ""
                echo "Manually trigger the critic on a spec, even if it hasn't completed all tasks."
                exit 0
                ;;
            -*)
                die "Unknown option: $1"
                ;;
            *)
                if [[ -z "${queue_id}" ]]; then
                    queue_id="$1"
                else
                    die "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    if [[ -z "${queue_id}" ]]; then
        die "Queue ID required. Usage: boi critic run <queue-id>"
    fi

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${BOI_STATE_DIR}" "${QUEUE_DIR}" "${SCRIPT_DIR}" "${queue_id}" <<'PYEOF'
import json
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.critic_config import load_critic_config
from lib.critic import run_critic
from lib.queue import get_entry

state_dir = sys.argv[1]
queue_dir = sys.argv[2]
boi_dir = sys.argv[3]
queue_id = sys.argv[4]

entry = get_entry(queue_dir, queue_id)
if entry is None:
    print(f"Error: Unknown spec '{queue_id}'. Use 'boi queue' to see queued specs.", file=sys.stderr)
    sys.exit(1)

spec_path = entry.get("spec_path", "")
if not spec_path or not os.path.isfile(spec_path):
    print(f"Error: Spec file not found for '{queue_id}': {spec_path}", file=sys.stderr)
    sys.exit(1)

config = load_critic_config(state_dir)

result = run_critic(
    spec_path=spec_path,
    queue_dir=queue_dir,
    queue_id=queue_id,
    config=config,
)

print(f"Critic prompt generated: {result['prompt_path']}")
print(f"To review the prompt: cat {result['prompt_path']}")
print()
print("The critic prompt has been written. The daemon will pick it up on the next cycle,")
print("or you can run it manually with:")
print(f"  claude -p {result['prompt_path']}")
PYEOF
}

_critic_disable() {
    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${BOI_STATE_DIR}" <<'PYEOF'
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.critic_config import load_critic_config, save_critic_config

state_dir = sys.argv[1]
config = load_critic_config(state_dir)
config["enabled"] = False
save_critic_config(state_dir, config)
print("Critic disabled.")
PYEOF
}

_critic_enable() {
    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${BOI_STATE_DIR}" <<'PYEOF'
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.critic_config import load_critic_config, save_critic_config

state_dir = sys.argv[1]
config = load_critic_config(state_dir)
config["enabled"] = True
save_critic_config(state_dir, config)
print("Critic enabled.")
PYEOF
}

_critic_checks() {
    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${BOI_STATE_DIR}" "${SCRIPT_DIR}" <<'PYEOF'
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.critic_config import load_critic_config, get_active_checks

state_dir = sys.argv[1]
boi_dir = sys.argv[2]

config = load_critic_config(state_dir)
checks_dir = os.path.join(boi_dir, "templates", "checks")
checks = get_active_checks(config, checks_dir, state_dir)

if not checks:
    print("No active checks found.")
    sys.exit(0)

print(f"Active checks ({len(checks)}):")
print()

for check in checks:
    label = "default" if check["source"] == "default" else "custom"
    print(f"  [{label}] {check['name']}")

    # Extract first non-empty, non-heading line as description
    lines = check["content"].strip().split("\n")
    for line in lines:
        stripped = line.strip()
        if stripped and not stripped.startswith("#"):
            # Truncate long descriptions
            if len(stripped) > 80:
                stripped = stripped[:77] + "..."
            print(f"           {stripped}")
            break
    print()
PYEOF
}

_critic_benchmark() {
    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${BOI_STATE_DIR}" "${SCRIPT_DIR}" <<'PYEOF'
import json
import os
import sys
import tempfile
import time

boi_dir = os.environ["BOI_SCRIPT_DIR"]
sys.path.insert(0, boi_dir)

from lib.critic import run_critic
from lib.critic_config import DEFAULT_CONFIG, get_active_checks, load_critic_config

state_dir = sys.argv[1]
fixtures_dir = os.path.join(boi_dir, "tests", "fixtures", "critic-eval")
checks_dir = os.path.join(boi_dir, "templates", "checks")

spec_names = [
    "malformed-tasks",
    "fake-verify",
    "unbounded-growth",
    "silent-failures",
    "incomplete-spec",
    "perfect-spec",
]

# Count active checks
config = load_critic_config(state_dir)
checks = get_active_checks(config, checks_dir, state_dir)
num_checks = len(checks)

print("BOI Critic Benchmark")
print()

timings = []
total_no_critic = 0.0

for spec_name in spec_names:
    spec_path = os.path.join(fixtures_dir, f"{spec_name}.md")
    if not os.path.isfile(spec_path):
        print(f"  {spec_name + ':':24s}MISSING (fixture not found)")
        continue

    # Measure baseline (just reading the spec, no critic)
    baseline_start = time.monotonic()
    with open(spec_path, "r") as f:
        _ = f.read()
    baseline_elapsed = time.monotonic() - baseline_start
    total_no_critic += baseline_elapsed

    # Measure run_critic (prompt generation + check loading + file write)
    with tempfile.TemporaryDirectory() as tmpdir:
        queue_dir = os.path.join(tmpdir, "queue")
        os.makedirs(queue_dir)
        # Create mock queue entry
        entry = {
            "id": "q-bench",
            "spec_path": spec_path,
            "status": "completed",
            "critic_passes": 0,
        }
        with open(os.path.join(queue_dir, "q-bench.json"), "w") as f:
            json.dump(entry, f)

        # Create critic dirs in temp state
        critic_dir = os.path.join(tmpdir, "critic")
        os.makedirs(os.path.join(critic_dir, "custom"), exist_ok=True)

        start = time.monotonic()
        result = run_critic(
            spec_path=spec_path,
            queue_dir=queue_dir,
            queue_id="q-bench",
            config=DEFAULT_CONFIG,
        )
        elapsed = time.monotonic() - start

    timings.append((spec_name, elapsed, num_checks))
    print(f"  {spec_name + ':':24s}{elapsed:.3f}s ({num_checks} checks)")

print()

if timings:
    avg = sum(t[1] for t in timings) / len(timings)
    total_critic = sum(t[1] for t in timings)
    overhead_pct = (
        ((total_critic - total_no_critic) / total_no_critic * 100)
        if total_no_critic > 0
        else 0
    )
    print(f"  Average: {avg:.3f}s per spec")
    print(f"  Overhead vs no-critic: ~{overhead_pct:.0f}%")
else:
    print("  No fixtures found. Run t-1 first.")
PYEOF
}

# ─── Subcommand: spec ────────────────────────────────────────────────────────

_spec_usage() {
    echo -e "${BOLD}BOI Spec${NC} — Live spec management"
    echo ""
    echo "Usage: boi spec <queue-id> [subcommand] [options]"
    echo ""
    echo "Subcommands:"
    echo "  (none)                          Show tasks with status"
    echo "  add \"Title\" [--spec \"...\"] [--verify \"...\"]"
    echo "                                  Add a new PENDING task"
    echo "  skip <task-id> [--reason \"...\"]  Mark a task as SKIPPED"
    echo "  next <task-id>                  Reorder task to be next"
    echo "  block <task-id> --on <dep-id>   Block task on a dependency"
    echo "  edit [<task-id>]                Edit spec or task in \$EDITOR"
    echo "  --json                          Output tasks as JSON"
    echo ""
    echo "Examples:"
    echo "  boi spec q-001                  # show tasks"
    echo "  boi spec q-001 --json           # tasks as JSON"
    echo "  boi spec q-001 add \"Fix tests\" --spec \"Run and fix failing tests\""
    echo "  boi spec q-001 skip t-5 --reason \"not needed\""
    echo "  boi spec q-001 next t-7         # make t-7 the next task"
    echo "  boi spec q-001 block t-4 --on t-2"
    echo "  boi spec q-001 edit             # edit full spec"
    echo "  boi spec q-001 edit t-3         # edit task t-3"
}

cmd_spec() {
    local queue_id=""
    local json_mode=false

    if [[ $# -eq 0 ]] || [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]] || [[ "$1" == "help" ]]; then
        _spec_usage
        exit 0
    fi

    # First positional arg is the queue ID
    queue_id="$1"
    shift

    # Resolve spec file path
    local spec_file="${QUEUE_DIR}/${queue_id}.spec.md"
    if [[ ! -f "${spec_file}" ]]; then
        die "Spec file not found: ${spec_file}. Is '${queue_id}' a valid queue ID?"
    fi

    # No more args: show mode (default)
    if [[ $# -eq 0 ]]; then
        _spec_show "${spec_file}" false
        return
    fi

    # Dispatch subcommand or flag
    local subcommand="$1"
    shift

    case "${subcommand}" in
        --json)
            _spec_show "${spec_file}" true
            ;;
        add)
            _spec_add "${spec_file}" "$@"
            ;;
        skip)
            _spec_skip "${spec_file}" "$@"
            ;;
        next)
            _spec_next "${spec_file}" "$@"
            ;;
        block)
            _spec_block "${spec_file}" "$@"
            ;;
        edit)
            _spec_edit "${spec_file}" "$@"
            ;;
        -h|--help|help)
            _spec_usage
            exit 0
            ;;
        *)
            die "Unknown spec subcommand: ${subcommand}. Use 'boi spec --help' for usage."
            ;;
    esac
}

_spec_show() {
    local spec_file="$1"
    local json_mode="$2"

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${spec_file}" "${json_mode}" <<'PYEOF'
import json
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.spec_parser import parse_boi_spec

spec_file = sys.argv[1]
json_mode = sys.argv[2] == "true" or sys.argv[2] == "True"

with open(spec_file, encoding="utf-8") as f:
    content = f.read()

tasks = parse_boi_spec(content)

if json_mode:
    result = []
    for t in tasks:
        # Extract blocked_by from body (always include, default empty list)
        blocked_by = []
        for line in t.body.splitlines():
            if line.strip().startswith("**Blocked by:**"):
                deps = line.strip().split("**Blocked by:**")[1].strip()
                blocked_by = [d.strip() for d in deps.split(",") if d.strip()]
                break
        entry = {
            "id": t.id,
            "title": t.title,
            "status": t.status,
            "body": t.body,
            "blocked_by": blocked_by,
        }
        result.append(entry)
    print(json.dumps(result, indent=2))
else:
    # Status symbols
    symbols = {"DONE": "\033[0;32m✓\033[0m", "PENDING": "\033[1;33m○\033[0m", "SKIPPED": "\033[2m—\033[0m"}
    next_pending_found = False
    for t in tasks:
        sym = symbols.get(t.status, "?")
        marker = ""
        if t.status == "PENDING" and not next_pending_found:
            marker = " \033[1;33m← next\033[0m"
            next_pending_found = True
        # Check for blocked-by
        blocked = ""
        for line in t.body.splitlines():
            if line.strip().startswith("**Blocked by:**"):
                deps = line.strip().split("**Blocked by:**")[1].strip()
                blocked = f" \033[2m[blocked by: {deps}]\033[0m"
                break
        print(f"  {sym} {t.id}: {t.title}{blocked}{marker}")
PYEOF
}

_spec_add() {
    local spec_file="$1"
    shift
    local title=""
    local spec_text=""
    local verify_text=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --spec)
                spec_text="$2"
                shift 2
                ;;
            --verify)
                verify_text="$2"
                shift 2
                ;;
            -*)
                die "Unknown option for 'add': $1"
                ;;
            *)
                if [[ -z "${title}" ]]; then
                    title="$1"
                else
                    die "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    if [[ -z "${title}" ]]; then
        die "Title required. Usage: boi spec <id> add \"Task Title\" [--spec \"...\"] [--verify \"...\"]"
    fi

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${spec_file}" "${title}" "${spec_text}" "${verify_text}" <<'PYEOF'
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.spec_editor import add_task

spec_file = sys.argv[1]
title = sys.argv[2]
spec_text = sys.argv[3]
verify_text = sys.argv[4]

try:
    new_id = add_task(spec_file, title, spec_text, verify_text)
    print(f"Added task {new_id}: {title}")
except ValueError as e:
    print(f"Error: {e}", file=sys.stderr)
    sys.exit(1)
PYEOF
}

_spec_skip() {
    local spec_file="$1"
    shift
    local task_id=""
    local reason=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --reason)
                reason="$2"
                shift 2
                ;;
            -*)
                die "Unknown option for 'skip': $1"
                ;;
            *)
                if [[ -z "${task_id}" ]]; then
                    task_id="$1"
                else
                    die "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    if [[ -z "${task_id}" ]]; then
        die "Task ID required. Usage: boi spec <id> skip <task-id> [--reason \"...\"]"
    fi

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${spec_file}" "${task_id}" "${reason}" <<'PYEOF'
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.spec_editor import skip_task

spec_file = sys.argv[1]
task_id = sys.argv[2]
reason = sys.argv[3]

try:
    skip_task(spec_file, task_id, reason)
    print(f"Task {task_id} marked as SKIPPED.")
except ValueError as e:
    print(f"Error: {e}", file=sys.stderr)
    sys.exit(1)
PYEOF
}

_spec_next() {
    local spec_file="$1"
    shift
    local task_id=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help) echo "Usage: boi spec <id> next <task-id>"; exit 0 ;;
            -*) die "Unknown option for 'next': $1" ;;
            *)
                if [[ -z "${task_id}" ]]; then
                    task_id="$1"
                else
                    die "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    if [[ -z "${task_id}" ]]; then
        die "Task ID required. Usage: boi spec <id> next <task-id>"
    fi

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${spec_file}" "${task_id}" <<'PYEOF'
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.spec_editor import reorder_task

spec_file = sys.argv[1]
task_id = sys.argv[2]

try:
    reorder_task(spec_file, task_id)
    print(f"Task {task_id} moved to be next.")
except ValueError as e:
    print(f"Error: {e}", file=sys.stderr)
    sys.exit(1)
PYEOF
}

_spec_block() {
    local spec_file="$1"
    shift
    local task_id=""
    local dep_id=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --on)
                dep_id="$2"
                shift 2
                ;;
            -h|--help) echo "Usage: boi spec <id> block <task-id> --on <dep-id>"; exit 0 ;;
            -*) die "Unknown option for 'block': $1" ;;
            *)
                if [[ -z "${task_id}" ]]; then
                    task_id="$1"
                else
                    die "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    if [[ -z "${task_id}" ]]; then
        die "Task ID required. Usage: boi spec <id> block <task-id> --on <dep-id>"
    fi
    if [[ -z "${dep_id}" ]]; then
        die "Dependency required. Usage: boi spec <id> block <task-id> --on <dep-id>"
    fi

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${spec_file}" "${task_id}" "${dep_id}" <<'PYEOF'
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.spec_editor import block_task

spec_file = sys.argv[1]
task_id = sys.argv[2]
dep_id = sys.argv[3]

try:
    block_task(spec_file, task_id, dep_id)
    print(f"Task {task_id} blocked on {dep_id}.")
except ValueError as e:
    print(f"Error: {e}", file=sys.stderr)
    sys.exit(1)
PYEOF
}

_spec_edit() {
    local spec_file="$1"
    shift
    local task_id=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help) echo "Usage: boi spec <id> edit [<task-id>]"; exit 0 ;;
            -*) die "Unknown option for 'edit': $1" ;;
            *)
                if [[ -z "${task_id}" ]]; then
                    task_id="$1"
                else
                    die "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    local editor="${EDITOR:-vi}"

    if [[ -z "${task_id}" ]]; then
        # Edit the full spec file
        "${editor}" "${spec_file}"
    else
        # Extract task section to temp file, edit, splice back
        local tmp_file
        tmp_file=$(mktemp /tmp/boi-spec-edit-XXXXXX.md)

        # Extract the task section and capture offsets
        local offsets
        offsets=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${spec_file}" "${task_id}" "${tmp_file}" <<'PYEOF'
import os
import re
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])

spec_file = sys.argv[1]
task_id = sys.argv[2]
tmp_file = sys.argv[3]

with open(spec_file, encoding="utf-8") as f:
    content = f.read()

# Find task section
pattern = re.compile(r"^(###\s+" + re.escape(task_id) + r":\s+.+)$", re.MULTILINE)
match = pattern.search(content)
if match is None:
    print(f"Error: Task {task_id} not found in spec", file=sys.stderr)
    sys.exit(1)

start = match.start()
# Find next task heading
next_heading = re.compile(r"^###\s+t-\d+:\s+", re.MULTILINE)
rest = content[match.end():]
next_match = next_heading.search(rest)
if next_match is not None:
    end = match.end() + next_match.start()
else:
    end = len(content)

section = content[start:end]
with open(tmp_file, "w", encoding="utf-8") as f:
    f.write(section)

# Print offsets for splice-back
print(f"{start},{end}")
PYEOF
        )
        if [[ $? -ne 0 ]]; then
            rm -f "${tmp_file}"
            die "Failed to extract task ${task_id}"
        fi

        # Open editor
        "${editor}" "${tmp_file}"

        # Splice back
        local start_offset end_offset
        start_offset="${offsets%%,*}"
        end_offset="${offsets##*,}"

        BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${spec_file}" "${tmp_file}" "${start_offset}" "${end_offset}" <<'PYEOF'
import os
import sys

spec_file = sys.argv[1]
tmp_file = sys.argv[2]
start = int(sys.argv[3])
end = int(sys.argv[4])

with open(spec_file, encoding="utf-8") as f:
    content = f.read()

with open(tmp_file, encoding="utf-8") as f:
    new_section = f.read()

new_content = content[:start] + new_section + content[end:]

tmp_out = spec_file + ".tmp"
with open(tmp_out, "w", encoding="utf-8") as f:
    f.write(new_content)
os.rename(tmp_out, spec_file)

print("Task updated.")
PYEOF

        rm -f "${tmp_file}"
    fi
}

# ─── Subcommand: project ────────────────────────────────────────────────────

_project_usage() {
    echo -e "${BOLD}BOI Project${NC} — Project management"
    echo ""
    echo "Usage: boi project <subcommand> [options]"
    echo ""
    echo "Subcommands:"
    echo "  create <name> [--description \"...\"]   Create a new project"
    echo "  list [--json]                         List all projects"
    echo "  status <name> [--json]                Show project status + specs"
    echo "  context <name>                        Print project context.md"
    echo "  delete <name>                         Delete a project"
    echo ""
    echo "Examples:"
    echo "  boi project create my-app --description \"My cool app\""
    echo "  boi project list                      # show all projects"
    echo "  boi project list --json               # JSON output"
    echo "  boi project status my-app             # show project + its specs"
    echo "  boi project context my-app            # print context.md"
    echo "  boi project delete my-app             # remove project"
}

cmd_project() {
    if [[ $# -eq 0 ]] || [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]] || [[ "$1" == "help" ]]; then
        _project_usage
        exit 0
    fi

    local subcommand="$1"
    shift

    case "${subcommand}" in
        create)  _project_create "$@" ;;
        list)    _project_list "$@" ;;
        status)  _project_status "$@" ;;
        context) _project_context "$@" ;;
        delete)  _project_delete "$@" ;;
        -h|--help|help) _project_usage; exit 0 ;;
        *)
            die "Unknown project subcommand: ${subcommand}. Use 'boi project --help' for usage."
            ;;
    esac
}

_project_create() {
    local name=""
    local description=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --description) description="$2"; shift 2 ;;
            -h|--help) _project_usage; exit 0 ;;
            -*) die "Unknown option: $1" ;;
            *)
                if [[ -z "${name}" ]]; then
                    name="$1"; shift
                else
                    die "Unexpected argument: $1"
                fi
                ;;
        esac
    done

    [[ -z "${name}" ]] && die "Usage: boi project create <name> [--description \"...\"]"

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${name}" "${description}" <<'PYEOF'
import json
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.project import create_project

name = sys.argv[1]
description = sys.argv[2]

try:
    project = create_project(name, description)
    print(f"Created project '{name}' at ~/.boi/projects/{name}/")
except ValueError as e:
    print(f"Error: {e}", file=sys.stderr)
    sys.exit(1)
PYEOF
}

_project_list() {
    local json_mode=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --json) json_mode=true; shift ;;
            -h|--help) _project_usage; exit 0 ;;
            -*) die "Unknown option: $1" ;;
            *) die "Unexpected argument: $1" ;;
        esac
    done

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${json_mode}" <<'PYEOF'
import json
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.project import list_projects

json_mode = sys.argv[1] == "true"
projects = list_projects()

if json_mode:
    print(json.dumps(projects, indent=2))
    sys.exit(0)

if not projects:
    print("No projects found.")
    sys.exit(0)

# Table header
print(f"{'NAME':<24}{'SPECS':<8}{'DESCRIPTION'}")
for p in projects:
    name = p.get("name", "?")
    specs = p.get("spec_count", 0)
    desc = p.get("description", "")
    if len(desc) > 50:
        desc = desc[:47] + "..."
    print(f"{name:<24}{specs:<8}{desc}")
PYEOF
}

_project_status() {
    local name=""
    local json_mode=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --json) json_mode=true; shift ;;
            -h|--help) _project_usage; exit 0 ;;
            -*) die "Unknown option: $1" ;;
            *)
                if [[ -z "${name}" ]]; then
                    name="$1"; shift
                else
                    die "Unexpected argument: $1"
                fi
                ;;
        esac
    done

    [[ -z "${name}" ]] && die "Usage: boi project status <name> [--json]"

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${name}" "${json_mode}" "${QUEUE_DIR}" <<'PYEOF'
import json
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.project import get_project

name = sys.argv[1]
json_mode = sys.argv[2] == "true"
queue_dir = sys.argv[3]

project = get_project(name)
if project is None:
    print(f"Error: Project '{name}' not found", file=sys.stderr)
    sys.exit(1)

# Find specs belonging to this project
specs = []
if os.path.isdir(queue_dir):
    for f in sorted(os.listdir(queue_dir)):
        if not f.startswith("q-") or not f.endswith(".json"):
            continue
        if ".telemetry" in f or ".iteration-" in f:
            continue
        fpath = os.path.join(queue_dir, f)
        try:
            with open(fpath, encoding="utf-8") as fh:
                entry = json.load(fh)
            if entry.get("project") == name:
                specs.append(entry)
        except (json.JSONDecodeError, OSError):
            continue

if json_mode:
    output = dict(project)
    output["specs"] = specs
    print(json.dumps(output, indent=2))
    sys.exit(0)

# Human-readable output
print(f"Project: {name}")
desc = project.get("description", "")
if desc:
    print(f"Description: {desc}")
print(f"Created: {project.get('created_at', '?')}")
print(f"Default priority: {project.get('default_priority', 100)}")
print(f"Default max iter: {project.get('default_max_iter', 30)}")
print()

if not specs:
    print("No specs in queue for this project.")
else:
    print(f"{'QUEUE':<16}{'STATUS':<14}{'ITER':<10}{'SPEC'}")
    for entry in specs:
        qid = entry.get("id", "?")
        status = entry.get("status", "?")
        iteration = entry.get("iteration", 0)
        max_iter = entry.get("max_iterations", "?")
        spec_path = entry.get("original_spec_path", entry.get("spec_path", ""))
        spec_name = os.path.basename(spec_path) if spec_path else "?"
        print(f"{qid:<16}{status:<14}{iteration}/{max_iter:<8}{spec_name}")
PYEOF
}

_project_context() {
    local name=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help) _project_usage; exit 0 ;;
            -*) die "Unknown option: $1" ;;
            *)
                if [[ -z "${name}" ]]; then
                    name="$1"; shift
                else
                    die "Unexpected argument: $1"
                fi
                ;;
        esac
    done

    [[ -z "${name}" ]] && die "Usage: boi project context <name>"

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${name}" <<'PYEOF'
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.project import get_project, get_project_context

name = sys.argv[1]

project = get_project(name)
if project is None:
    print(f"Error: Project '{name}' not found", file=sys.stderr)
    sys.exit(1)

content = get_project_context(name)
if content:
    print(content, end="")
else:
    print(f"(no context.md for project '{name}')")
PYEOF
}

_project_delete() {
    local name=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help) _project_usage; exit 0 ;;
            -*) die "Unknown option: $1" ;;
            *)
                if [[ -z "${name}" ]]; then
                    name="$1"; shift
                else
                    die "Unexpected argument: $1"
                fi
                ;;
        esac
    done

    [[ -z "${name}" ]] && die "Usage: boi project delete <name>"

    # Check project exists
    if [[ ! -d "${BOI_STATE_DIR}/projects/${name}" ]]; then
        die "Project '${name}' not found"
    fi

    # Confirm with user
    echo -n "Delete project '${name}'? This does not cancel running specs. [y/N] "
    read -r confirm
    if [[ "${confirm}" != "y" && "${confirm}" != "Y" ]]; then
        echo "Cancelled."
        exit 0
    fi

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${name}" <<'PYEOF'
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.project import delete_project

name = sys.argv[1]

try:
    delete_project(name)
    print(f"Deleted project '{name}'")
except ValueError as e:
    print(f"Error: {e}", file=sys.stderr)
    sys.exit(1)
PYEOF
}

# ─── Do (Natural Language) ────────────────────────────────────────────────────

cmd_do() {
    local user_input=""
    local dry_run=false
    local yes_mode=false

    # Recursion guard: prevent boi do from calling itself
    if [[ "${BOI_DO_DEPTH:-0}" -ge 1 ]]; then
        die "boi do cannot call itself (recursion depth ${BOI_DO_DEPTH})"
    fi
    export BOI_DO_DEPTH=$(( ${BOI_DO_DEPTH:-0} + 1 ))

    # Parse args
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --dry-run) dry_run=true; shift ;;
            --yes|-y) yes_mode=true; shift ;;
            -h|--help)
                echo "Usage: boi do \"natural language command\" [--dry-run] [--yes]"
                echo ""
                echo "Translate natural language into BOI CLI commands using an LLM."
                echo ""
                echo "Options:"
                echo "  --dry-run   Show generated commands without executing"
                echo "  --yes, -y   Skip confirmation for destructive commands"
                echo ""
                echo "Examples:"
                echo "  boi do \"show status\"                    # runs: boi status"
                echo "  boi do \"cancel the stuck spec\"          # runs: boi cancel q-NNN"
                echo "  boi do --dry-run \"dispatch my spec\"     # preview commands"
                echo "  boi do --yes \"skip task t-5 on q-007\"   # auto-confirm"
                exit 0
                ;;
            -*) die "Unknown option: $1" ;;
            *) user_input="$1"; shift ;;
        esac
    done

    [[ -z "${user_input}" ]] && die "Usage: boi do \"your request here\""

    # Check claude is available
    if ! command -v claude &>/dev/null; then
        die "claude CLI not found. Install Claude Code to use 'boi do'."
    fi

    # Step 1-2: Gather context and build prompt
    info "Gathering context..."
    local prompt
    prompt=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${user_input}" <<'PYEOF'
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.do import gather_context, build_prompt

user_input = sys.argv[1]
context = gather_context(user_input)
prompt = build_prompt(user_input, context)
print(prompt)
PYEOF
    ) || die "Failed to build prompt"

    # Step 3: Call claude -p with the prompt
    info "Asking Claude..."
    local response
    response=$(claude -p "${prompt}" 2>/dev/null) || die "Claude invocation failed"

    # Step 4: Parse response
    local parsed
    parsed=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${response}" <<'PYEOF'
import json
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.do import parse_response, classify_destructive

response_text = sys.argv[1]

try:
    data = parse_response(response_text)
except ValueError as e:
    print(json.dumps({"error": str(e)}))
    sys.exit(1)

# Safety-net: override destructive classification if needed
if classify_destructive(data["commands"]):
    data["destructive"] = True

print(json.dumps(data))
PYEOF
    ) || die "Failed to parse Claude response"

    # Check for parse error
    local parse_error
    parse_error=$(echo "${parsed}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('error',''))" 2>/dev/null)
    if [[ -n "${parse_error}" ]]; then
        die "Failed to parse response: ${parse_error}"
    fi

    # Extract fields from parsed response
    local explanation
    explanation=$(echo "${parsed}" | python3 -c "import json,sys; print(json.load(sys.stdin)['explanation'])")
    local destructive
    destructive=$(echo "${parsed}" | python3 -c "import json,sys; print(json.load(sys.stdin)['destructive'])")
    local commands_json
    commands_json=$(echo "${parsed}" | python3 -c "import json,sys; print(json.dumps(json.load(sys.stdin)['commands']))")

    # Display explanation
    echo ""
    echo -e "${BOLD}Plan:${NC} ${explanation}"
    echo ""

    # Display commands
    local num_commands
    num_commands=$(echo "${commands_json}" | python3 -c "import json,sys; print(len(json.load(sys.stdin)))")

    echo -e "${BOLD}Commands:${NC}"
    local i=0
    while [[ ${i} -lt ${num_commands} ]]; do
        local cmd
        cmd=$(echo "${commands_json}" | python3 -c "import json,sys; print(json.load(sys.stdin)[${i}])")
        echo "  ${cmd}"
        i=$((i + 1))
    done
    echo ""

    # Dry run: stop here
    if [[ "${dry_run}" == "true" ]]; then
        echo -e "${DIM}(dry run — commands not executed)${NC}"
        return 0
    fi

    # Confirm destructive commands unless --yes
    if [[ "${destructive}" == "True" && "${yes_mode}" != "true" ]]; then
        echo -e "${YELLOW}Warning:${NC} This includes destructive operations."
        echo -n "Execute? [y/N] "
        read -r confirm
        if [[ "${confirm}" != "y" && "${confirm}" != "Y" ]]; then
            echo "Cancelled."
            return 0
        fi
    fi

    # Step 5: Execute commands
    local i=0
    while [[ ${i} -lt ${num_commands} ]]; do
        local cmd
        cmd=$(echo "${commands_json}" | python3 -c "import json,sys; print(json.load(sys.stdin)[${i}])")
        echo -e "${DIM}\$ ${cmd}${NC}"
        eval "${cmd}"
        local exit_code=$?
        if [[ ${exit_code} -ne 0 ]]; then
            warn "Command exited with code ${exit_code}: ${cmd}"
        fi
        i=$((i + 1))
    done
}

# ─── Main ────────────────────────────────────────────────────────────────────

usage() {
    echo -e "${BOLD}BOI${NC} — Beginning of Infinity"
    echo ""
    echo "Self-evolving autonomous agent fleet."
    echo ""
    echo "Usage: boi <command> [options]"
    echo ""
    echo "Commands:"
    echo "  install     Create worktrees and configure BOI"
    echo "  dispatch    Submit a spec to the queue"
    echo "  queue       Show spec queue with status"
    echo "  status      Show workers + queue progress"
    echo "  log         View logs for a spec"
    echo "  cancel      Cancel a queued/running spec"
    echo "  stop        Stop daemon and all workers"
    echo "  workers     Show worker/worktree status"
    echo "  telemetry   Show per-iteration breakdown"
    echo "  review      Review experiment proposals on a paused spec"
    echo "  purge       Remove completed/failed/canceled specs"
    echo "  critic      Manage the critic validation system"
    echo "  spec        Live spec management (add, skip, reorder, block tasks)"
    echo "  project     Manage projects (create, list, status, context, delete)"
    echo "  do          Translate natural language into BOI commands"
    echo "  doctor      Check prerequisites and environment health"
    echo "  dashboard   Live-updating queue progress"
    echo ""
    echo "Examples:"
    echo "  boi install                         # one-time setup"
    echo "  boi dispatch --spec spec.md         # submit a spec"
    echo "  boi dispatch --spec s.md --priority 50  # high priority"
    echo "  boi queue                           # view queue"
    echo "  boi status                          # check progress"
    echo "  boi status --watch                  # live dashboard"
    echo "  boi log q-001                       # tail spec output"
    echo "  boi log q-001 --full                # full output"
    echo "  boi review q-001                    # review experiments"
    echo "  boi cancel q-001                    # cancel a spec"
    echo "  boi stop                            # stop everything"
    echo "  boi workers                         # show worktrees"
    echo "  boi telemetry q-001                 # iteration breakdown"
    echo "  boi purge                           # clean finished specs"
    echo "  boi purge --dry-run                 # preview purge"
    echo "  boi critic status                   # show critic config"
    echo "  boi spec q-001                      # show spec tasks"
    echo "  boi spec q-001 add \"Fix tests\"      # add a task"
    echo "  boi project create my-app            # create a project"
    echo "  boi project list                     # list projects"
    echo "  boi do \"show me what's running\"       # natural language"
    echo "  boi do --dry-run \"cancel stuck specs\" # preview commands"
    echo "  boi doctor                          # check prerequisites"
    echo "  boi --version                       # show version"
}

main() {
    if [[ $# -eq 0 ]]; then
        usage
        exit 0
    fi

    local command="$1"
    shift

    case "${command}" in
        install)    cmd_install "$@" ;;
        dispatch)   cmd_dispatch "$@" ;;
        queue)      cmd_queue "$@" ;;
        status)     cmd_status "$@" ;;
        log)        cmd_log "$@" ;;
        cancel)     cmd_cancel "$@" ;;
        review)     cmd_review "$@" ;;
        stop)       cmd_stop "$@" ;;
        purge)      cmd_purge "$@" ;;
        workers)    cmd_workers "$@" ;;
        telemetry)  cmd_telemetry "$@" ;;
        doctor)     cmd_doctor "$@" ;;
        critic)     cmd_critic "$@" ;;
        spec)       cmd_spec "$@" ;;
        project)    cmd_project "$@" ;;
        do)         cmd_do "$@" ;;
        dashboard)  cmd_dashboard "$@" ;;
        -h|--help|help) usage; exit 0 ;;
        --version) echo "boi ${BOI_VERSION}"; exit 0 ;;
        *)
            die "Unknown command: ${command}. Use 'boi --help' for usage."
            ;;
    esac
}

main "$@"
