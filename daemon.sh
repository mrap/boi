#!/bin/bash
# daemon.sh — Queue-aware dispatch daemon for BOI.
#
# Main orchestration loop that:
#   - Scans ~/.boi/queue/ for specs with status "queued" or "requeued"
#   - Sorts by priority, filters out DAG-blocked specs
#   - Assigns specs to free workers
#   - Monitors worker PIDs every poll cycle
#   - Detects completion, requeues unfinished specs, handles crashes
#   - Writes events to ~/.boi/events/
#
# Usage:
#   bash daemon.sh              # Start the daemon
#   bash daemon.sh --foreground # Run in foreground (for debugging)
#   bash daemon.sh --stop       # Stop the running daemon

set -uo pipefail

# Constants
BOI_STATE_DIR="${HOME}/.boi"
BOI_CONFIG="${BOI_STATE_DIR}/config.json"
PID_FILE="${BOI_STATE_DIR}/daemon.pid"
LOCK_FILE="${BOI_STATE_DIR}/daemon.lock"
QUEUE_DIR="${BOI_STATE_DIR}/queue"
EVENTS_DIR="${BOI_STATE_DIR}/events"
LOG_DIR="${BOI_STATE_DIR}/logs"
DAEMON_LOG="${LOG_DIR}/daemon.log"
POLL_INTERVAL_S=5
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKER_SCRIPT="${SCRIPT_DIR}/worker.sh"

# Colors (only used in foreground mode)
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# State tracking
declare -A WORKER_CHECKOUTS
declare -A WORKER_CURRENT_SPEC
declare -A WORKER_PIDS
declare -A WORKER_START_TIME

FOREGROUND=false
DEFAULT_WORKER_TIMEOUT=1800  # 30 minutes

# ─── Logging ──────────────────────────────────────────────────────────────────

log() {
    local level="$1"
    shift
    local timestamp
    timestamp=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    local msg="[${timestamp}] [${level}] $*"

    if [[ "${FOREGROUND}" == "true" ]]; then
        case "${level}" in
            ERROR) echo -e "${RED}${msg}${NC}" ;;
            WARN)  echo -e "${YELLOW}${msg}${NC}" ;;
            INFO)  echo -e "${GREEN}${msg}${NC}" ;;
            *)     echo "${msg}" ;;
        esac
    fi

    echo "${msg}" >> "${DAEMON_LOG}"
}

log_info()  { log "INFO" "$@"; }
log_warn()  { log "WARN" "$@"; }
log_error() { log "ERROR" "$@"; }

# ─── Event Writing ──────────────────────────────────────────────────────────

write_event() {
    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "$@" <<'PYEOF'
import sys, json, os
sys.path.insert(0, os.environ.get("BOI_SCRIPT_DIR", "."))
from lib.event_log import write_event as _write

event = {}
for arg in sys.argv[1:]:
    if "=" not in arg:
        continue
    key, val = arg.split("=", 1)
    try:
        val = int(val)
    except ValueError:
        try:
            val = float(val)
        except ValueError:
            if val.startswith("[") or val.startswith("{"):
                try:
                    val = json.loads(val)
                except json.JSONDecodeError:
                    pass
    event[key] = val

events_dir = os.environ.get("BOI_EVENTS_DIR", os.path.expanduser("~/.boi/events"))
seq = _write(events_dir, event)
print(seq)
PYEOF
}

# ─── Worker Management ───────────────────────────────────────────────────────

load_workers_from_config() {
    while IFS= read -r worker_json; do
        if [[ -z "${worker_json}" ]]; then
            continue
        fi
        local wid wpath
        wid=$(echo "${worker_json}" | python3 -c "import json,sys; print(json.loads(sys.stdin.read())['id'])")
        wpath=$(echo "${worker_json}" | python3 -c "import json,sys; w=json.loads(sys.stdin.read()); print(w.get('worktree_path', w.get('checkout_path', '')))")
        WORKER_CHECKOUTS["${wid}"]="${wpath}"
        WORKER_CURRENT_SPEC["${wid}"]=""
        WORKER_PIDS["${wid}"]=""
        WORKER_START_TIME["${wid}"]=""
    done < <(python3 - "${BOI_CONFIG}" <<'PYEOF'
import json, sys
data = json.load(open(sys.argv[1]))
for w in data.get("workers", []):
    print(json.dumps(w))
PYEOF
    )
}

