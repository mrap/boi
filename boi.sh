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
#   upgrade   — Update BOI to the latest version
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
#   boi log <queue-id> [--full] [--failures]
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
SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"

# Colors
RED='\033[38;2;210;15;57m'        # Catppuccin Latte red
GREEN='\033[38;2;64;160;43m'      # Catppuccin Latte green
YELLOW='\033[38;2;223;142;29m'    # Catppuccin Latte yellow/peach
BOLD='\033[1m'
DIM='\033[38;2;108;111;133m'      # Catppuccin Latte subtext0
NC='\033[0m'

# ─── Helpers ─────────────────────────────────────────────────────────────────

die() {
    echo -e "${RED}Error:${NC} $1" >&2
    exit 1
}

die_usage() {
    echo -e "${RED}Error:${NC} $1" >&2
    exit 2
}

warn() {
    echo -e "${YELLOW}Warning:${NC} $1"
}

info() {
    echo -e "${GREEN}boi:${NC} $1"
}

# Progress step: prints "  Description... " without newline, then call progress_done or progress_fail
progress_step() {
    printf "  %s... " "$1"
}

progress_done() {
    local detail="${1:-}"
    if [[ -n "${detail}" ]]; then
        echo -e "${GREEN}✓${NC} (${detail})"
    else
        echo -e "${GREEN}✓${NC}"
    fi
}

progress_fail() {
    local detail="${1:-}"
    if [[ -n "${detail}" ]]; then
        echo -e "${RED}✗${NC} (${detail})"
    else
        echo -e "${RED}✗${NC}"
    fi
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

# Check if a BOI upgrade is available (cached, non-blocking).
# Shows a one-line warning if behind remote. Caches result for 1 hour.
check_upgrade_available() {
    local src_dir="${BOI_STATE_DIR}/src"
    local cache_file="${BOI_STATE_DIR}/.upgrade-check"
    local cache_ttl=3600  # 1 hour in seconds

    # Skip if not a git repo
    [[ -d "${src_dir}/.git" ]] || return 0

    local need_refresh=true

    if [[ -f "${cache_file}" ]]; then
        local cache_ts cache_result
        cache_ts=$(head -1 "${cache_file}" 2>/dev/null || echo "0")
        cache_result=$(sed -n '2p' "${cache_file}" 2>/dev/null || echo "ok")
        local now
        now=$(date +%s)
        local age=$(( now - cache_ts ))

        # Show cached result (even if stale) so users see the warning
        if [[ "${cache_result}" =~ ^behind:([0-9]+)$ ]]; then
            echo ""
            echo -e "  ${YELLOW}Update available:${NC} ${BASH_REMATCH[1]} commit(s) behind. Run ${BOLD}boi upgrade${NC} to update."
        fi

        if [[ ${age} -lt ${cache_ttl} ]]; then
            need_refresh=false
        fi
    fi

    # Refresh in background if cache is stale or missing
    if [[ "${need_refresh}" == "true" ]]; then
        (
            if timeout 10 git -C "${src_dir}" fetch origin --quiet 2>/dev/null; then
                local local_head remote_head
                local_head=$(git -C "${src_dir}" rev-parse HEAD 2>/dev/null)
                remote_head=$(git -C "${src_dir}" rev-parse origin/main 2>/dev/null)
                if [[ -n "${local_head}" ]] && [[ -n "${remote_head}" ]]; then
                    local tmp="${cache_file}.tmp"
                    if [[ "${local_head}" == "${remote_head}" ]]; then
                        printf '%s\nok\n' "$(date +%s)" > "${tmp}"
                    else
                        local count
                        count=$(git -C "${src_dir}" rev-list HEAD..origin/main --count 2>/dev/null || echo "0")
                        printf '%s\nbehind:%s\n' "$(date +%s)" "${count}" > "${tmp}"
                    fi
                    mv "${tmp}" "${cache_file}"
                fi
            fi
        ) &>/dev/null &
        disown 2>/dev/null
    fi

    return 0
}

# resolve_queue_id — Find the most recent running or last-completed spec.
# Used by log, cancel, telemetry, spec when no queue-id is provided.
# Prints the queue-id and spec name to stderr for context, and the queue-id to stdout.
# Returns 1 if no spec can be resolved.
resolve_queue_id() {
    local result
    result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.queue_compat import get_queue

queue_dir = sys.argv[1]
entries = get_queue(queue_dir)

if not entries:
    sys.exit(1)

# Prefer: running > queued > needs_review > completed > failed > canceled
priority_order = {"running": 0, "queued": 1, "needs_review": 2, "completed": 3, "failed": 4, "canceled": 5}

# Sort by status priority, then by most recent activity
def sort_key(e):
    status = e.get("status", "unknown")
    status_rank = priority_order.get(status, 99)
    # Use last_iteration_at or submitted_at for recency
    ts = e.get("last_iteration_at") or e.get("submitted_at") or ""
    return (status_rank, ts)  # lower rank = better, later ts = better (but we reverse ts)

# Sort: best status first, then most recent within that status
sorted_entries = sorted(entries, key=lambda e: (
    priority_order.get(e.get("status", "unknown"), 99),
    -(hash(e.get("last_iteration_at") or e.get("submitted_at") or "")),
))

# If multiple specs are actively running, we can't auto-select
running = [e for e in entries if e.get("status") == "running"]
if len(running) > 1:
    ids = ", ".join(e["id"] for e in running)
    print(f"MULTIPLE:{ids}", end="")
    sys.exit(2)

best = sorted_entries[0]
spec_name = os.path.splitext(os.path.basename(best.get("original_spec_path", best.get("spec_path", ""))))[0]
print(f"{best['id']}|{spec_name}", end="")
PYEOF
    )

    local exit_code=$?

    if [[ ${exit_code} -eq 1 ]]; then
        return 1
    fi

    if [[ ${exit_code} -eq 2 ]]; then
        # Multiple running specs — caller must handle
        echo "${result}"
        return 2
    fi

    echo "${result}"
    return 0
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
    local after=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --spec)
                [[ -z "${2:-}" ]] && die_usage "--spec requires a file path"
                spec_file="$2"
                shift 2
                ;;
            --tasks)
                [[ -z "${2:-}" ]] && die_usage "--tasks requires a file path"
                tasks_file="$2"
                shift 2
                ;;
            --priority)
                [[ -z "${2:-}" ]] && die_usage "--priority requires a number"
                priority="$2"
                shift 2
                ;;
            --max-iter)
                [[ -z "${2:-}" ]] && die_usage "--max-iter requires a number"
                max_iter="$2"
                shift 2
                ;;
            --checkout|--worktree)
                [[ -z "${2:-}" ]] && die_usage "--worktree requires a path"
                worktree="$2"
                shift 2
                ;;
            --timeout)
                [[ -z "${2:-}" ]] && die_usage "--timeout requires seconds"
                timeout="$2"
                shift 2
                ;;
            --no-critic)
                no_critic=true
                shift
                ;;
            --mode|-m)
                [[ -z "${2:-}" ]] && die_usage "--mode requires a value (execute, challenge, discover, generate)"
                mode="$2"
                shift 2
                ;;
            --project)
                [[ -z "${2:-}" ]] && die_usage "--project requires a project name"
                project="$2"
                shift 2
                ;;
            --experiment-budget)
                [[ -z "${2:-}" ]] && die_usage "--experiment-budget requires a number"
                experiment_budget="$2"
                shift 2
                ;;
            --after)
                [[ -z "${2:-}" ]] && die_usage "--after requires one or more queue IDs (e.g. q-001 or q-001,q-002)"
                after="$2"
                shift 2
                ;;
            --dry-run)
                dry_run=true
                shift
                ;;
            -h|--help)
                echo "Usage: boi dispatch <spec.md> [--priority N] [--max-iter N] [--mode MODE] [--experiment-budget N] [--worktree <path>] [--timeout SECONDS] [--no-critic] [--project <name>] [--after q-NNN[,q-NNN...]] [--dry-run]"
                echo "       boi dispatch --spec <spec.md> [options]   (explicit flag form)"
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
                echo "  --after q-NNN     Block until q-NNN (or comma-separated list) completes"
                echo "  --dry-run         Validate and show what would be dispatched without enqueueing"
                exit 0
                ;;
            *)
                # Smart default: if first positional arg is a .md file, treat as --spec
                if [[ -z "${spec_file}" ]] && [[ -z "${tasks_file}" ]] && [[ "$1" == *.md ]] && [[ -f "$1" ]]; then
                    spec_file="$1"
                    shift
                else
                    die_usage "Unknown option: $1. Use 'boi dispatch --help' for usage."
                fi
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
    progress_step "Validating spec"
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
        progress_fail
        echo -e "${RED}Spec validation failed:${NC}" >&2
        echo "${validation_output}" >&2
        die "Fix the spec and try again."
    fi

    progress_done "${validation_output}"

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
        if [[ -n "${after}" ]]; then
            info "[dry-run] Blocked by: ${after}"
        fi
        exit 0
    fi

    # Enqueue the spec
    progress_step "Queuing"
    local worktree_arg=""
    if [[ -n "${worktree}" ]]; then
        worktree_arg="${worktree}"
    fi

    local result
    result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${input_file}" "${QUEUE_DIR}" "${priority}" "${max_iter}" "${worktree_arg}" "${timeout}" "${mode}" "${project}" "${experiment_budget}" "${after}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import dispatch
