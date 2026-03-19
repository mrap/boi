#!/bin/bash
# dashboard.sh — Interactive live-updating compact dashboard for BOI.
#
# Renders a color-coded queue view that refreshes every 2s.
# Accepts keyboard input for sort, filter, navigation, and spec actions.
# Designed for tmux panes and small terminal windows.
#
# Usage:
#   bash dashboard.sh           # Run with auto-refresh
#   boi dashboard               # Same, via CLI
#   boi status --watch          # Same, via CLI
#
# Keyboard controls:
#   s  Cycle sort mode (queue → status → progress → dag → name → recent)
#   f  Cycle filter (all → running → queued → completed)
#   c  Toggle completed specs visibility
#   ↑↓ Select spec (highlight row)
#   l  Show log for selected spec
#   t  Show telemetry for selected spec
#   x  Cancel selected spec (with confirmation)
#   Enter  Show full spec details
#   r  Force refresh
#   q  Quit
#   ?  Help overlay

set -uo pipefail

BOI_STATE_DIR="${HOME}/.boi"
BOI_CONFIG="${BOI_STATE_DIR}/config.json"
QUEUE_DIR="${BOI_STATE_DIR}/queue"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REFRESH_INTERVAL=2

# Interactive state variables
SORT_MODE="${BOI_SORT_MODE:-queue}"
FILTER_STATUS="${BOI_FILTER_STATUS:-all}"
SHOW_COMPLETED="${BOI_SHOW_COMPLETED:-true}"
SELECTED_ROW=0
SHOW_HELP=false

# Visible queue IDs (populated by render_dashboard, used by action handlers)
VISIBLE_QUEUE_IDS=()
VISIBLE_COUNT=0

# Sort mode cycle order
SORT_MODES=("queue" "status" "progress" "dag" "name" "recent")

# Filter cycle order
FILTER_MODES=("all" "running" "queued" "completed")