get_worker_timeout() {
    # Get timeout from config, with optional per-spec override from queue entry
    local queue_id="${1:-}"
    local timeout="${DEFAULT_WORKER_TIMEOUT}"

    # Check config for global timeout
    local config_timeout
    config_timeout=$(python3 -c "
import json
try:
    with open('${BOI_CONFIG}') as f:
        c = json.load(f)
    print(c.get('worker_timeout_seconds', ${DEFAULT_WORKER_TIMEOUT}))
except Exception:
    print(${DEFAULT_WORKER_TIMEOUT})
" 2>/dev/null || echo "${DEFAULT_WORKER_TIMEOUT}")
    timeout="${config_timeout}"

    # Check per-spec override
    if [[ -n "${queue_id}" ]]; then
        local spec_timeout
        spec_timeout=$(python3 -c "
import json
try:
    with open('${QUEUE_DIR}/${queue_id}.json') as f:
        e = json.load(f)
    t = e.get('worker_timeout_seconds')
    if t is not None:
        print(t)
    else:
        print('')
except Exception:
    print('')
" 2>/dev/null || echo "")
        if [[ -n "${spec_timeout}" ]]; then
            timeout="${spec_timeout}"
        fi
    fi

    echo "${timeout}"
}

get_free_worker() {
    for wid in $(echo "${!WORKER_CURRENT_SPEC[@]}" | tr ' ' '\n' | sort); do
        if [[ -z "${WORKER_CURRENT_SPEC[${wid}]}" ]]; then
            local checkout="${WORKER_CHECKOUTS[${wid}]}"
            if [[ -d "${checkout}" ]]; then
                echo "${wid}"
                return 0
            fi
        fi
    done
    return 1
}

assign_spec_to_worker() {
    local queue_id="$1"
    local worker_id="$2"
    local worktree_path="${WORKER_CHECKOUTS[${worker_id}]}"

    log_info "Assigning spec ${queue_id} to worker ${worker_id} (${worktree_path})"

    # Set running and get iteration + spec_path + phase in one Python call
    local assign_json
    assign_json=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" "${worker_id}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.queue import set_running, get_entry
set_running(sys.argv[1], sys.argv[2], sys.argv[3])
entry = get_entry(sys.argv[1], sys.argv[2])
result = {
    "iteration": entry.get("iteration", 1) if entry else 1,
    "spec_path": entry.get("spec_path", "") if entry else "",
    "phase": entry.get("phase", "execute") if entry else "execute",
}
print(json.dumps(result))
PYEOF
    )

    local iteration spec_path phase
    iteration=$(echo "${assign_json}" | python3 -c "import json,sys; print(json.loads(sys.stdin.read())['iteration'])")
    spec_path=$(echo "${assign_json}" | python3 -c "import json,sys; print(json.loads(sys.stdin.read())['spec_path'])")
    phase=$(echo "${assign_json}" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('phase','execute'))")

    # Launch worker with appropriate flags based on phase
    local worker_flags=""
    if [[ "${phase}" == "decompose" ]]; then
        worker_flags="--decompose"
        log_info "Phase: DECOMPOSE — launching decomposition worker"

        # Set decomposition timeout (15 minutes) if not already set
        BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.queue import _read_entry, _write_entry
entry = _read_entry(sys.argv[1], sys.argv[2])
if entry and "worker_timeout_seconds" not in entry:
    entry["worker_timeout_seconds"] = 900  # 15 minutes for decomposition
    _write_entry(sys.argv[1], entry)
PYEOF
    elif [[ "${phase}" == "evaluate" ]]; then
        worker_flags="--evaluate"
        log_info "Phase: EVALUATE — launching evaluation worker"

        # Set evaluation timeout (15 minutes) if not already set
        BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.queue import _read_entry, _write_entry
entry = _read_entry(sys.argv[1], sys.argv[2])
if entry and "worker_timeout_seconds" not in entry:
    entry["worker_timeout_seconds"] = 900  # 15 minutes for evaluation
    _write_entry(sys.argv[1], entry)
PYEOF
    fi
    bash "${WORKER_SCRIPT}" ${worker_flags} "${queue_id}" "${worktree_path}" "${spec_path}" "${iteration}" >> "${DAEMON_LOG}" 2>&1

    # Read PID from the PID file
    local pid_file="${QUEUE_DIR}/${queue_id}.pid"
    local pid=""
    if [[ -f "${pid_file}" ]]; then
        pid=$(cat "${pid_file}")
    fi

    if [[ -z "${pid}" ]]; then
        log_error "Failed to get PID for spec ${queue_id}."
        return 1
    fi

    WORKER_CURRENT_SPEC["${worker_id}"]="${queue_id}"
    WORKER_PIDS["${worker_id}"]="${pid}"
    WORKER_START_TIME["${worker_id}"]="$(date +%s)"

    local timestamp
    timestamp=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    write_event "type=spec_started" "queue_id=${queue_id}" "worker_id=${worker_id}" "iteration=${iteration}" "timestamp=${timestamp}"

    log_info "Spec ${queue_id} assigned to ${worker_id} (PID ${pid}, iteration ${iteration})"
    return 0
}

check_worker_completion() {
    local worker_id="$1"
    local queue_id="${WORKER_CURRENT_SPEC[${worker_id}]}"
    local pid="${WORKER_PIDS[${worker_id}]}"

    # Skip idle workers
    if [[ -z "${queue_id}" ]] || [[ -z "${pid}" ]]; then
        return 0
    fi

    # Check for timeout before checking PID
    local start_time="${WORKER_START_TIME[${worker_id}]:-}"
    if [[ -n "${start_time}" ]]; then
        local now_epoch
        now_epoch=$(date +%s)
        local elapsed=$(( now_epoch - start_time ))
        local timeout
        timeout=$(get_worker_timeout "${queue_id}")

        if [[ ${elapsed} -ge ${timeout} ]]; then
            local elapsed_min=$(( elapsed / 60 ))
            local timeout_min=$(( timeout / 60 ))
            log_warn "Worker ${worker_id} timed out after ${elapsed_min}m for spec ${queue_id} (limit: ${timeout_min}m)"

            # Kill the tmux session
            local tmux_session="boi-${queue_id}"
            if tmux -L boi has-session -t "${tmux_session}" 2>/dev/null; then
                tmux -L boi kill-session -t "${tmux_session}" 2>/dev/null
                log_info "Killed tmux session ${tmux_session} due to timeout"
            fi

            # Also kill the PID directly if still alive
            if kill -0 "${pid}" 2>/dev/null; then
                kill "${pid}" 2>/dev/null
                sleep 2
                if kill -0 "${pid}" 2>/dev/null; then
                    kill -9 "${pid}" 2>/dev/null
                fi
            fi

            # Treat as crash
            log_info "Processing timeout as crash for spec ${queue_id}"

            local exit_file="${QUEUE_DIR}/${queue_id}.exit"
            # No exit file from timeout — process_worker_completion handles crash path

            local result_json
            result_json=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" "${EVENTS_DIR}" "${LOG_DIR}" "${BOI_STATE_DIR}/hooks" "${SCRIPT_DIR}" "__none__" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.daemon_ops import process_worker_completion

result = process_worker_completion(
    queue_dir=sys.argv[1],
    queue_id=sys.argv[2],
    events_dir=sys.argv[3],
    log_dir=sys.argv[4],
    hooks_dir=sys.argv[5],
    script_dir=sys.argv[6],
    exit_code=None,
)
print(json.dumps(result))
PYEOF
            )

            local outcome
            outcome=$(echo "${result_json}" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('outcome','unknown'))" 2>/dev/null || echo "unknown")
            log_warn "Spec ${queue_id} timed out — outcome: ${outcome}"

            free_worker "${worker_id}" "${queue_id}"
            return 0
        fi
    fi

    # Check if PID is still alive
    if kill -0 "${pid}" 2>/dev/null; then
        return 0
    fi

    # Worker has exited. Process completion in a single Python call.
    log_info "Worker ${worker_id} (PID ${pid}) has exited for spec ${queue_id}"

    local exit_file="${QUEUE_DIR}/${queue_id}.exit"

    # Read exit code from exit file (written by run script inside tmux)
    local exit_code_arg="__none__"
    if [[ -f "${exit_file}" ]]; then
        local worker_exit_code
        worker_exit_code=$(cat "${exit_file}" 2>/dev/null || true)
        if [[ -n "${worker_exit_code}" ]]; then
            exit_code_arg="${worker_exit_code}"
        fi
    fi

    # Single batched Python call for all completion logic
    # Check phase to determine which completion handler to use
    local result_json
    result_json=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" "${EVENTS_DIR}" "${LOG_DIR}" "${BOI_STATE_DIR}/hooks" "${SCRIPT_DIR}" "${exit_code_arg}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.queue import get_entry

queue_dir = sys.argv[1]
queue_id = sys.argv[2]
events_dir = sys.argv[3]
exit_code = sys.argv[7] if sys.argv[7] != "__none__" else None

# Check the phase to determine which handler to use
entry = get_entry(queue_dir, queue_id)
phase = entry.get("phase", "execute") if entry else "execute"

if phase == "decompose":
    from lib.daemon_ops import process_decomposition_completion
    result = process_decomposition_completion(
        queue_dir=queue_dir,
        queue_id=queue_id,
        events_dir=events_dir,
        spec_path=entry.get("spec_path", "") if entry else "",
        exit_code=exit_code,
    )
elif phase == "evaluate":
    from lib.daemon_ops import process_evaluation_completion
    result = process_evaluation_completion(
        queue_dir=queue_dir,
        queue_id=queue_id,
        events_dir=events_dir,
        hooks_dir=sys.argv[5],
        spec_path=entry.get("spec_path", "") if entry else "",
        exit_code=exit_code,
    )
else:
    from lib.daemon_ops import process_worker_completion
    result = process_worker_completion(
        queue_dir=queue_dir,
        queue_id=queue_id,
        events_dir=events_dir,
        log_dir=sys.argv[4],
        hooks_dir=sys.argv[5],
        script_dir=sys.argv[6],
        exit_code=exit_code,
    )
print(json.dumps(result))
PYEOF
    )

    # Log the outcome
    local outcome
    outcome=$(echo "${result_json}" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('outcome','unknown'))" 2>/dev/null || echo "unknown")

    case "${outcome}" in
        completed)
            log_info "Spec ${queue_id} completed" ;;
        requeued)
            log_info "Spec ${queue_id} requeued (still has pending tasks)" ;;
        failed)
            local reason
            reason=$(echo "${result_json}" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('reason',''))" 2>/dev/null || echo "")
            log_warn "Spec ${queue_id} failed: ${reason}" ;;
        crashed)
            log_warn "Spec ${queue_id} crashed, requeued with cooldown" ;;
        needs_review)
            log_info "Spec ${queue_id} paused for experiment review" ;;
        decomposition_complete)
            local task_count
            task_count=$(echo "${result_json}" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('task_count',0))" 2>/dev/null || echo "0")
            log_info "Spec ${queue_id} decomposition complete (${task_count} tasks). Transitioning to execute phase." ;;
        decomposition_retry)
            log_warn "Spec ${queue_id} decomposition failed, retrying..." ;;
        decomposition_failed)
            local reason
            reason=$(echo "${result_json}" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('reason',''))" 2>/dev/null || echo "")
            log_error "Spec ${queue_id} decomposition failed permanently: ${reason}" ;;
        evaluate_phase_entered)
            log_info "Spec ${queue_id} entering evaluate phase (checking Success Criteria)." ;;
        evaluate_converged)
            local status
            status=$(echo "${result_json}" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('status',''))" 2>/dev/null || echo "")
            log_info "Spec ${queue_id} evaluation converged: ${status}" ;;
        evaluate_loop_back)
            local pending
            pending=$(echo "${result_json}" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('pending_count',0))" 2>/dev/null || echo "0")
            log_info "Spec ${queue_id} evaluation found ${pending} unmet criteria. Looping back to execute phase." ;;
        evaluate_crashed)
            log_warn "Spec ${queue_id} evaluation crashed, requeued." ;;
        *)
            log_error "Spec ${queue_id} unknown outcome: ${outcome}" ;;
    esac

    free_worker "${worker_id}" "${queue_id}"
}