from lib.db import DuplicateSpecError

spec_path = sys.argv[1]
queue_dir = sys.argv[2]
priority = int(sys.argv[3])
max_iter = int(sys.argv[4])
checkout = sys.argv[5] if len(sys.argv) > 5 and sys.argv[5] else None
timeout_str = sys.argv[6] if len(sys.argv) > 6 and sys.argv[6] else None
mode = sys.argv[7] if len(sys.argv) > 7 and sys.argv[7] else "execute"
project_name = sys.argv[8] if len(sys.argv) > 8 and sys.argv[8] else None
experiment_budget_str = sys.argv[9] if len(sys.argv) > 9 and sys.argv[9] else None
after_str = sys.argv[10] if len(sys.argv) > 10 and sys.argv[10] else ""

# Parse --after CLI flag (comma-separated queue IDs)
blocked_by_cli = [d.strip() for d in after_str.split(",") if d.strip()] if after_str else []

# Parse **Blocked-By:** from spec file header (first 50 lines)
blocked_by_header = []
try:
    with open(spec_path, encoding="utf-8") as _f:
        for _i, _line in enumerate(_f):
            if _i >= 50:
                break
            _line = _line.strip()
            if _line.lower().startswith("**blocked-by:**"):
                _deps_str = _line.split(":**", 1)[1].strip()
                blocked_by_header = [d.strip() for d in _deps_str.split(",") if d.strip()]
                break
except Exception:
    pass

# Merge CLI + header (deduplicated, CLI listed first)
_seen = set()
blocked_by = []
for _dep in blocked_by_cli + blocked_by_header:
    if _dep not in _seen:
        _seen.add(_dep)
        blocked_by.append(_dep)

try:
    result = dispatch(
        queue_dir=queue_dir,
        spec_path=spec_path,
        priority=priority,
        max_iterations=max_iter,
        checkout=checkout,
        timeout=int(timeout_str) if timeout_str else None,
        mode=mode,
        project=project_name,
        experiment_budget=int(experiment_budget_str) if experiment_budget_str else None,
        blocked_by=blocked_by if blocked_by else None,
    )
    print(json.dumps(result))
except DuplicateSpecError as e:
    print(json.dumps({"error": "duplicate", "message": str(e)}))
    sys.exit(2)
except ValueError as e:
    print(json.dumps({"error": "validation", "message": str(e)}))
    sys.exit(3)
PYEOF
    )

    local enqueue_exit=$?

    if [[ ${enqueue_exit} -eq 2 ]]; then
        # Duplicate spec error
        progress_fail
        local dup_msg
        dup_msg=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['message'])")
        die "${dup_msg}"
    fi

    if [[ ${enqueue_exit} -eq 3 ]]; then
        # Dependency validation error
        progress_fail
        local val_msg
        val_msg=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['message'])")
        die "${val_msg}"
    fi

    if [[ ${enqueue_exit} -ne 0 ]]; then
        progress_fail
        die "Failed to enqueue spec."
    fi

    local queue_id
    local task_count
    local pending_count
    local enqueued_mode
    local blocked_by_ids
    queue_id=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['id'])")
    task_count=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['tasks'])")
    pending_count=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['pending'])")
    enqueued_mode=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('mode','execute'))")
    blocked_by_ids=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); bl=d.get('blocked_by',[]); print(', '.join(bl) if bl else '')")

    progress_done "${queue_id}, ${pending_count}/${task_count} tasks, priority ${priority}"

    if [[ -n "${blocked_by_ids}" ]]; then
        info "  blocked by: ${blocked_by_ids} (will not run until dependencies complete)"
    fi

    # Emit boi.spec.dispatched event to hex-events (if installed)
    local _hex_emit_py="${HOME}/.hex-events/hex_emit.py"
    if [[ -f "${_hex_emit_py}" ]]; then
        local _spec_title
        _spec_title=$(python3 -c "
import sys
try:
    with open(sys.argv[1], encoding='utf-8') as f:
        for line in f:
            s = line.strip()
            if s.startswith('# '):
                print(s[2:].strip())
                break
except Exception:
    pass
" "${input_file}" 2>/dev/null || true)
        python3 "${_hex_emit_py}" boi.spec.dispatched \
            "{\"queue_id\":\"${queue_id}\",\"spec_title\":\"${_spec_title}\",\"tasks_total\":${task_count},\"spec_path\":\"${input_file}\"}" \
            2>/dev/null || true
    fi

    # Start daemon if not already running
    if require_daemon; then
        progress_step "Starting daemon"
        progress_done "already running"
    else
        progress_step "Starting daemon"
        nohup python3 "${SCRIPT_DIR}/daemon.py" > "${LOG_DIR}/daemon-startup.log" 2>&1 < /dev/null &
        sleep 1
        if require_daemon; then
            progress_done
        else
            progress_fail "check ${LOG_DIR}/daemon-startup.log"
        fi
    fi

    echo ""
    info "Dispatched. Monitor with: boi status"

    # Check for available upgrades (cached, non-blocking)
    check_upgrade_available
}

# ─── Subcommand: queue ──────────────────────────────────────────────────────

cmd_queue() {
    local json_mode=false
    local watch_mode=false
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --json) json_mode=true; shift ;;
            --watch) watch_mode=true; shift ;;
            -h|--help)
                echo "Usage: boi queue [--json] [--watch]"
                echo ""
                echo "Options:"
                echo "  --json    Output machine-readable JSON"
                echo "  --watch   Auto-refresh every 2s"
                exit 0
                ;;
            *) die_usage "Unknown option: $1" ;;
        esac
    done

    require_config

    if [[ "${watch_mode}" == "true" ]]; then
        while true; do
            clear
            cmd_queue_inner "${json_mode}"
            sleep 2
        done
    fi

    cmd_queue_inner "${json_mode}"
}

# ─── Subcommand: status ──────────────────────────────────────────────────────