cycle_sort() {
    local i
    for i in "${!SORT_MODES[@]}"; do
        if [[ "${SORT_MODES[$i]}" == "${SORT_MODE}" ]]; then
            local next=$(( (i + 1) % ${#SORT_MODES[@]} ))
            SORT_MODE="${SORT_MODES[$next]}"
            return
        fi
    done
    SORT_MODE="queue"
}

cycle_filter() {
    local i
    for i in "${!FILTER_MODES[@]}"; do
        if [[ "${FILTER_MODES[$i]}" == "${FILTER_STATUS}" ]]; then
            local next=$(( (i + 1) % ${#FILTER_MODES[@]} ))
            FILTER_STATUS="${FILTER_MODES[$next]}"
            return
        fi
    done
    FILTER_STATUS="all"
}

toggle_completed() {
    if [[ "${SHOW_COMPLETED}" == "true" ]]; then
        SHOW_COMPLETED="false"
    else
        SHOW_COMPLETED="true"
    fi
}

# Get the queue ID for the currently selected row
get_selected_queue_id() {
    if [[ ${VISIBLE_COUNT} -eq 0 ]]; then
        echo ""
        return
    fi
    # Clamp selection
    if [[ ${SELECTED_ROW} -ge ${VISIBLE_COUNT} ]]; then
        SELECTED_ROW=$((VISIBLE_COUNT - 1))
    fi
    if [[ ${SELECTED_ROW} -lt 0 ]]; then
        SELECTED_ROW=0
    fi
    echo "${VISIBLE_QUEUE_IDS[${SELECTED_ROW}]}"
}

show_keybinds() {
    # Dim keybinding bar at the bottom, styled like Claude Code's status bar
    local DIM=$'\033[38;2;108;111;133m'
    local CYAN_BOLD=$'\033[38;2;4;165;229m'
    local NC=$'\033[0m'

    echo ""
    printf " %s" \
        "${CYAN_BOLD}s${NC}${DIM}:sort${NC}  " \
        "${CYAN_BOLD}f${NC}${DIM}:filter${NC}  " \
        "${CYAN_BOLD}c${NC}${DIM}:hide completed${NC}  " \
        "${CYAN_BOLD}↑↓${NC}${DIM}:select${NC}  " \
        "${CYAN_BOLD}l${NC}${DIM}:log${NC}  " \
        "${CYAN_BOLD}t${NC}${DIM}:telemetry${NC}  " \
        "${CYAN_BOLD}q${NC}${DIM}:quit${NC}  " \
        "${CYAN_BOLD}?${NC}${DIM}:help${NC}"
    echo ""

    # Show current state indicators
    local state_parts=()
    if [[ "${SORT_MODE}" != "queue" ]]; then
        state_parts+=("sort:${SORT_MODE}")
    fi
    if [[ "${FILTER_STATUS}" != "all" ]]; then
        state_parts+=("filter:${FILTER_STATUS}")
    fi
    if [[ "${SHOW_COMPLETED}" == "false" ]]; then
        state_parts+=("completed:hidden")
    fi
    if [[ ${#state_parts[@]} -gt 0 ]]; then
        local IFS=" | "
        printf " ${DIM}[%s]${NC}\n" "${state_parts[*]}"
    fi
}

show_help_overlay() {
    local DIM=$'\033[38;2;108;111;133m'
    local CYAN_BOLD=$'\033[38;2;4;165;229m'
    local NC=$'\033[0m'

    echo ""
    echo "${DIM}╭─ Dashboard Controls ─────────────────╮${NC}"
    echo "${DIM}│                                      │${NC}"
    echo "${DIM}│  ${CYAN_BOLD}s${NC}${DIM}  Cycle sort: queue → status →     │${NC}"
    echo "${DIM}│     progress → dag → name → recent   │${NC}"
    echo "${DIM}│  ${CYAN_BOLD}f${NC}${DIM}  Cycle filter: all → running →    │${NC}"
    echo "${DIM}│     queued → completed               │${NC}"
    echo "${DIM}│  ${CYAN_BOLD}c${NC}${DIM}  Toggle completed specs           │${NC}"
    echo "${DIM}│  ${CYAN_BOLD}r${NC}${DIM}  Force refresh now                │${NC}"
    echo "${DIM}│  ${CYAN_BOLD}l${NC}${DIM}  Show log for selected spec       │${NC}"
    echo "${DIM}│  ${CYAN_BOLD}t${NC}${DIM}  Show telemetry for selected spec │${NC}"
    echo "${DIM}│  ${CYAN_BOLD}x${NC}${DIM}  Cancel selected spec             │${NC}"
    echo "${DIM}│  ${CYAN_BOLD}↑↓${NC}${DIM} Select spec (highlight row)      │${NC}"
    echo "${DIM}│  ${CYAN_BOLD}⏎${NC}${DIM}  Show spec details                │${NC}"
    echo "${DIM}│  ${CYAN_BOLD}q${NC}${DIM}  Quit dashboard                   │${NC}"
    echo "${DIM}│  ${CYAN_BOLD}?${NC}${DIM}  This help                        │${NC}"
    echo "${DIM}│                                      │${NC}"
    echo "${DIM}╰──────────────────────────────────────╯${NC}"
}

# Run an external command, pausing the dashboard. Resume on keypress.
run_spec_action() {
    local cmd="$1"
    shift
    clear
    "${cmd}" "$@"
    echo ""
    echo -e "\033[38;2;108;111;133mPress any key to return to dashboard...\033[0m"
    read -rsn1 || true
}

handle_key() {
    local key="$1"

    # If help overlay is shown, any key clears it
    if [[ "${SHOW_HELP}" == "true" ]]; then
        SHOW_HELP=false
        return
    fi

    case "${key}" in
        s) cycle_sort ;;
        f) cycle_filter ;;
        c) toggle_completed ;;
        r) ;; # force refresh (just re-render, which happens naturally)
        q) echo ""; exit 0 ;;
        '?') SHOW_HELP=true ;;
        l)
            # Show log for selected spec
            local qid
            qid=$(get_selected_queue_id)
            if [[ -n "${qid}" ]]; then
                run_spec_action bash "${SCRIPT_DIR}/boi.sh" log "${qid}"
            fi
            ;;
        t)
            # Show telemetry for selected spec
            local qid
            qid=$(get_selected_queue_id)
            if [[ -n "${qid}" ]]; then
                run_spec_action bash "${SCRIPT_DIR}/boi.sh" telemetry "${qid}"
            fi
            ;;
        x)
            # Cancel selected spec (with confirmation)
            local qid
            qid=$(get_selected_queue_id)
            if [[ -n "${qid}" ]]; then
                echo ""
                echo -e "\033[38;2;223;142;29mCancel ${qid}? (y/N)\033[0m"
                local confirm=""
                read -rsn1 confirm || true
                if [[ "${confirm}" == "y" || "${confirm}" == "Y" ]]; then
                    bash "${SCRIPT_DIR}/boi.sh" cancel "${qid}"
                    echo ""
                    echo -e "\033[38;2;108;111;133mPress any key to return to dashboard...\033[0m"
                    read -rsn1 || true
                fi
            fi
            ;;
        '')
            # Enter key (empty string from read)
            local qid
            qid=$(get_selected_queue_id)
            if [[ -n "${qid}" ]]; then
                # Show full spec details: read the spec file and queue entry
                clear
                echo -e "\033[1m${qid} — Spec Details\033[0m"
                echo ""
                # Show queue entry info
                local queue_file="${QUEUE_DIR}/${qid}.json"
                if [[ -f "${queue_file}" ]]; then
                    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${queue_file}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])