# free_worker: release a worker after its spec finishes (any outcome).
free_worker() {
    local worker_id="$1"
    local queue_id="$2"

    WORKER_CURRENT_SPEC["${worker_id}"]=""
    WORKER_PIDS["${worker_id}"]=""
    WORKER_START_TIME["${worker_id}"]=""

    # Clean up PID and exit files
    rm -f "${QUEUE_DIR}/${queue_id}.pid"
    rm -f "${QUEUE_DIR}/${queue_id}.exit"
}

# ─── Main Loop ────────────────────────────────────────────────────────────────

write_daemon_state() {
    local state_file="${BOI_STATE_DIR}/daemon-state.json"
    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${BOI_CONFIG}" "${state_file}" <<'PYEOF'
import json, sys, os
from datetime import datetime, timezone

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.queue import get_queue

queue_dir = sys.argv[1]
config_path = sys.argv[2]
state_file = sys.argv[3]

entries = get_queue(queue_dir)

# Count by status
counts = {"queued": 0, "requeued": 0, "running": 0, "completed": 0, "failed": 0, "canceled": 0, "needs_review": 0}
for e in entries:
    s = e.get("status", "queued")
    if s in counts:
        counts[s] += 1

# Load worker count
workers = []
try:
    with open(config_path) as f:
        config = json.load(f)
        workers = config.get("workers", [])
except Exception:
    pass

state = {
    "timestamp": datetime.now(timezone.utc).isoformat(),
    "daemon_pid": os.getpid(),
    "queue_summary": counts,
    "total_specs": len(entries),
    "worker_count": len(workers),
    "specs": [
        {
            "id": e["id"],
            "status": e.get("status"),
            "priority": e.get("priority"),
            "iteration": e.get("iteration", 0),
            "max_iterations": e.get("max_iterations", 30),
            "last_worker": e.get("last_worker"),
            "tasks_done": e.get("tasks_done", 0),
            "tasks_total": e.get("tasks_total", 0),
            "consecutive_failures": e.get("consecutive_failures", 0),
        }
        for e in entries
        if e.get("status") in ("queued", "requeued", "running", "needs_review")
    ],
}

tmp = state_file + ".tmp"
with open(tmp, "w") as f:
    json.dump(state, f, indent=2)
    f.write("\n")
os.rename(tmp, state_file)
PYEOF
}