cmd_status() {
    local watch_mode=false
    local json_mode=false
    local sort_mode=""
    local filter_status=""
    local view_mode="default"   # default | all | running | recent:N

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --watch) watch_mode=true; shift ;;
            --json) json_mode=true; shift ;;
            --all) view_mode="all"; shift ;;
            --running) view_mode="running"; shift ;;
            --recent)
                if [[ $# -lt 2 ]]; then
                    die_usage "--recent requires a number"
                fi
                view_mode="recent:$2"; shift 2 ;;
            --recent=*) view_mode="recent:${1#--recent=}"; shift ;;
            --sort)
                if [[ $# -lt 2 ]]; then
                    die_usage "--sort requires a value (queue, status, progress, dag, name, recent)"
                fi
                sort_mode="$2"; shift 2 ;;
            --sort=*) sort_mode="${1#--sort=}"; shift ;;
            --filter)
                if [[ $# -lt 2 ]]; then
                    die_usage "--filter requires a value (all, running, queued, completed)"
                fi
                filter_status="$2"; shift 2 ;;
            --filter=*) filter_status="${1#--filter=}"; shift ;;
            -h|--help)
                echo "Usage: boi status [--watch] [--all] [--running] [--recent N] [--json] [--sort MODE] [--filter STATUS]"
                echo ""
                echo "Options:"
                echo "  --watch           Auto-refresh interactive dashboard"
                echo "  --all             Show all specs (default hides old completed/canceled)"
                echo "  --running         Show only running specs"
                echo "  --recent N        Show last N specs by activity"
                echo "  --json            Output machine-readable JSON"
                echo "  --sort MODE       Sort by: queue, status, progress, dag, name, recent"
                echo "  --filter STATUS   Filter by: all, running, queued, completed"
                echo ""
                echo "Examples:"
                echo "  boi status                          # Running + recent (last 24h)"
                echo "  boi status --all                    # All specs"
                echo "  boi status --running                # Only running specs"
                echo "  boi status --recent 10              # Last 10 specs"
                echo "  boi status --sort progress          # Sorted output"
                echo "  boi status --watch --sort dag       # Interactive with initial sort"
                exit 0
                ;;
            *) die_usage "Unknown option: $1" ;;
        esac
    done

    # Validate sort mode if provided
    if [[ -n "${sort_mode}" ]]; then
        case "${sort_mode}" in
            queue|status|progress|dag|name|recent) ;;
            *) die_usage "Invalid sort mode '${sort_mode}'. Must be: queue, status, progress, dag, name, recent" ;;
        esac
    fi

    # Validate filter status if provided
    if [[ -n "${filter_status}" ]]; then
        case "${filter_status}" in
            all|running|queued|completed) ;;
            *) die_usage "Invalid filter '${filter_status}'. Must be: all, running, queued, completed" ;;
        esac
    fi

    require_config

    if [[ "${watch_mode}" == "true" ]] && [ -t 1 ]; then
        # Flicker-free refresh: render to buffer, then overwrite screen in one shot
        # (watch -c doesn't support 24-bit true color, so we do it manually)
        # Only runs in an interactive TTY; non-TTY (pipes, tests) falls through to single render.
        tput civis 2>/dev/null || true  # hide cursor
        trap 'tput cnorm 2>/dev/null; printf "\033[?25h"; exit' INT TERM EXIT
        clear
        while true; do
            local buf
            if [[ -n "${sort_mode}" || -n "${filter_status}" ]]; then
                buf=$(cmd_queue_sorted "${sort_mode:-queue}" "${filter_status:-all}" "${view_mode}" 2>&1)
            else
                buf=$(cmd_queue_inner "${json_mode}" "${view_mode}" 2>&1)
            fi
            # Move cursor home + clear to end (no blank-screen flash)
            printf '\033[H\033[J%s\n' "$buf"
            sleep 2
        done
    fi

    # Non-interactive: if sort or filter specified, use dashboard format for richer output
    if [[ -n "${sort_mode}" || -n "${filter_status}" ]]; then
        cmd_queue_sorted "${sort_mode:-queue}" "${filter_status:-all}" "${view_mode}"
    else
        cmd_queue_inner "${json_mode}" "${view_mode}"
    fi

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

    # Check for available upgrades (cached, non-blocking)
    check_upgrade_available
}

# Inner queue display used by both cmd_queue and cmd_status
cmd_queue_inner() {
    local json_mode="${1:-false}"
    local view_mode="${2:-default}"

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${BOI_CONFIG}" "${json_mode}" "${view_mode}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.status import build_queue_status, format_queue_table, format_queue_json

queue_dir = sys.argv[1]
config_path = sys.argv[2]
json_mode = sys.argv[3] == "True" or sys.argv[3] == "true"
view_mode = sys.argv[4] if len(sys.argv) > 4 else "default"

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
    print(format_queue_table(status_data, view_mode=view_mode))
PYEOF
}

# Non-interactive sorted/filtered output using format_dashboard
cmd_queue_sorted() {
    local sort_mode="${1:-queue}"
    local filter_status="${2:-all}"
    local view_mode="${3:-default}"

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${BOI_CONFIG}" "${sort_mode}" "${filter_status}" "${view_mode}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.status import build_queue_status, format_dashboard

queue_dir = sys.argv[1]
config_path = sys.argv[2]
sort_mode = sys.argv[3]
filter_status = sys.argv[4]
view_mode = sys.argv[5] if len(sys.argv) > 5 else "default"

config = None
if os.path.isfile(config_path):
    try:
        with open(config_path) as f:
            config = json.load(f)
    except Exception:
        pass

status_data = build_queue_status(queue_dir, config)
output = format_dashboard(
    status_data,
    sort_mode=sort_mode,
    filter_status=filter_status,
    show_completed=True,
    selected_row=-1,
    view_mode=view_mode,
)

# Strip __QUEUE_IDS__ metadata line from output
for line in output.splitlines():
    if not line.startswith("__QUEUE_IDS__:"):
        print(line)
PYEOF
}

# ─── Subcommand: log ─────────────────────────────────────────────────────────

cmd_log() {
    local queue_id=""
    local full_mode=false
    local failures_mode=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --full) full_mode=true; shift ;;
            --failures) failures_mode=true; shift ;;
            -h|--help)
                echo "Usage: boi log [queue-id] [--full] [--failures]"
                echo ""
                echo "Options:"
                echo "  --full       Show full output (default: tail last 50 lines)"
                echo "  --failures   Show only failed iteration logs"
                echo ""
                echo "If no queue-id is given, shows logs for the most recent spec."
                exit 0
                ;;
            -*)
                die_usage "Unknown option: $1"
                ;;
            *)
                if [[ -z "${queue_id}" ]]; then
                    queue_id="$1"
                else
                    die_usage "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    require_config

    if [[ -z "${queue_id}" ]]; then
        # Auto-resolve to most recent spec
        local resolved
        resolved=$(resolve_queue_id) || true
        local resolve_exit=$?

        if [[ -z "${resolved}" ]]; then
            die "No specs in queue. Nothing to show logs for."
        fi

        if [[ "${resolved}" == MULTIPLE:* ]]; then
            local ids="${resolved#MULTIPLE:}"
            die "Multiple specs running (${ids}). Specify a queue-id: boi log <queue-id>"
        fi

        queue_id="${resolved%%|*}"
        local spec_name="${resolved#*|}"
        echo -e "${DIM}Showing log for ${queue_id} (${spec_name})...${NC}"
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

    # --failures mode: show tail of each failed iteration's log
    if [[ "${failures_mode}" == "true" ]]; then
        local found_failures=false
        for log_file in $(ls "${LOG_DIR}/${queue_id}"-iter-*.log 2>/dev/null | sort -t- -k4 -n); do
            if [[ ! -f "${log_file}" ]]; then
                continue
            fi
            local iter_num
            iter_num=$(echo "${log_file}" | sed -n "s/.*-iter-\([0-9]*\)\.log/\1/p")
            # Check if this iteration has a failure_reason in its metadata
            local iter_meta="${QUEUE_DIR}/${queue_id}.iteration-${iter_num}.json"
            if [[ -f "${iter_meta}" ]]; then
                local has_failure
                has_failure=$(python3 -c "
import json, sys
try:
    d = json.load(open('${iter_meta}'))
    if d.get('failure_reason') or d.get('crash'):
        print('yes')
    else:
        print('no')
except Exception:
    print('no')
" 2>/dev/null)
                if [[ "${has_failure}" == "yes" ]]; then
                    found_failures=true
                    local reason
                    reason=$(python3 -c "
import json
d = json.load(open('${iter_meta}'))
print(d.get('failure_reason', 'Unknown error'))
" 2>/dev/null)
                    echo -e "${RED}── Iteration ${iter_num}: ${reason}${NC}"
                    tail -n 20 "${log_file}"
                    echo ""
                fi
            fi
        done
        if [[ "${found_failures}" == "false" ]]; then
            echo "No failed iterations found for ${queue_id}."
        fi
        return
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
                echo "Usage: boi cancel [queue-id]"
                echo ""
                echo "Cancel a queued or running spec."
                echo "The spec will be marked as canceled and its worker released."
                echo ""
                echo "If no queue-id is given, cancels the most recent spec (with confirmation)."
                echo ""
                echo "Use 'boi queue' to see available queue IDs."
                exit 0
                ;;
            -*)
                die_usage "Unknown option: $1"
                ;;
            *)
                if [[ -z "${queue_id}" ]]; then
                    queue_id="$1"
                else
                    die_usage "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    require_config

    if [[ -z "${queue_id}" ]]; then
        # Auto-resolve to most recent running spec
        local resolved
        resolved=$(resolve_queue_id) || true

        if [[ -z "${resolved}" ]]; then
            die "No specs in queue. Nothing to cancel."
        fi

        if [[ "${resolved}" == MULTIPLE:* ]]; then
            local ids="${resolved#MULTIPLE:}"
            die "Multiple specs running (${ids}). Specify a queue-id: boi cancel <queue-id>"
        fi

        queue_id="${resolved%%|*}"
        local spec_name="${resolved#*|}"

        echo -e "Cancel ${BOLD}${queue_id}${NC} (${spec_name})? [y/N] "
        read -r confirm
        if [[ "${confirm}" != "y" ]] && [[ "${confirm}" != "Y" ]]; then
            info "Canceled."
            exit 0
        fi
    fi

    # Cancel in queue
    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import cancel_spec

