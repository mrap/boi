#!/bin/bash
# dashboard.sh — Live-updating compact dashboard for BOI.
#
# Renders a 60-char-wide, color-coded queue view that refreshes every 2s.
# Designed for tmux panes and small terminal windows.
#
# Usage:
#   bash dashboard.sh           # Run with auto-refresh
#   boi dashboard               # Same, via CLI
#   boi status --watch          # Same, via CLI

set -uo pipefail

BOI_STATE_DIR="${HOME}/.boi"
BOI_CONFIG="${BOI_STATE_DIR}/config.json"
QUEUE_DIR="${BOI_STATE_DIR}/queue"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REFRESH_INTERVAL=2

render_dashboard() {
    BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${BOI_CONFIG}" <<'PYEOF'
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

status_data = build_queue_status(queue_dir, config)
print(format_dashboard(status_data))
PYEOF
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
        sleep "${REFRESH_INTERVAL}"
    done
}

main "$@"