daemon_loop() {
    log_info "Daemon loop started."

    while true; do
        # Check all active workers
        for worker_id in "${!WORKER_CURRENT_SPEC[@]}"; do
            check_worker_completion "${worker_id}"
        done

        # Find and dispatch ready specs
        local free_worker
        free_worker=$(get_free_worker 2>/dev/null || true)

        while [[ -n "${free_worker}" ]]; do
            # Get next eligible spec from queue (single Python call)
            local next_spec
            next_spec=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.daemon_ops import pick_next_spec
result = pick_next_spec(sys.argv[1])
if result:
    print(result["id"])
PYEOF
            )

            if [[ -z "${next_spec}" ]]; then
                break
            fi

            assign_spec_to_worker "${next_spec}" "${free_worker}"
            free_worker=$(get_free_worker 2>/dev/null || true)
        done

        # Check if queue is fully drained (all completed/failed/canceled)
        local active_count
        active_count=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.daemon_ops import get_active_count
print(get_active_count(sys.argv[1]))
PYEOF
        )

        if [[ "${active_count}" == "0" ]]; then
            # Check if any workers are still busy
            local any_busy=false
            for wid in "${!WORKER_CURRENT_SPEC[@]}"; do
                if [[ -n "${WORKER_CURRENT_SPEC[${wid}]}" ]]; then
                    any_busy=true
                    break
                fi
            done

            if [[ "${any_busy}" == "false" ]]; then
                log_info "Queue fully drained. Daemon staying alive for new specs."
            fi
        fi

        # Write daemon state snapshot for monitoring
        write_daemon_state

        # Check for needs_review specs that have timed out
        local auto_rejected
        auto_rejected=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${EVENTS_DIR}" "${BOI_STATE_DIR}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.daemon_ops import check_needs_review_timeouts