queue_dir = sys.argv[1]
queue_id = sys.argv[2]

try:
    cancel_spec(queue_dir, queue_id)
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

# ─── Subcommand: resume ──────────────────────────────────────────────────────

cmd_resume() {
    local queue_id=""
    local all_mode=false

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help)
                echo "Usage: boi resume [queue-id | --all]"
                echo ""
                echo "Resume a failed or canceled spec."
                echo "Resets status to queued, clears failures, preserves progress."
                echo ""
                echo "Options:"
                echo "  --all    Resume ALL failed specs at once"
                echo ""
                echo "Use 'boi queue' to see available queue IDs."
                exit 0
                ;;
            --all)
                all_mode=true
                shift
                ;;
            -*)
                die_usage "Unknown option: $1"
                ;;
            *)
                if [[ -z "${queue_id}" ]]; then
                    queue_id="$1"
                else
                    die_usage "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    require_config

    if [[ "${all_mode}" == true ]]; then
        queue_id="--all"
    elif [[ -z "${queue_id}" ]]; then
        die_usage "Usage: boi resume <queue-id> or boi resume --all"
    fi

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import resume_spec

queue_dir = sys.argv[1]
queue_id = sys.argv[2]

try:
    resumed = resume_spec(queue_dir, queue_id)
    if queue_id == "--all":
        if resumed:
            print(f"Resumed {len(resumed)} spec(s): {', '.join(resumed)}")
        else:
            print("No failed specs to resume.")
    else:
        print(f"Spec '{queue_id}' resumed. Daemon will pick it up on next poll.")
except ValueError as e:
    print(f"Error: {e}", file=sys.stderr)
    sys.exit(1)
PYEOF
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
                die_usage "Unknown option: $1"
                ;;
            *)
                if [[ -z "${queue_id}" ]]; then
                    queue_id="$1"
                else
                    die_usage "Unexpected argument: $1"
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
                die_usage "Unknown option: $1. Use 'boi purge --help' for usage."
                ;;
        esac
    done

    require_config

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${LOG_DIR}" "${all_mode}" "${dry_run}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import purge_specs

queue_dir = sys.argv[1]
log_dir = sys.argv[2]
all_mode = sys.argv[3] == "true"
dry_run = sys.argv[4] == "true"

results = purge_specs(queue_dir, log_dir, all_mode=all_mode, dry_run=dry_run)

if not results:
    print("Nothing to purge.")
    sys.exit(0)

prefix = "[dry-run] Would remove" if dry_run else "Purging"
print(f"{prefix} {len(results)} spec(s)...")
for r in results:
    file_count = len(r["files_removed"])
    spec_name = r.get("spec_name", "")
    name_part = f" {spec_name}" if spec_name else ""
    task_count = r.get("tasks_total", "?")
    iteration = r.get("iteration", "?")
    print(f"  {r['id']}{name_part} ({task_count} tasks, {iteration} iterations)")

if dry_run:
    total = sum(len(r["files_removed"]) for r in results)
    print(f"\nWould remove {len(results)} spec(s), {total} file(s).")
else:
    total = sum(len(r["files_removed"]) for r in results)
    print(f"\nRemoved {len(results)} spec(s), {total} file(s) total.")
PYEOF
}

# ─── Subcommand: migrate-db ───────────────────────────────────────────────────

cmd_migrate_db() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help)
                echo "Usage: boi migrate-db"
                echo ""
                echo "Migrate JSON queue and event files to SQLite."
                echo "Archives original JSON files to queue/archive/ and events/archive/."
                echo "Inverse of export-db."
                exit 0
                ;;
            *)
                die_usage "Unknown option: $1. Use 'boi migrate-db --help' for usage."
                ;;
        esac
    done

    require_config

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${EVENTS_DIR}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import migrate_db

queue_dir = sys.argv[1]
events_dir = sys.argv[2]
result = migrate_db(queue_dir, events_dir)

specs = result.get("specs", 0)
events = result.get("events", 0)
total = specs + events

if total == 0:
    print("Nothing to migrate.")
else:
    print(f"Migrated {specs} spec(s) and {events} event(s) to SQLite.")
PYEOF
}

# ─── Subcommand: export-db ────────────────────────────────────────────────────

cmd_export_db() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help)
                echo "Usage: boi export-db"
                echo ""
                echo "Export all specs from SQLite to q-NNN.json files."
                echo "Inverse of migrate-db. Enables rollback to bash daemon."
                exit 0
                ;;
            *)
                die_usage "Unknown option: $1. Use 'boi export-db --help' for usage."
                ;;
        esac
    done

    require_config

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import export_db

queue_dir = sys.argv[1]
count = export_db(queue_dir)

if count == 0:
    print("No specs to export.")
else:
    print(f"Exported {count} spec(s) to JSON.")
PYEOF
}

# ─── Subcommand: stop ────────────────────────────────────────────────────────

# ─── Subcommand: daemon ───────────────────────────────────────────────────────