queue_file = sys.argv[1]
try:
    with open(queue_file) as f:
        entry = json.load(f)
except Exception as e:
    print(f"Error reading queue entry: {e}")
    sys.exit(1)

qid = entry.get("id", "?")
status = entry.get("status", "?")
mode = entry.get("mode", "execute")
iteration = entry.get("iteration", 0)
max_iter = entry.get("max_iterations", 30)
tasks_done = entry.get("tasks_done", 0)
tasks_total = entry.get("tasks_total", 0)
worker = entry.get("last_worker", "-")
spec_path = entry.get("original_spec_path", entry.get("spec_path", ""))

print(f"Status:     {status}")
print(f"Mode:       {mode}")
print(f"Iteration:  {iteration}/{max_iter}")
print(f"Tasks:      {tasks_done}/{tasks_total} done")
print(f"Worker:     {worker or '-'}")
print(f"Spec:       {spec_path}")

# Show task list from spec if available
if spec_path and os.path.isfile(spec_path):
    print("")
    print("\033[1mTasks:\033[0m")
    import re
    with open(spec_path) as f:
        content = f.read()
    # Find task headers and their status
    task_pattern = re.compile(r'^### (t-\d+:.+)\n(DONE|PENDING|FAILED|SKIPPED)', re.MULTILINE)
    for match in task_pattern.finditer(content):
        task_name = match.group(1)
        task_status = match.group(2)
        icon = {"DONE": "✓", "PENDING": "·", "FAILED": "✗", "SKIPPED": "—"}.get(task_status, "?")
        print(f"  {icon} {task_name}")

blocked_by = entry.get("blocked_by", [])
if blocked_by:
    print(f"\nBlocked by: {', '.join(blocked_by)}")