rejected = check_needs_review_timeouts(sys.argv[1], sys.argv[2], sys.argv[3])
if rejected:
    print(",".join(rejected))
PYEOF
        )
        if [[ -n "${auto_rejected}" ]]; then
            log_warn "Auto-rejected experiments for timed-out specs: ${auto_rejected}"
        fi

        # Write daemon heartbeat
        local heartbeat_file="${BOI_STATE_DIR}/daemon-heartbeat"
        date -u +"%Y-%m-%dT%H:%M:%SZ" > "${heartbeat_file}.tmp"
        mv "${heartbeat_file}.tmp" "${heartbeat_file}"

        sleep "${POLL_INTERVAL_S}"
    done
}

# ─── Daemon Lifecycle ─────────────────────────────────────────────────────────

write_pid_file() {
    local tmp="${PID_FILE}.tmp"
    echo "$$" > "${tmp}"
    mv "${tmp}" "${PID_FILE}"
}

cleanup() {
    log_info "Daemon shutting down."
    rm -f "${PID_FILE}"
}

stop_daemon() {
    if [[ ! -f "${PID_FILE}" ]]; then
        echo "No daemon running (no PID file)."
        exit 0
    fi

    local pid
    pid=$(cat "${PID_FILE}")

    if kill -0 "${pid}" 2>/dev/null; then
        echo "Stopping daemon (PID ${pid})..."
        kill "${pid}"
        local waited=0
        while kill -0 "${pid}" 2>/dev/null && [[ ${waited} -lt 10 ]]; do
            sleep 1
            waited=$((waited + 1))
        done
        if kill -0 "${pid}" 2>/dev/null; then
            echo "Daemon did not stop gracefully. Sending SIGKILL."
            kill -9 "${pid}" 2>/dev/null
        fi
        echo "Daemon stopped."
    else
        echo "Daemon (PID ${pid}) is not running. Cleaning up PID file."
    fi

    rm -f "${PID_FILE}"
}