cmd_daemon() {
    if [[ $# -eq 0 ]]; then
        echo "Usage: boi daemon <subcommand>"
        echo ""
        echo "Subcommands:"
        echo "  status [--json]   Show daemon status (running/stopped, PID, uptime)"
        return 0
    fi

    local subcmd="$1"
    shift

    case "${subcmd}" in
        status)
            local json_flag=false
            while [[ $# -gt 0 ]]; do
                case "$1" in
                    --json) json_flag=true; shift ;;
                    *) shift ;;
                esac
            done

            BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${BOI_STATE_DIR}" "${json_flag}" <<'PYEOF'
import json
import os
import sys
sys.path.insert(0, os.environ.get("BOI_SCRIPT_DIR", os.path.dirname(__file__)))
from lib.daemon_lock import daemon_status

state_dir = sys.argv[1]
as_json = sys.argv[2] == "true"

status = daemon_status(state_dir)

if as_json:
    print(json.dumps(status, indent=2))
else:
    if status.get("running"):
        pid = status.get("pid", "?")
        uptime = status.get("uptime")
        uptime_str = ""
        if uptime is not None:
            hours = int(uptime // 3600)
            minutes = int((uptime % 3600) // 60)
            if hours > 0:
                uptime_str = f" (uptime: {hours}h {minutes}m)"
            else:
                uptime_str = f" (uptime: {minutes}m)"
        print(f"Daemon: running (PID {pid}){uptime_str}")
    else:
        print("Daemon: stopped")
PYEOF
            ;;
        *)
            echo "Unknown daemon subcommand: ${subcmd}"
            echo "Usage: boi daemon status [--json]"
            return 1
            ;;
    esac
}

# ─── Subcommand: stop ────────────────────────────────────────────────────────

cmd_stop() {
    require_config

    local force=false
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --force) force=true; shift ;;
            *) shift ;;
        esac
    done

    # Kill tracked worker PIDs from the DB
    local force_flag=""
    if ${force}; then
        force_flag="force=True"
    else
        force_flag="force=False"
    fi
    local db_killed
    db_killed=$(python3 -c "
import sys; sys.path.insert(0, '${BOI_SRC_DIR}')
from lib.cli_ops import stop_all_workers
killed = stop_all_workers('${QUEUE_DIR}', ${force_flag})
print(len(killed))
" 2>/dev/null || echo "0")
    if [[ "${db_killed}" -gt 0 ]]; then
        info "Killed ${db_killed} tracked worker PID(s) from DB."
    fi

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

# ─── Subcommand: cleanup ─────────────────────────────────────────────────────

cmd_cleanup() {
    require_config

    info "Scanning for orphaned BOI worker processes..."
    local orphan_count
    orphan_count=$(python3 -c "
import sys; sys.path.insert(0, '${BOI_SRC_DIR}')
from lib.cli_ops import cleanup_orphans
orphans = cleanup_orphans('${QUEUE_DIR}')
for pid in orphans:
    print(f'  Killed orphaned process: PID {pid}')
print(len(orphans))
" 2>&1)

    # Last line is the count
    local count
    count=$(echo "${orphan_count}" | tail -1)
    # Print the kill lines (all but last)
    echo "${orphan_count}" | head -n -1

    if [[ "${count}" -gt 0 ]]; then
        info "Cleaned up ${count} orphaned process(es)."
    else
        info "No orphaned processes found."
    fi
}

# ─── Subcommand: upgrade ─────────────────────────────────────────────────────

cmd_upgrade() {
    local force=false
    local check_only=false
    local no_plugin=false
    local src_dir="${BOI_STATE_DIR}/src"

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --force)     force=true; shift ;;
            --check)     check_only=true; shift ;;
            --no-plugin) no_plugin=true; shift ;;
            -h|--help)
                echo "Usage: boi upgrade [--check] [--force] [--no-plugin]"
                echo ""
                echo "Update BOI to the latest version."
                echo ""
                echo "Options:"
                echo "  --check       Check if an update is available without upgrading"
                echo "  --force       Stop daemon and workers immediately, don't wait"
                echo "  --no-plugin   Skip updating the Claude Code plugin"
                exit 0
                ;;
            *) die_usage "Unknown option: $1. Use 'boi upgrade --help' for usage." ;;
        esac
    done

    # 1. Pre-flight: verify ~/.boi/src/ is a git repo
    if [[ ! -d "${src_dir}/.git" ]]; then
        die "BOI source at ${src_dir} is not a git repo. Reinstall with: curl -fsSL https://raw.githubusercontent.com/mrap/boi/main/install-public.sh | bash"
    fi

    echo -e "${BOLD}BOI Upgrade${NC}"
    echo ""

    # 2. Fetch latest from remote
    progress_step "Checking for updates"
    if ! git -C "${src_dir}" fetch origin 2>/dev/null; then
        progress_fail
        die "Cannot reach remote. Check your network connection."
    fi

    local old_commit new_remote_commit
    old_commit=$(git -C "${src_dir}" rev-parse HEAD)
    new_remote_commit=$(git -C "${src_dir}" rev-parse origin/main 2>/dev/null || true)

    if [[ -z "${new_remote_commit}" ]]; then
        progress_fail
        die "Could not resolve origin/main."
    fi

    if [[ "${old_commit}" == "${new_remote_commit}" ]]; then
        progress_done "already up to date"
        echo ""
        local short_commit="${old_commit:0:7}"
        info "BOI is up to date (v${BOI_VERSION}, ${short_commit})."
        return 0
    fi

    local commit_count
    commit_count=$(git -C "${src_dir}" rev-list HEAD..origin/main --count 2>/dev/null || echo "?")
    progress_done "${commit_count} new commit(s)"

    # Show what's coming
    echo ""
    echo -e "${BOLD}New commits:${NC}"
    git -C "${src_dir}" log --oneline HEAD..origin/main 2>/dev/null | head -20
    echo ""

    # --check: just report and exit
    if [[ "${check_only}" == "true" ]]; then
        local old_short="${old_commit:0:7}"
        local new_short="${new_remote_commit:0:7}"
        info "Current: v${BOI_VERSION} (${old_short})"
        info "Latest:  ${new_short}"
        info "${commit_count} commit(s) behind. Run 'boi upgrade' to update."
        return 0
    fi

    # 3. Stop daemon (if running)
    local daemon_was_running=false
    if require_daemon; then
        daemon_was_running=true

        if [[ "${force}" == "true" ]]; then
            progress_step "Stopping daemon (force)"
            cmd_stop
            progress_done
        else
            # Wait for active worker iterations to finish
            local active_workers=0
            local sessions
            sessions=$(tmux -L boi list-sessions -F '#{session_name}' 2>/dev/null || true)
            if [[ -n "${sessions}" ]]; then
                while IFS= read -r session; do
                    if [[ "${session}" == boi-* ]]; then
                        active_workers=$((active_workers + 1))
                    fi
                done <<< "${sessions}"
            fi

            if [[ ${active_workers} -gt 0 ]]; then
                progress_step "Waiting for ${active_workers} active worker(s) to finish"
                local waited=0
                local timeout=120
                while [[ ${waited} -lt ${timeout} ]]; do
                    sleep 5
                    waited=$((waited + 5))
                    # Re-check active workers
                    active_workers=0
                    sessions=$(tmux -L boi list-sessions -F '#{session_name}' 2>/dev/null || true)
                    if [[ -n "${sessions}" ]]; then
                        while IFS= read -r session; do
                            if [[ "${session}" == boi-* ]]; then
                                active_workers=$((active_workers + 1))
                            fi
                        done <<< "${sessions}"
                    fi
                    if [[ ${active_workers} -eq 0 ]]; then
                        break
                    fi
                done
                if [[ ${active_workers} -gt 0 ]]; then
                    progress_fail "timed out after ${timeout}s"
                    warn "Proceeding with daemon stop. Active workers will be terminated."
                else
                    progress_done
                fi
            fi

            progress_step "Stopping daemon"
            cmd_stop
            progress_done
        fi
    fi

    # 4. Pull code
    progress_step "Pulling latest code"
    if ! git -C "${src_dir}" pull --ff-only origin main 2>/dev/null; then
        # ff-only failed (diverged), force reset — safe since ~/.boi/src/ is read-only install
        warn "Fast-forward failed. Resetting to origin/main."
        git -C "${src_dir}" reset --hard origin/main 2>/dev/null
    fi

    local new_commit
    new_commit=$(git -C "${src_dir}" rev-parse HEAD)
    progress_done "${old_commit:0:7} → ${new_commit:0:7}"

    # 5. Update Claude Code plugin (unless --no-plugin)
    if [[ "${no_plugin}" == "false" ]]; then
        local claude_dir="${HOME}/.claude"
        if [[ -d "${claude_dir}" ]]; then
            progress_step "Updating Claude Code plugin"
            mkdir -p "${claude_dir}/skills/boi" "${claude_dir}/commands"
            if [[ -f "${src_dir}/plugin/skills/boi/SKILL.md" ]]; then
                cp "${src_dir}/plugin/skills/boi/SKILL.md" "${claude_dir}/skills/boi/SKILL.md"
            fi
            if [[ -f "${src_dir}/plugin/commands/boi.md" ]]; then
                cp "${src_dir}/plugin/commands/boi.md" "${claude_dir}/commands/boi.md"
            fi
            progress_done
        fi
    fi

    # 6. Restart daemon (if it was running)
    if [[ "${daemon_was_running}" == "true" ]]; then
        progress_step "Restarting daemon with upgraded code"
        # start_daemon() uses SCRIPT_DIR which now points to updated ~/.boi/src/
        nohup python3 "${src_dir}/daemon.py" > "${LOG_DIR}/daemon-startup.log" 2>&1 < /dev/null &
        sleep 1
        if require_daemon; then
            progress_done
        else
            progress_fail "check ${LOG_DIR}/daemon-startup.log"
        fi
    fi

    # 7. Report
    echo ""
    echo -e "${BOLD}Changes:${NC}"
    git -C "${src_dir}" diff --stat "${old_commit}..${new_commit}" 2>/dev/null | tail -5
    echo ""
    # Clear upgrade check cache so the warning disappears
    rm -f "${BOI_STATE_DIR}/.upgrade-check"

    info "BOI upgraded successfully! (${old_commit:0:7} → ${new_commit:0:7})"
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
            *) die_usage "Unknown option: $1" ;;
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