PYEOF
                else
                    echo "Queue entry not found: ${queue_file}"
                fi
                echo ""
                echo -e "\033[38;2;108;111;133mPress any key to return to dashboard...\033[0m"
                read -rsn1 || true
            fi
            ;;
        $'\e')
            # Escape sequence (arrow keys, etc.)
            local arrow=""
            read -rsn2 -t 0.1 arrow || true
            case "${arrow}" in
                '[A') # Up arrow — wrap to bottom
                    if [[ ${SELECTED_ROW} -gt 0 ]]; then
                        SELECTED_ROW=$((SELECTED_ROW - 1))
                    elif [[ ${VISIBLE_COUNT} -gt 0 ]]; then
                        SELECTED_ROW=$((VISIBLE_COUNT - 1))
                    fi
                    ;;
                '[B') # Down arrow — wrap to top
                    if [[ ${VISIBLE_COUNT} -gt 0 ]] && [[ ${SELECTED_ROW} -lt $((VISIBLE_COUNT - 1)) ]]; then
                        SELECTED_ROW=$((SELECTED_ROW + 1))
                    else
                        SELECTED_ROW=0
                    fi
                    ;;
            esac
            ;;
        *) ;; # ignore unknown keys
    esac
}

render_dashboard() {
    export BOI_SORT_MODE="${SORT_MODE}"
    export BOI_FILTER_STATUS="${FILTER_STATUS}"
    export BOI_SHOW_COMPLETED="${SHOW_COMPLETED}"
    export BOI_SELECTED_ROW="${SELECTED_ROW}"

    local output
    output=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${BOI_CONFIG}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.status import build_queue_status, format_dashboard

queue_dir = sys.argv[1]
config_path = sys.argv[2]

config = None
if os.path.isfile(config_path):
    try:
        with open(config_path) as f:
            config = json.load(f)
    except Exception:
        pass

# Pass interactive state to format_dashboard via kwargs
sort_mode = os.environ.get("BOI_SORT_MODE", "queue")
filter_status = os.environ.get("BOI_FILTER_STATUS", "all")
show_completed = os.environ.get("BOI_SHOW_COMPLETED", "true") == "true"
selected_row = int(os.environ.get("BOI_SELECTED_ROW", "0"))

status_data = build_queue_status(queue_dir, config)
print(format_dashboard(
    status_data,
    sort_mode=sort_mode,
    filter_status=filter_status,
    show_completed=show_completed,
    selected_row=selected_row,
))
PYEOF
    )

    # Extract __QUEUE_IDS__ line from output and parse visible queue IDs
    VISIBLE_QUEUE_IDS=()
    VISIBLE_COUNT=0
    local display_output=""

    while IFS= read -r line; do
        if [[ "${line}" == __QUEUE_IDS__:* ]]; then
            local ids_str="${line#__QUEUE_IDS__:}"
            if [[ -n "${ids_str}" ]]; then
                IFS=',' read -ra VISIBLE_QUEUE_IDS <<< "${ids_str}"
                VISIBLE_COUNT=${#VISIBLE_QUEUE_IDS[@]}
            fi
        else
            if [[ -n "${display_output}" ]]; then
                display_output="${display_output}
${line}"
            else
                display_output="${line}"
            fi
        fi
    done <<< "${output}"

    # Clamp selected row after we know visible count
    if [[ ${VISIBLE_COUNT} -gt 0 ]]; then
        if [[ ${SELECTED_ROW} -ge ${VISIBLE_COUNT} ]]; then
            SELECTED_ROW=$((VISIBLE_COUNT - 1))
        fi
    else
        SELECTED_ROW=0
    fi

    echo "${display_output}"
}

main() {
    if [[ ! -d "${QUEUE_DIR}" ]]; then
        mkdir -p "${QUEUE_DIR}"
    fi

    # Trap Ctrl+C for clean exit
    trap 'echo ""; exit 0' INT TERM

    while true; do
        clear
        render_dashboard

        if [[ "${SHOW_HELP}" == "true" ]]; then
            show_help_overlay
        else
            show_keybinds
        fi

        # Read one keypress with timeout (replaces sleep)
        local key=""
        read -rsn1 -t "${REFRESH_INTERVAL}" key || true

        if [[ -n "${key}" ]]; then
            handle_key "${key}"
        fi
    done
}

main "$@"