main() {
    case "${1:-}" in
        --stop)
            stop_daemon
            exit 0
            ;;
        --foreground)
            FOREGROUND=true
            ;;
        --help|-h)
            echo "Usage: bash daemon.sh [--foreground|--stop|--help]"
            exit 0
            ;;
    esac

    if [[ ! -f "${BOI_CONFIG}" ]]; then
        echo "Error: Config not found at ${BOI_CONFIG}. Run 'boi install' first." >&2
        exit 1
    fi

    mkdir -p "${LOG_DIR}" "${EVENTS_DIR}" "${QUEUE_DIR}" "${BOI_STATE_DIR}/hooks"

    exec 200>"${LOCK_FILE}"
    if ! flock -n 200; then
        echo "Error: Another daemon is already running." >&2
        exit 1
    fi

    trap cleanup EXIT INT TERM

    export BOI_SCRIPT_DIR="${SCRIPT_DIR}"
    export BOI_EVENTS_DIR="${EVENTS_DIR}"

    write_pid_file
    log_info "Daemon started (PID $$)"

    load_workers_from_config
    local worker_count=${#WORKER_CHECKOUTS[@]}
    log_info "Loaded ${worker_count} workers from config"

    # Recover any specs stuck in "running" status from a previous daemon crash
    local recovered
    recovered=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.queue import recover_running_specs
count = recover_running_specs(sys.argv[1])
print(count)
PYEOF
    )
    if [[ "${recovered}" -gt 0 ]]; then
        log_warn "Recovered ${recovered} spec(s) stuck in 'running' status."
    fi

    daemon_loop

    log_info "Daemon exiting."
}

main "$@"