GREEN = "\033[38;2;64;160;43m"
YELLOW = "\033[38;2;223;142;29m"
RED = "\033[38;2;210;15;57m"
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
                echo "Usage: boi telemetry [queue-id] [--json]"
                echo ""
                echo "If no queue-id is given, shows telemetry for the most recent spec."
                exit 0
                ;;
            -*)
                die_usage "Unknown option: $1"
                ;;
            *)
                if [[ -z "${queue_id}" ]]; then
                    queue_id="$1"
                else
                    die_usage "Unexpected argument: $1"
                fi
                shift
                ;;
        esac
    done

    if [[ -z "${queue_id}" ]]; then
        # Auto-resolve to most recent spec
        local resolved
        resolved=$(resolve_queue_id) || true

        if [[ -z "${resolved}" ]]; then
            die "No specs in queue. Nothing to show telemetry for."
        fi

        if [[ "${resolved}" == MULTIPLE:* ]]; then
            local ids="${resolved#MULTIPLE:}"
            die "Multiple specs running (${ids}). Specify a queue-id: boi telemetry <queue-id>"
        fi

        queue_id="${resolved%%|*}"
        local spec_name="${resolved#*|}"
        echo -e "${DIM}Showing telemetry for ${queue_id} (${spec_name})...${NC}"
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
    local json_mode=false
    # Handle flags before running checks
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --json) json_mode=true; shift ;;
            -h|--help)
                echo "Usage: boi doctor [--json]"
                echo ""
                echo "Check prerequisites and environment health."
                echo ""
                echo "Options:"
                echo "  --json    Output machine-readable JSON"
                echo ""
                echo "Checks:"
                echo "  tmux, claude CLI, Python version"
                echo "  State directory, config, workers"
                echo "  Daemon status, heartbeat"
                echo "  Critic configuration"
                exit 0
                ;;
            *) die_usage "Unknown option: $1. Use 'boi doctor --help' for usage." ;;
        esac
    done

    # Collect results into arrays for JSON output
    local -a check_names=()
    local -a check_statuses=()
    local -a check_details=()
    local -a check_fixes=()

    if [[ "${json_mode}" == "false" ]]; then
        echo -e "${BOLD}BOI Doctor${NC}"
        echo ""
    fi

    local pass_count=0
    local fail_count=0
    local warn_count=0

    _doctor_pass() {
        check_names+=("$1")
        check_statuses+=("pass")
        check_details+=("$1")
        check_fixes+=("")
        if [[ "${json_mode}" == "false" ]]; then
            echo -e "  ${GREEN}[PASS]${NC} $1"
        fi
        pass_count=$((pass_count + 1))
    }

    _doctor_fail() {
        check_names+=("$1")
        check_statuses+=("fail")
        check_details+=("$1")
        check_fixes+=("${2:-}")
        if [[ "${json_mode}" == "false" ]]; then
            echo -e "  ${RED}[FAIL]${NC} $1"
            if [[ -n "${2:-}" ]]; then
                echo -e "         Fix: $2"
            fi
        fi
        fail_count=$((fail_count + 1))
    }

    _doctor_warn() {
        check_names+=("$1")
        check_statuses+=("warn")
        check_details+=("$1")
        check_fixes+=("${2:-}")
        if [[ "${json_mode}" == "false" ]]; then
            echo -e "  ${YELLOW}[WARN]${NC} $1"
            if [[ -n "${2:-}" ]]; then
                echo -e "         Fix: $2"
            fi
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

    # 9. daemon.py exists
    if [[ -f "${SCRIPT_DIR}/daemon.py" ]]; then
        _doctor_pass "daemon.py found"
    else
        _doctor_fail "daemon.py not found in ${SCRIPT_DIR}" "Reinstall BOI or check your installation"
    fi

    # 10. Daemon PID file check (Python daemon)
    if [[ -f "${PID_FILE}" ]]; then
        local daemon_pid
        daemon_pid=$(cat "${PID_FILE}" 2>/dev/null || true)
        if [[ -n "${daemon_pid}" ]] && kill -0 "${daemon_pid}" 2>/dev/null; then
            # Verify it's a Python daemon process (not legacy bash)
            local daemon_cmd
            daemon_cmd=$(ps -p "${daemon_pid}" -o args= 2>/dev/null || true)
            if [[ "${daemon_cmd}" == *"python"*"daemon.py"* ]]; then
                _doctor_pass "Python daemon running (PID ${daemon_pid})"
            elif [[ "${daemon_cmd}" == *"daemon.sh"* ]]; then
                _doctor_warn "Legacy bash daemon running (PID ${daemon_pid})" "Stop and restart: 'boi stop && boi dispatch --spec <spec>'"
            else
                _doctor_pass "Daemon running (PID ${daemon_pid})"
            fi
        else
            _doctor_warn "Daemon not running (stale PID file)" "Run 'boi dispatch' to start the daemon"
        fi
    else
        _doctor_warn "Daemon not running" "Run 'boi dispatch' to start the daemon"
    fi

    # 11. Daemon heartbeat check
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

    # 12. SQLite database check
    local db_file="${BOI_STATE_DIR}/boi.db"
    if [[ -f "${db_file}" ]]; then
        local db_status
        db_status=$(python3 -c "
import sqlite3, sys
try:
    conn = sqlite3.connect('${db_file}')
    tables = [r[0] for r in conn.execute(\"SELECT name FROM sqlite_master WHERE type='table'\").fetchall()]
    required = {'specs', 'workers', 'processes', 'iterations', 'events'}
    missing = required - set(tables)
    if missing:
        print(f'WARN|missing tables: {\", \".join(sorted(missing))}')
    else:
        spec_count = conn.execute('SELECT COUNT(*) FROM specs').fetchone()[0]
        print(f'PASS|{len(tables)} tables, {spec_count} specs')
    conn.close()
except Exception as e:
    print(f'FAIL|{e}')
" 2>/dev/null || echo "FAIL|could not check database")
        local db_level="${db_status%%|*}"
        local db_detail="${db_status#*|}"
        case "${db_level}" in
            PASS) _doctor_pass "SQLite database: ${db_detail}" ;;
            WARN) _doctor_warn "SQLite database: ${db_detail}" "Run 'boi migrate-db' to initialize" ;;
            *) _doctor_fail "SQLite database: ${db_detail}" "Check ~/.boi/boi.db" ;;
        esac
    else
        _doctor_warn "SQLite database not found (~/.boi/boi.db)" "Database will be created on first dispatch"
    fi

    # 13. Critic configuration
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

    if [[ "${json_mode}" == "true" ]]; then
        # Build JSON output
        python3 -c "
import json, sys
names = sys.argv[1].split('|') if sys.argv[1] else []
statuses = sys.argv[2].split('|') if sys.argv[2] else []
fixes = sys.argv[3].split('|') if sys.argv[3] else []
checks = []
for i in range(len(names)):
    entry = {'check': names[i], 'status': statuses[i] if i < len(statuses) else 'unknown'}
    if i < len(fixes) and fixes[i]:
        entry['fix'] = fixes[i]
    checks.append(entry)
result = {'pass': int(sys.argv[4]), 'fail': int(sys.argv[5]), 'warn': int(sys.argv[6]), 'checks': checks}
print(json.dumps(result, indent=2))
" "$(IFS='|'; echo "${check_names[*]}")" "$(IFS='|'; echo "${check_statuses[*]}")" "$(IFS='|'; echo "${check_fixes[*]}")" "${pass_count}" "${fail_count}" "${warn_count}"
    else
        echo ""
        echo -e "Results: ${GREEN}${pass_count} passed${NC}, ${RED}${fail_count} failed${NC}, ${YELLOW}${warn_count} warning(s)${NC}"
    fi
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
            die_usage "Unknown critic subcommand: ${subcommand}. Use 'boi critic --help' for usage."
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
    local json_mode=false
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --json) json_mode=true; shift ;;
            -h|--help)
                echo "Usage: boi critic status [--json]"
                echo ""
                echo "Show critic configuration, active checks, and pass counts."
                echo ""
                echo "Options:"
                echo "  --json    Output machine-readable JSON"
                exit 0
                ;;
            *) die_usage "Unknown option: $1" ;;
        esac
    done

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${BOI_STATE_DIR}" "${QUEUE_DIR}" "${SCRIPT_DIR}" "${json_mode}" <<'PYEOF'
import json
import os
import sys

sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.critic_config import load_critic_config, get_active_checks

state_dir = sys.argv[1]
queue_dir = sys.argv[2]
boi_dir = sys.argv[3]
json_mode = sys.argv[4] == "true" or sys.argv[4] == "True"

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

# Collect critic pass info for active specs
pass_entries = []
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
                pass_entries.append((entry.get("id", "?"), entry.get("status", "?"), passes))
        except (json.JSONDecodeError, OSError):
            continue

if json_mode:
    result = {
        "enabled": enabled,
        "trigger": trigger,
        "max_passes": max_passes,
        "checks": [{"name": c["name"], "source": c["source"]} for c in checks],
        "default_checks": default_count,
        "custom_checks": custom_count,
        "prompt_override": prompt_override,
    }
    if pass_entries:
        result["spec_passes"] = [{"id": qid, "status": status, "passes": passes} for qid, status, passes in pass_entries]
    print(json.dumps(result, indent=2))
else:
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

    if pass_entries:
        print()
        print("  Critic passes:")
        for qid, status, passes in pass_entries:
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
                die_usage "Unknown option: $1"
                ;;
            *)
                if [[ -z "${queue_id}" ]]; then
                    queue_id="$1"
                else
                    die_usage "Unexpected argument: $1"
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
from lib.queue_compat import get_entry

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
    echo "Usage: boi spec [queue-id] [subcommand] [options]"
    echo ""
    echo "If no queue-id is given, uses the most recent spec."
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

    if [[ $# -eq 0 ]]; then
        # No args: auto-resolve queue-id and show tasks
        local resolved
        resolved=$(resolve_queue_id) || true

        if [[ -z "${resolved}" ]]; then
            _spec_usage
            exit 0
        fi

        if [[ "${resolved}" == MULTIPLE:* ]]; then
            local ids="${resolved#MULTIPLE:}"
            die "Multiple specs running (${ids}). Specify a queue-id: boi spec <queue-id>"
        fi

        queue_id="${resolved%%|*}"
        local spec_name="${resolved#*|}"
        echo -e "${DIM}Showing spec for ${queue_id} (${spec_name})...${NC}"

        local spec_file="${QUEUE_DIR}/${queue_id}.spec.md"
        if [[ ! -f "${spec_file}" ]]; then
            die "Spec file not found: ${spec_file}."
        fi
        _spec_show "${spec_file}" false
        return
    fi

    if [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]] || [[ "$1" == "help" ]]; then
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
            die_usage "Unknown spec subcommand: ${subcommand}. Use 'boi spec --help' for usage."
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
    symbols = {"DONE": "\033[38;2;64;160;43m✓\033[0m", "PENDING": "\033[38;2;223;142;29m○\033[0m", "SKIPPED": "\033[38;2;108;111;133m—\033[0m"}
    next_pending_found = False
    for t in tasks:
        sym = symbols.get(t.status, "?")
        marker = ""
        if t.status == "PENDING" and not next_pending_found:
            marker = " \033[38;2;223;142;29m← next\033[0m"
            next_pending_found = True
        # Check for blocked-by
        blocked = ""
        for line in t.body.splitlines():
            if line.strip().startswith("**Blocked by:**"):
                deps = line.strip().split("**Blocked by:**")[1].strip()
                blocked = f" \033[38;2;108;111;133m[blocked by: {deps}]\033[0m"
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
                die_usage "Unknown option for 'add': $1"
                ;;
            *)
                if [[ -z "${title}" ]]; then
                    title="$1"
                else
                    die_usage "Unexpected argument: $1"
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
                die_usage "Unknown option for 'skip': $1"
                ;;
            *)
                if [[ -z "${task_id}" ]]; then
                    task_id="$1"
                else
                    die_usage "Unexpected argument: $1"
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
            -*) die_usage "Unknown option for 'next': $1" ;;
            *)
                if [[ -z "${task_id}" ]]; then
                    task_id="$1"
                else
                    die_usage "Unexpected argument: $1"
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
            -*) die_usage "Unknown option for 'block': $1" ;;
            *)
                if [[ -z "${task_id}" ]]; then
                    task_id="$1"
                else
                    die_usage "Unexpected argument: $1"
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
            -*) die_usage "Unknown option for 'edit': $1" ;;
            *)
                if [[ -z "${task_id}" ]]; then
                    task_id="$1"
                else
                    die_usage "Unexpected argument: $1"
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
            die_usage "Unknown project subcommand: ${subcommand}. Use 'boi project --help' for usage."
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
            -*) die_usage "Unknown option: $1" ;;
            *)
                if [[ -z "${name}" ]]; then
                    name="$1"; shift
                else
                    die_usage "Unexpected argument: $1"
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
            -*) die_usage "Unknown option: $1" ;;
            *) die_usage "Unexpected argument: $1" ;;
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
            -*) die_usage "Unknown option: $1" ;;
            *)
                if [[ -z "${name}" ]]; then
                    name="$1"; shift
                else
                    die_usage "Unexpected argument: $1"
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
            -*) die_usage "Unknown option: $1" ;;
            *)
                if [[ -z "${name}" ]]; then
                    name="$1"; shift
                else
                    die_usage "Unexpected argument: $1"
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
            -*) die_usage "Unknown option: $1" ;;
            *)
                if [[ -z "${name}" ]]; then
                    name="$1"; shift
                else
                    die_usage "Unexpected argument: $1"
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
            -*) die_usage "Unknown option: $1" ;;
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
    echo "  resume      Resume a failed/canceled spec (preserves progress)"
    echo "  stop        Stop daemon and all workers"
    echo "  workers     Show worker/worktree status"
    echo "  telemetry   Show per-iteration breakdown"
    echo "  review      Review experiment proposals on a paused spec"
    echo "  purge       Remove completed/failed/canceled specs"
    echo "  critic      Manage the critic validation system"
    echo "  spec        Live spec management (add, skip, reorder, block tasks)"
    echo "  project     Manage projects (create, list, status, context, delete)"
    echo "  do          Translate natural language into BOI commands"
    echo "  upgrade     Update BOI to the latest version"
    echo "  doctor      Check prerequisites and environment health"
    echo "  dashboard   Live-updating queue progress"
    echo ""
    echo "Examples:"
    echo "  boi install                         # one-time setup"
    echo "  boi dispatch spec.md                # submit a spec"
    echo "  boi dispatch spec.md --priority 50  # high priority"
    echo "  boi dispatch --spec s.md            # explicit flag form"
    echo "  boi queue                           # view queue"
    echo "  boi status                          # check progress"
    echo "  boi status --watch                  # live dashboard"
    echo "  boi log                             # tail most recent spec"
    echo "  boi log q-001                       # tail specific spec"
    echo "  boi log q-001 --full                # full output"
    echo "  boi log q-001 --failures            # show only failed iterations"
    echo "  boi review q-001                    # review experiments"
    echo "  boi cancel                          # cancel most recent"
    echo "  boi cancel q-001                    # cancel specific spec"
    echo "  boi stop                            # stop everything"
    echo "  boi workers                         # show worktrees"
    echo "  boi telemetry                       # iteration breakdown"
    echo "  boi spec                            # show tasks"
    echo "  boi spec q-001 add \"Fix tests\"      # add a task"
    echo "  boi purge                           # clean finished specs"
    echo "  boi purge --dry-run                 # preview purge"
    echo "  boi critic status                   # show critic config"
    echo "  boi spec q-001                      # show spec tasks"
    echo "  boi spec q-001 add \"Fix tests\"      # add a task"
    echo "  boi project create my-app            # create a project"
    echo "  boi project list                     # list projects"
    echo "  boi do \"show me what's running\"       # natural language"
    echo "  boi do --dry-run \"cancel stuck specs\" # preview commands"
    echo "  boi upgrade                         # update to latest"
    echo "  boi upgrade --check                 # check for updates"
    echo "  boi doctor                          # check prerequisites"
    echo "  boi --version                       # show version"
}

# ─── Subcommand: dep ────────────────────────────────────────────────────────

cmd_dep() {
    if [[ $# -eq 0 ]] || [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]]; then
        echo "Usage: boi dep <add|remove|list> <spec-id> --after <dep-id>[,<dep-id>...]"
        echo ""
        echo "Commands:"
        echo "  add    <spec-id> --after <dep-id>   Add dependency (with cycle detection)"
        echo "  remove <spec-id> --after <dep-id>   Remove dependency"
        echo "  list   <spec-id>                    List dependencies for a spec"
        exit 0
    fi

    local subcommand="$1"
    shift

    case "${subcommand}" in
        add)    _dep_add "$@" ;;
        remove) _dep_remove "$@" ;;
        list)   _dep_list "$@" ;;
        -h|--help) cmd_dep --help ;;
        *)
            die_usage "Unknown dep subcommand: ${subcommand}. Use 'boi dep --help'."
            ;;
    esac
}

_dep_add() {
    local spec_id=""
    local after=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --after)
                [[ -z "${2:-}" ]] && die_usage "--after requires one or more queue IDs"
                after="$2"
                shift 2
                ;;
            -h|--help)
                echo "Usage: boi dep add <spec-id> --after <dep-id>[,<dep-id>...]"
                exit 0
                ;;
            *)
                if [[ -z "${spec_id}" ]]; then
                    spec_id="$1"
                    shift
                else
                    die_usage "Unexpected argument: $1"
                fi
                ;;
        esac
    done

    [[ -z "${spec_id}" ]] && die_usage "spec-id required. Usage: boi dep add <spec-id> --after <dep-id>"
    [[ -z "${after}" ]] && die_usage "--after required. Usage: boi dep add <spec-id> --after <dep-id>"

    local result
    result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${spec_id}" "${after}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import add_dependency

queue_dir = sys.argv[1]
spec_id = sys.argv[2]
after_str = sys.argv[3]
dep_ids = [d.strip() for d in after_str.split(",") if d.strip()]

try:
    result = add_dependency(queue_dir, spec_id, dep_ids)
    print(json.dumps(result))
except ValueError as e:
    print(json.dumps({"error": str(e)}))
    sys.exit(1)
PYEOF
    )

    local exit_code=$?
    if [[ ${exit_code} -ne 0 ]]; then
        local err_msg
        err_msg=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('error','unknown error'))" 2>/dev/null || echo "Failed to add dependency")
        die "${err_msg}"
    fi

    local added_ids
    added_ids=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(', '.join(d.get('added',[])))" 2>/dev/null)
    info "${spec_id} now blocked by: ${added_ids}"
}

_dep_remove() {
    local spec_id=""
    local after=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --after)
                [[ -z "${2:-}" ]] && die_usage "--after requires one or more queue IDs"
                after="$2"
                shift 2
                ;;
            -h|--help)
                echo "Usage: boi dep remove <spec-id> --after <dep-id>[,<dep-id>...]"
                exit 0
                ;;
            *)
                if [[ -z "${spec_id}" ]]; then
                    spec_id="$1"
                    shift
                else
                    die_usage "Unexpected argument: $1"
                fi
                ;;
        esac
    done

    [[ -z "${spec_id}" ]] && die_usage "spec-id required. Usage: boi dep remove <spec-id> --after <dep-id>"
    [[ -z "${after}" ]] && die_usage "--after required. Usage: boi dep remove <spec-id> --after <dep-id>"

    local result
    result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${spec_id}" "${after}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import remove_dependency

queue_dir = sys.argv[1]
spec_id = sys.argv[2]
after_str = sys.argv[3]
dep_ids = [d.strip() for d in after_str.split(",") if d.strip()]

try:
    result = remove_dependency(queue_dir, spec_id, dep_ids)
    print(json.dumps(result))
except ValueError as e:
    print(json.dumps({"error": str(e)}))
    sys.exit(1)
PYEOF
    )

    local exit_code=$?
    if [[ ${exit_code} -ne 0 ]]; then
        local err_msg
        err_msg=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('error','unknown error'))" 2>/dev/null || echo "Failed to remove dependency")
        die "${err_msg}"
    fi

    local removed_ids
    removed_ids=$(echo "${result}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(', '.join(d.get('removed',[])))" 2>/dev/null)
    info "Removed dependencies from ${spec_id}: ${removed_ids}"
}

_dep_list() {
    local spec_id=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help)
                echo "Usage: boi dep list <spec-id>"
                exit 0
                ;;
            *)
                if [[ -z "${spec_id}" ]]; then
                    spec_id="$1"
                    shift
                else
                    die_usage "Unexpected argument: $1"
                fi
                ;;
        esac
    done

    [[ -z "${spec_id}" ]] && die_usage "spec-id required. Usage: boi dep list <spec-id>"

    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${spec_id}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.db import Database
from pathlib import Path

queue_dir = sys.argv[1]
spec_id = sys.argv[2]
state_dir = str(Path(queue_dir).parent)
db_path = os.path.join(state_dir, "boi.db")
db = Database(db_path, queue_dir)

try:
    deps = db.get_dependencies(spec_id)
    dependents = db.get_dependents(spec_id)

    if deps:
        print(f"  {spec_id} is blocked by:")
        for d in deps:
            print(f"    {d['id']} [{d['status']}]")
    else:
        print(f"  {spec_id} has no dependencies")

    if dependents:
        print(f"  Specs waiting on {spec_id}:")
        for d in dependents:
            print(f"    {d['id']} [{d['status']}]")
finally:
    db.close()
PYEOF
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
        resume)     cmd_resume "$@" ;;
        review)     cmd_review "$@" ;;
        daemon)     cmd_daemon "$@" ;;
        stop)       cmd_stop "$@" ;;
        cleanup)    cmd_cleanup "$@" ;;
        purge)      cmd_purge "$@" ;;
        workers)    cmd_workers "$@" ;;
        telemetry)  cmd_telemetry "$@" ;;
        doctor)     cmd_doctor "$@" ;;
        upgrade)    cmd_upgrade "$@" ;;
        critic)     cmd_critic "$@" ;;
        spec)       cmd_spec "$@" ;;
        project)    cmd_project "$@" ;;
        do)         cmd_do "$@" ;;
        dashboard)  cmd_dashboard "$@" ;;
        dep)        cmd_dep "$@" ;;
        migrate-db) cmd_migrate_db "$@" ;;
        export-db)  cmd_export_db "$@" ;;
        -h|--help) usage; exit 0 ;;
        help)
            # 'boi help' shows usage; 'boi help <cmd>' shows <cmd> --help
            if [[ $# -eq 0 ]]; then
                usage
                exit 0
            fi
            main "$1" --help
            ;;
        --version) echo "boi ${BOI_VERSION}"; exit 0 ;;
        *)
            die_usage "Unknown command: ${command}. Use 'boi --help' for usage."
            ;;
    esac
}

main "$@"
