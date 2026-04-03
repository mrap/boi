#!/bin/bash
# install-public.sh — Public installer for BOI (Beginning of Infinity).
#
# Install BOI on any macOS or Linux machine:
#   curl -fsSL https://raw.githubusercontent.com/mrap/boi/main/install-public.sh | bash
#   bash install-public.sh
#
# This script:
#   1. Checks prerequisites (bash, python3 3.10+, git, tmux)
#   2. Clones/updates the BOI repo to ~/.boi/src/
#   3. Creates the ~/.boi/ state directory structure
#   4. Creates a 'boi' symlink in a PATH-accessible location
#   5. Prints success message with next steps

set -uo pipefail

# ─── Constants ───────────────────────────────────────────────────────────────

BOI_REPO="https://github.com/mrap/boi.git"
DEFAULT_PREFIX="${HOME}/.boi"
BOI_SRC_DIR=""  # set after prefix is known
BOI_STATE_DIR=""  # set after prefix is known
SYMLINK_DIR=""  # determined by platform

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

# Flags
PREFIX=""
NO_SYMLINK=false
NO_PLUGIN=false
UPDATE_MODE=false
RUNTIME=""
WORKER_COUNT=3
MAX_WORKERS=5
SKIP_WORKERS=false
VERBOSE=false

# ─── Logging ─────────────────────────────────────────────────────────────────

log_info()  { echo -e "${GREEN}[boi]${NC} $1"; }
log_warn()  { echo -e "${YELLOW}[boi]${NC} $1"; }
log_error() { echo -e "${RED}[boi]${NC} $1" >&2; }
log_step()  { echo -e "\n${BOLD}==> $1${NC}"; }

# Verbose-only variants (suppressed when VERBOSE=false)
vlog_info() { [[ "${VERBOSE}" == "true" ]] && log_info "$1" || true; }
vlog_warn() { [[ "${VERBOSE}" == "true" ]] && log_warn "$1" || true; }
vlog_step() { [[ "${VERBOSE}" == "true" ]] && log_step "$1" || true; }

# Clean single-line progress (always shown)
step_ok()   { echo -e "  ${GREEN}✓${NC} $1"; }
step_skip() { echo -e "  ${DIM}–${NC} $1"; }
step_fail() { echo -e "  ${RED}✗${NC} $1" >&2; }

# ─── Usage ───────────────────────────────────────────────────────────────────

usage() {
    cat <<EOF
BOI Installer — Beginning of Infinity

Usage:
  bash install-public.sh [OPTIONS]
  curl -fsSL <url>/install-public.sh | bash

Options:
  --prefix <path>    Install location (default: ~/.boi)
  --workers N        Number of workers to create (default: 3, max: 5)
  --no-symlink       Skip creating the 'boi' symlink in PATH
  --no-plugin        Skip installing Claude Code plugin
  --no-workers       Skip worker/worktree creation
  --update           Update an existing installation
  --runtime <name>   Set runtime: claude (default) or codex
  --verbose          Show detailed progress output
  -h, --help         Show this help

Prerequisites:
  bash, python3 (3.10+), git, tmux

EOF
}

# ─── Argument Parsing ────────────────────────────────────────────────────────

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --prefix)
                [[ -z "${2:-}" ]] && { log_error "--prefix requires a path"; exit 1; }
                PREFIX="$2"
                shift 2
                ;;
            --workers)
                [[ -z "${2:-}" ]] && { log_error "--workers requires a number"; exit 1; }
                WORKER_COUNT="$2"
                shift 2
                ;;
            --no-workers)
                SKIP_WORKERS=true
                shift
                ;;
            --no-symlink)
                NO_SYMLINK=true
                shift
                ;;
            --no-plugin)
                NO_PLUGIN=true
                shift
                ;;
            --update)
                UPDATE_MODE=true
                shift
                ;;
            --runtime)
                [[ -z "${2:-}" ]] && { log_error "--runtime requires a value (claude or codex)"; exit 1; }
                RUNTIME="$2"
                shift 2
                ;;
            --verbose)
                VERBOSE=true
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                log_error "Unknown option: $1"
                usage
                exit 1
                ;;
        esac
    done

    # Resolve prefix
    if [[ -z "${PREFIX}" ]]; then
        PREFIX="${DEFAULT_PREFIX}"
    fi
    # Expand ~ if present
    PREFIX="${PREFIX/#\~/$HOME}"

    BOI_SRC_DIR="${PREFIX}/src"
    BOI_STATE_DIR="${PREFIX}"

    # Validate worker count
    if ! [[ "${WORKER_COUNT}" =~ ^[0-9]+$ ]] || [[ "${WORKER_COUNT}" -lt 1 ]] || [[ "${WORKER_COUNT}" -gt "${MAX_WORKERS}" ]]; then
        log_error "--workers must be between 1 and ${MAX_WORKERS}. Got: ${WORKER_COUNT}"
        exit 1
    fi
}

# ─── Prerequisite Checks ────────────────────────────────────────────────────

check_command() {
    local cmd="$1"
    local install_hint="$2"
    if ! command -v "${cmd}" &>/dev/null; then
        log_error "'${cmd}' not found. ${install_hint}"
        return 1
    fi
    return 0
}

check_python_version() {
    local python_cmd=""
    if command -v python3 &>/dev/null; then
        python_cmd="python3"
    elif command -v python &>/dev/null; then
        python_cmd="python"
    else
        log_error "python3 not found."
        log_error "Install: https://www.python.org/downloads/ or use your package manager."
        return 1
    fi

    local version
    version=$("${python_cmd}" -c "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')" 2>/dev/null)
    if [[ -z "${version}" ]]; then
        log_error "Could not determine Python version."
        return 1
    fi

    local major minor
    major=$(echo "${version}" | cut -d. -f1)
    minor=$(echo "${version}" | cut -d. -f2)

    if [[ "${major}" -lt 3 ]] || { [[ "${major}" -eq 3 ]] && [[ "${minor}" -lt 10 ]]; }; then
        log_error "Python 3.10+ required. Found: ${version}"
        log_error "Install: https://www.python.org/downloads/"
        return 1
    fi

    vlog_info "Python: ${version} (${python_cmd})"
    return 0
}

check_prerequisites() {
    vlog_step "Checking prerequisites"

    local failed=false

    if ! check_command "bash" "bash is required."; then
        failed=true
    else
        vlog_info "bash: $(bash --version | head -1 | sed 's/GNU bash, version //' | cut -d' ' -f1)"
    fi

    if ! check_python_version; then
        failed=true
    fi

    if ! check_command "git" "Install: https://git-scm.com/downloads"; then
        failed=true
    else
        vlog_info "git: $(git --version | sed 's/git version //')"
    fi

    if ! check_command "tmux" "Install: sudo apt install tmux (Linux) or brew install tmux (macOS)"; then
        failed=true
    else
        vlog_info "tmux: $(tmux -V | sed 's/tmux //')"
    fi

    if [[ "${failed}" == "true" ]]; then
        echo ""
        log_error "Missing prerequisites. Install the above and retry."
        step_fail "Prerequisites"
        exit 1
    fi

    vlog_info "All prerequisites met."
    step_ok "Prerequisites"
}

# ─── Clone / Update Repo ────────────────────────────────────────────────────

clone_or_update_repo() {
    vlog_step "Setting up BOI source"

    if [[ -d "${BOI_SRC_DIR}/.git" ]]; then
        if [[ "${UPDATE_MODE}" == "true" ]]; then
            vlog_info "Updating existing installation at ${BOI_SRC_DIR}"
            if ! git -C "${BOI_SRC_DIR}" pull --rebase --quiet 2>/dev/null; then
                vlog_warn "git pull failed. Trying fetch + reset."
                git -C "${BOI_SRC_DIR}" fetch origin
                git -C "${BOI_SRC_DIR}" reset --hard origin/main
            fi
            vlog_info "Source updated."
        else
            vlog_info "BOI source already exists at ${BOI_SRC_DIR}"
            vlog_info "Use --update to pull latest changes."
        fi
    else
        vlog_info "Cloning BOI to ${BOI_SRC_DIR}"
        mkdir -p "$(dirname "${BOI_SRC_DIR}")"
        local git_out
        if ! git_out=$(git clone "${BOI_REPO}" "${BOI_SRC_DIR}" 2>&1); then
            log_error "Failed to clone BOI repository."
            log_error "Check your network connection and try again."
            [[ "${VERBOSE}" == "true" ]] && echo "${git_out}" >&2
            step_fail "Source"
            exit 1
        fi
        vlog_info "Source cloned."
    fi

    step_ok "Source"
}

# ─── Directory Structure ────────────────────────────────────────────────────

create_directories() {
    vlog_step "Creating state directories"

    local dirs=(
        "${BOI_STATE_DIR}"
        "${BOI_STATE_DIR}/queue"
        "${BOI_STATE_DIR}/events"
        "${BOI_STATE_DIR}/logs"
        "${BOI_STATE_DIR}/hooks"
        "${BOI_STATE_DIR}/critic/custom"
        "${BOI_STATE_DIR}/worktrees"
        "${BOI_STATE_DIR}/projects"
    )

    for dir in "${dirs[@]}"; do
        if [[ ! -d "${dir}" ]]; then
            mkdir -p "${dir}"
            vlog_info "Created: ${dir}"
        else
            vlog_info "Exists:  ${dir}"
        fi
    done

    # Create critic config with defaults
    if [[ ! -f "${BOI_STATE_DIR}/critic/config.json" ]]; then
        cat > "${BOI_STATE_DIR}/critic/config.json" << 'CRITIC_EOF'
{
  "enabled": true,
  "trigger": "on_complete",
  "max_passes": 2,
  "checks": ["spec-integrity", "verify-commands", "code-quality", "completeness", "fleet-readiness", "blast-radius"],
  "custom_checks_dir": "custom",
  "timeout_seconds": 600
}
CRITIC_EOF
        vlog_info "Created critic config with defaults"
    fi
}

# ─── Runtime Config ──────────────────────────────────────────────────────────

seed_runtime_config() {
    local config_file="${BOI_STATE_DIR}/config.json"

    if [[ "${UPDATE_MODE}" == "true" ]]; then
        # On update: preserve any existing runtime config. Never overwrite.
        if [[ -f "${config_file}" ]]; then
            vlog_info "Preserving existing config.json (not overwriting on --update)"
            return 0
        fi
        # If no config.json yet (upgrade from very old install), fall through to create.
    fi

    # Determine runtime: explicit flag > default "claude"
    local runtime="${RUNTIME:-claude}"

    if [[ -f "${config_file}" ]]; then
        vlog_info "config.json already exists, skipping runtime seed"
        return 0
    fi

    local context_root="${BOI_CONTEXT_ROOT:-}"
    if [[ -n "${context_root}" ]]; then
        cat > "${config_file}" << CONF_EOF
{
  "runtime": {
    "default": "${runtime}"
  },
  "context_root": "${context_root}"
}
CONF_EOF
    else
        cat > "${config_file}" << CONF_EOF
{
  "runtime": {
    "default": "${runtime}"
  }
}
CONF_EOF
    fi
    vlog_info "Created config.json with runtime=${runtime}"
}

# ─── Guardrails ──────────────────────────────────────────────────────────────

seed_guardrails() {
    local guardrails_file="${BOI_STATE_DIR}/guardrails.toml"

    if [[ -f "${guardrails_file}" ]]; then
        vlog_info "guardrails.toml already exists, skipping"
        return 0
    fi

    cat > "${guardrails_file}" << 'GUARDRAILS_EOF'
[pipeline]
default = ["execute", "review", "critic"]
GUARDRAILS_EOF
    vlog_info "Created guardrails.toml with default pipeline"
}

# ─── Worker Setup ────────────────────────────────────────────────────────────

setup_workers() {
    if [[ "${SKIP_WORKERS}" == "true" ]]; then
        vlog_info "Skipping worker setup (--no-workers)"
        step_skip "Workers"
        return 0
    fi

    vlog_step "Setting up workers"

    local worktree_dir="${BOI_STATE_DIR}/worktrees"
    local worktree_prefix="${worktree_dir}/boi-worker-"
    local config_file="${BOI_STATE_DIR}/config.json"

    # Check if workers already exist (first worktree dir present + config has workers array)
    local first_worktree="${worktree_prefix}1"
    local config_has_workers=false
    if [[ -f "${config_file}" ]]; then
        if python3 -c "import json,sys; c=json.load(open('${config_file}')); sys.exit(0 if c.get('workers') else 1)" 2>/dev/null; then
            config_has_workers=true
        fi
    fi

    if [[ -d "${first_worktree}" ]] && [[ "${config_has_workers}" == "true" ]]; then
        local existing_count
        existing_count=$(python3 -c "import json; c=json.load(open('${config_file}')); print(len(c.get('workers',[])))" 2>/dev/null || echo "0")
        vlog_info "Workers already configured (${existing_count} workers). Skipping."
        vlog_info "Use --no-workers to skip, or delete ${worktree_dir} to recreate."
        step_ok "Workers (${existing_count} existing)"
        return 0
    fi

    # Check that we're not inside Claude Code (worktree creation needs a real terminal)
    if [[ -n "${CLAUDECODE:-}" ]]; then
        log_warn "Detected Claude Code environment. Worker worktree creation skipped."
        log_warn "Run from a terminal: bash ${BOI_SRC_DIR}/install-public.sh"
        step_skip "Workers (run from a terminal to create)"
        return 0
    fi

    # Determine runtime
    local runtime="${RUNTIME:-claude}"

    # Create worktrees
    vlog_info "Creating ${WORKER_COUNT} worker worktrees in ${worktree_dir}"

    local failed=0
    for i in $(seq 1 "${WORKER_COUNT}"); do
        local dest="${worktree_prefix}${i}"
        if [[ "${VERBOSE}" == "true" ]]; then
            printf "  Worker %d/%d... " "${i}" "${WORKER_COUNT}"
        fi

        if [[ -d "${dest}" ]]; then
            [[ "${VERBOSE}" == "true" ]] && echo -e "${GREEN}✓${NC} (already exists)"
            continue
        fi

        if git -C "${BOI_SRC_DIR}" worktree add "${dest}" 2>/dev/null; then
            [[ "${VERBOSE}" == "true" ]] && echo -e "${GREEN}✓${NC}"
        else
            [[ "${VERBOSE}" == "true" ]] && echo -e "${RED}✗${NC} (git worktree add failed)"
            failed=$((failed + 1))
        fi
    done

    if [[ "${failed}" -gt 0 ]]; then
        log_warn "${failed} worktree(s) failed. Workers may not function correctly."
        log_warn "Retry: bash ${BOI_SRC_DIR}/install.sh --workers ${WORKER_COUNT}"
    fi

    # Write full config.json with workers array (merges with existing config)
    local pyout
    pyout=$(_write_workers_to_config "${worktree_prefix}" "${runtime}")
    vlog_info "${pyout}"

    local created=$((WORKER_COUNT - failed))
    vlog_info "Workers ready: ${created}/${WORKER_COUNT}"
    step_ok "Workers (${created})"
}

_write_workers_to_config() {
    local worktree_prefix="$1"
    local runtime="$2"
    local config_file="${BOI_STATE_DIR}/config.json"
    local timestamp
    timestamp=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    local env_type
    env_type=$(uname | tr '[:upper:]' '[:lower:]')
    if [[ "${env_type}" == "darwin" ]]; then env_type="macos"; fi

    # Pass context_root from env (set by hex-upgrade or user) for --add-dir injection
    local context_root="${BOI_CONTEXT_ROOT:-}"

    python3 - "${config_file}" "${worktree_prefix}" "${WORKER_COUNT}" "${runtime}" \
        "${BOI_STATE_DIR}" "${BOI_SRC_DIR}" "${timestamp}" "${env_type}" "${context_root}" << 'PYEOF'
import json, sys, os

config_path    = sys.argv[1]
wt_prefix      = sys.argv[2]
worker_count   = int(sys.argv[3])
runtime        = sys.argv[4]
boi_state_dir  = sys.argv[5]
boi_src_dir    = sys.argv[6]
timestamp      = sys.argv[7]
env_type       = sys.argv[8]
context_root   = sys.argv[9] if len(sys.argv) > 9 else ""

# Load existing config if present
config = {}
if os.path.exists(config_path):
    with open(config_path) as f:
        try:
            config = json.load(f)
        except json.JSONDecodeError:
            config = {}

workers = []
for i in range(1, worker_count + 1):
    workers.append({
        "id": f"w-{i}",
        "worktree_path": f"{wt_prefix}{i}",
        "status": "idle"
    })

# Merge: preserve existing keys, add/overwrite workers-related keys
config.setdefault("version", "1")
config.setdefault("tool", "boi")
config.setdefault("created_at", timestamp)
config["environment"]     = env_type
config["worker_count"]    = worker_count
config["worktree_prefix"] = wt_prefix
config.setdefault("repo_path", boi_src_dir)
config["workers"]         = workers
config.setdefault("daemon", {
    "poll_interval_s": 5,
    "pid_file": f"{boi_state_dir}/daemon.pid",
    "log_dir":  f"{boi_state_dir}/logs"
})
config.setdefault("boi_dir", boi_src_dir)
config["runtime"] = config.get("runtime") or {"default": runtime}

# Auto-configure context_root for --add-dir injection (set by hex-upgrade or BOI_CONTEXT_ROOT env)
if context_root:
    config.setdefault("context_root", context_root)

tmp = config_path + ".tmp"
with open(tmp, "w") as f:
    json.dump(config, f, indent=2)
os.replace(tmp, config_path)
print(f"[boi] Config updated with {worker_count} workers.")
PYEOF
}

# ─── Critic Config Merge ─────────────────────────────────────────────────────

merge_critic_checks() {
    local critic_config="${BOI_STATE_DIR}/critic/config.json"

    if [[ ! -f "${critic_config}" ]]; then
        vlog_info "No critic config to merge — will be created by create_directories"
        return 0
    fi

    # Default checks that must be present
    local default_checks=("spec-integrity" "verify-commands" "code-quality" "completeness" "fleet-readiness" "blast-radius")

    # Use Python to merge checks without removing custom ones
    local pyout
    pyout=$(python3 - "${critic_config}" "${default_checks[@]}" << 'PYEOF'
import json, sys

config_path = sys.argv[1]
required_checks = sys.argv[2:]

with open(config_path) as f:
    config = json.load(f)

existing = config.get("checks", [])
added = []
for check in required_checks:
    if check not in existing:
        existing.append(check)
        added.append(check)

config["checks"] = existing

with open(config_path, "w") as f:
    json.dump(config, f, indent=2)

if added:
    print(f"[boi] Merged critic checks: added {added}")
else:
    print("[boi] Critic checks already up to date")
PYEOF
)
    vlog_info "${pyout}"
}

# ─── Phase Files Sync ────────────────────────────────────────────────────────

sync_phase_files() {
    if [[ "${UPDATE_MODE}" != "true" ]]; then
        return 0
    fi

    local phases_src="${BOI_SRC_DIR}/phases"
    local phases_dst="${BOI_STATE_DIR}/phases"

    if [[ ! -d "${phases_src}" ]]; then
        vlog_warn "No phases/ directory in source, skipping phase sync"
        return 0
    fi

    mkdir -p "${phases_dst}"
    for phase_file in "${phases_src}"/*.phase.toml; do
        [[ -f "${phase_file}" ]] || continue
        local fname
        fname=$(basename "${phase_file}")
        cp "${phase_file}" "${phases_dst}/${fname}"
        vlog_info "Synced phase: ${fname}"
    done
}

# ─── Symlink ─────────────────────────────────────────────────────────────────

determine_symlink_dir() {
    # Try common PATH-accessible locations in order of preference
    if [[ -d "/usr/local/bin" ]] && [[ -w "/usr/local/bin" ]]; then
        SYMLINK_DIR="/usr/local/bin"
    elif [[ -d "${HOME}/.local/bin" ]]; then
        SYMLINK_DIR="${HOME}/.local/bin"
    elif [[ -d "${HOME}/bin" ]]; then
        SYMLINK_DIR="${HOME}/bin"
    else
        # Create ~/.local/bin as a reasonable default
        SYMLINK_DIR="${HOME}/.local/bin"
        mkdir -p "${SYMLINK_DIR}"
    fi
}

create_symlink() {
    if [[ "${NO_SYMLINK}" == "true" ]]; then
        vlog_info "Skipping symlink creation (--no-symlink)"
        step_skip "Command"
        return 0
    fi

    vlog_step "Creating boi command"

    determine_symlink_dir

    local target="${BOI_SRC_DIR}/boi.sh"
    local symlink="${SYMLINK_DIR}/boi"

    if [[ ! -f "${target}" ]]; then
        log_error "boi.sh not found at ${target}"
        log_error "The clone may have failed. Check ${BOI_SRC_DIR}/"
        step_fail "Command"
        return 1
    fi

    # Ensure boi.sh is executable
    chmod +x "${target}"

    if [[ -L "${symlink}" ]]; then
        local existing
        existing=$(readlink "${symlink}")
        if [[ "${existing}" == "${target}" ]]; then
            vlog_info "Symlink already correct: ${symlink} -> ${target}"
        else
            ln -sf "${target}" "${symlink}"
            vlog_info "Updated symlink: ${symlink} -> ${target}"
        fi
    elif [[ -e "${symlink}" ]]; then
        log_warn "${symlink} exists but is not a symlink. Skipping."
        log_warn "You can manually link: ln -sf ${target} ${symlink}"
        step_skip "Command (manual link needed)"
        return 0
    else
        ln -s "${target}" "${symlink}"
        vlog_info "Created symlink: ${symlink} -> ${target}"
    fi

    # Check if the symlink directory is on PATH
    if [[ ":${PATH}:" != *":${SYMLINK_DIR}:"* ]]; then
        log_warn "${SYMLINK_DIR} is not on your PATH."
        echo ""
        log_warn "Add to your shell config:"
        if [[ "$(uname)" == "Darwin" ]]; then
            log_warn "  echo 'export PATH=\"${SYMLINK_DIR}:\$PATH\"' >> ~/.zshrc"
            log_warn "  source ~/.zshrc"
        else
            log_warn "  echo 'export PATH=\"${SYMLINK_DIR}:\$PATH\"' >> ~/.bashrc"
            log_warn "  source ~/.bashrc"
        fi
    fi

    step_ok "Command: boi"
}

# ─── Claude Code Plugin ─────────────────────────────────────────────────────

install_plugin() {
    if [[ "${NO_PLUGIN}" == "true" ]]; then
        vlog_info "Skipping Claude Code plugin (--no-plugin)"
        step_skip "Plugin"
        return 0
    fi

    vlog_step "Installing Claude Code plugin"

    local claude_dir="${HOME}/.claude"
    if [[ -d "${claude_dir}" ]]; then
        mkdir -p "${claude_dir}/skills/boi" "${claude_dir}/commands"

        # Copy skill
        cp "${BOI_SRC_DIR}/plugin/skills/boi/SKILL.md" "${claude_dir}/skills/boi/SKILL.md"

        # Copy command
        cp "${BOI_SRC_DIR}/plugin/commands/boi.md" "${claude_dir}/commands/boi.md"

        vlog_info "Claude Code plugin installed (/boi command + BOI skill)"
        step_ok "Plugin"
    else
        vlog_info "Claude Code not detected. Skip plugin install."
        vlog_info "To install later: cp -r ${BOI_SRC_DIR}/plugin/skills/boi ~/.claude/skills/"
        step_skip "Plugin (Claude Code not detected)"
    fi
}

# ─── Daemon Auto-Start and Processing Verification ───────────────────────────

# Returns 0 if daemon is running (PID file exists + process alive), 1 otherwise.
_daemon_running() {
    local pid_file="${BOI_STATE_DIR}/daemon.pid"
    if [[ ! -f "${pid_file}" ]]; then
        return 1
    fi
    local pid
    pid=$(cat "${pid_file}")
    if kill -0 "${pid}" 2>/dev/null; then
        return 0
    fi
    return 1
}

# Start the daemon if not already running. Waits up to $1 seconds for it to come up.
# Returns 0 on success, 1 if it fails to start.
_start_daemon_once() {
    local wait_secs="${1:-10}"
    local daemon_script="${BOI_SRC_DIR}/daemon.py"
    local log="${BOI_STATE_DIR}/logs/daemon-startup.log"

    if [[ ! -f "${daemon_script}" ]]; then
        log_error "daemon.py not found at ${daemon_script}"
        return 1
    fi

    BOI_NO_TMUX="${BOI_NO_TMUX:-}" nohup python3 "${daemon_script}" > "${log}" 2>&1 < /dev/null &

    local elapsed=0
    while [[ "${elapsed}" -lt "${wait_secs}" ]]; do
        sleep 1
        elapsed=$((elapsed + 1))
        if _daemon_running; then
            return 0
        fi
    done
    return 1
}

# Verify daemon is actively processing by checking the heartbeat file is written
# (or written freshly) within $1 seconds. Returns 0 if heartbeat appears, 1 otherwise.
_verify_heartbeat() {
    local wait_secs="${1:-10}"
    local heartbeat="${BOI_STATE_DIR}/daemon-heartbeat"

    # Capture current state: if heartbeat exists, note its content so we can detect
    # a fresh write (proves a new poll cycle ran).
    local before=""
    if [[ -f "${heartbeat}" ]]; then
        before=$(cat "${heartbeat}")
    fi

    local elapsed=0
    while [[ "${elapsed}" -lt "${wait_secs}" ]]; do
        sleep 1
        elapsed=$((elapsed + 1))

        if [[ -f "${heartbeat}" ]]; then
            local current
            current=$(cat "${heartbeat}" 2>/dev/null || true)
            # If heartbeat didn't exist before, any content means the daemon cycled.
            # If it existed, check that the timestamp has changed (new poll cycle).
            if [[ -z "${before}" ]] || [[ "${current}" != "${before}" ]]; then
                return 0
            fi
        fi
    done
    return 1
}

start_and_verify_daemon() {
    # Skip inside Claude Code — daemon requires a real terminal to fork cleanly.
    if [[ -n "${CLAUDECODE:-}" ]]; then
        log_warn "Detected Claude Code environment. Daemon start skipped."
        log_warn "Run from a terminal: bash ${BOI_SRC_DIR}/install-public.sh"
        step_skip "Daemon (run from a terminal to start)"
        return 0
    fi

    vlog_step "Starting daemon"

    if _daemon_running; then
        vlog_info "Daemon already running."
    else
        vlog_info "Starting daemon..."
        if ! _start_daemon_once 10; then
            vlog_warn "Daemon did not start on first attempt. Retrying once..."
            if ! _start_daemon_once 10; then
                log_error "Daemon failed to start after two attempts."
                log_error "Check logs: ${BOI_STATE_DIR}/logs/daemon-startup.log"
                log_error "Try manually: python3 ${BOI_SRC_DIR}/daemon.py --foreground"
                step_fail "Daemon"
                return 1
            fi
        fi
        vlog_info "Daemon started."
    fi

    # Verify the daemon is actively processing its poll loop (heartbeat appears/updates).
    vlog_info "Verifying daemon is processing..."
    if _verify_heartbeat 15; then
        vlog_info "Daemon is healthy and processing."
        step_ok "Daemon"
    else
        log_warn "No heartbeat after 15s. Attempting daemon restart..."
        # Kill stale process if PID file exists
        local pid_file="${BOI_STATE_DIR}/daemon.pid"
        if [[ -f "${pid_file}" ]]; then
            local pid
            pid=$(cat "${pid_file}")
            kill "${pid}" 2>/dev/null || true
            rm -f "${pid_file}"
            sleep 1
        fi
        if _start_daemon_once 10 && _verify_heartbeat 15; then
            vlog_info "Daemon is healthy after restart."
            step_ok "Daemon"
        else
            log_error "Daemon started but is not processing events."
            log_error "Check logs: ${BOI_STATE_DIR}/logs/daemon-startup.log"
            log_error "Run 'boi doctor' for detailed diagnostics."
            step_fail "Daemon"
            return 1
        fi
    fi
}

# ─── End-to-End Smoke Test ───────────────────────────────────────────────────

# Write a minimal smoke spec to $1. Uses smoke-1 task ID to avoid conflicts.
_write_smoke_spec() {
    local out_file="$1"
    cat > "${out_file}" << 'SMOKE_EOF'
# BOI Smoke Test

**Mode:** execute

## Tasks

### t-1: Verify BOI pipeline is operational
PENDING

**Spec:** Write the text "smoke_ok" to /tmp/boi-smoke-result.txt using:
  echo smoke_ok > /tmp/boi-smoke-result.txt

**Verify:** File /tmp/boi-smoke-result.txt exists and contains "smoke_ok".
SMOKE_EOF
}

# Dispatch a smoke spec and echo the queue ID on stdout.
# Returns 0 on success, 1 if dispatch failed or queue ID not found.
_smoke_dispatch() {
    local spec_file="$1"
    local output
    output=$(boi dispatch --spec "${spec_file}" --no-critic --priority 1 2>&1)
    local exit_code=$?
    if [[ "${exit_code}" -ne 0 ]]; then
        echo "${output}" >&2
        return 1
    fi
    # Extract the queue ID (e.g. q-322) from the progress line: "✓ (q-322, 1/1 tasks...)"
    local qid
    qid=$(echo "${output}" | grep -oE 'q-[0-9]+' | head -1)
    if [[ -z "${qid}" ]]; then
        log_error "Could not parse queue ID from dispatch output:"
        echo "${output}" >&2
        return 1
    fi
    echo "${qid}"
    return 0
}

# Poll boi status for $1 (queue ID) up to $2 seconds (default 120).
# Returns: 0=completed, 1=failed/canceled, 2=timeout.
_smoke_poll() {
    local queue_id="$1"
    local max_wait="${2:-120}"
    local poll_interval=5
    local elapsed=0

    printf "  Waiting"

    while [[ "${elapsed}" -lt "${max_wait}" ]]; do
        sleep "${poll_interval}"
        elapsed=$((elapsed + poll_interval))

        local status
        status=$(boi status --json --all 2>/dev/null | python3 -c "
import json, sys
qid = sys.argv[1]
try:
    d = json.load(sys.stdin)
    for e in d.get('entries', []):
        if e.get('id') == qid:
            print(e.get('status', 'unknown'))
            break
    else:
        print('unknown')
except Exception:
    print('unknown')
" "${queue_id}" 2>/dev/null || echo "unknown")

        case "${status}" in
            completed)
                echo ""
                return 0
                ;;
            failed|canceled)
                echo ""
                log_error "Smoke test ${queue_id} ended with status: ${status}"
                log_error "View logs: boi log ${queue_id}"
                return 1
                ;;
            *)
                printf "."
                ;;
        esac
    done

    echo ""
    return 2
}

# Run a full smoke test dispatch+poll cycle. Returns 0 on success, 1 on failure.
_run_smoke_once() {
    local spec_file="$1"
    local max_wait="${2:-120}"

    local queue_id
    if ! queue_id=$(_smoke_dispatch "${spec_file}"); then
        return 1
    fi

    vlog_info "Smoke test queued as ${queue_id}."

    _smoke_poll "${queue_id}" "${max_wait}"
    local poll_result=$?

    if [[ "${poll_result}" -eq 2 ]]; then
        log_warn "Smoke test timed out after ${max_wait}s (${queue_id} still in progress)."
        boi cancel "${queue_id}" 2>/dev/null || true
        return 1
    fi
    return "${poll_result}"
}

run_smoke_test() {
    # Skip inside Claude Code — cannot fork workers cleanly.
    if [[ -n "${CLAUDECODE:-}" ]]; then
        echo ""
        echo -e "${GREEN}${BOLD}BOI installed successfully!${NC}"
        echo -e "${DIM}(Smoke test skipped — run from a terminal to verify end-to-end)${NC}"
        return 0
    fi

    # Skip in headless/Docker/CI mode — tmux not available for workers.
    if [[ "${BOI_NO_TMUX:-}" == "1" ]]; then
        echo ""
        echo -e "${GREEN}${BOLD}BOI installed successfully!${NC}"
        echo -e "${DIM}(Smoke test skipped — headless mode, no tmux for workers)${NC}"
        return 0
    fi

    # Skip if boi is not on PATH yet (e.g. PATH not reloaded).
    if ! command -v boi &>/dev/null; then
        echo ""
        echo -e "${GREEN}${BOLD}BOI installed successfully!${NC}"
        log_warn "'boi' not on PATH yet. Smoke test skipped."
        log_warn "Add boi to PATH then re-run the installer to verify end-to-end."
        return 0
    fi

    vlog_step "Running smoke test"

    local smoke_spec
    smoke_spec=$(mktemp "/tmp/boi-smoke-XXXXXX.spec.md")
    _write_smoke_spec "${smoke_spec}"

    # First attempt
    if _run_smoke_once "${smoke_spec}" 120; then
        rm -f "${smoke_spec}" /tmp/boi-smoke-result.txt
        step_ok "Smoke test"
        echo ""
        echo -e "${GREEN}${BOLD}BOI is fully operational. Smoke test passed.${NC}"
        return 0
    fi

    # Remediation: restart daemon, then retry once.
    vlog_warn "Attempting remediation: restarting daemon..."

    local pid_file="${BOI_STATE_DIR}/daemon.pid"
    if [[ -f "${pid_file}" ]]; then
        local pid
        pid=$(cat "${pid_file}" 2>/dev/null || true)
        [[ -n "${pid}" ]] && kill "${pid}" 2>/dev/null || true
        rm -f "${pid_file}"
        sleep 2
    fi

    if ! _start_daemon_once 15; then
        log_error "Daemon failed to restart."
        log_error "Manual start: python3 ${BOI_SRC_DIR}/daemon.py --foreground"
        rm -f "${smoke_spec}" /tmp/boi-smoke-result.txt
        step_fail "Smoke test"
        return 1
    fi
    _verify_heartbeat 15 || log_warn "Heartbeat not detected after restart. Retrying anyway."

    # Write a fresh spec for the retry (avoid duplicate-spec detection).
    rm -f "${smoke_spec}"
    smoke_spec=$(mktemp "/tmp/boi-smoke-XXXXXX.spec.md")
    _write_smoke_spec "${smoke_spec}"

    vlog_info "Retrying smoke test..."
    if _run_smoke_once "${smoke_spec}" 120; then
        rm -f "${smoke_spec}" /tmp/boi-smoke-result.txt
        step_ok "Smoke test"
        echo ""
        echo -e "${GREEN}${BOLD}BOI is fully operational. Smoke test passed.${NC}"
        return 0
    fi

    log_error "Smoke test failed after remediation and retry."
    log_error "Daemon logs: ${BOI_STATE_DIR}/logs/daemon-startup.log"
    log_error "Run 'boi doctor' for diagnostics."
    rm -f "${smoke_spec}" /tmp/boi-smoke-result.txt
    step_fail "Smoke test"
    return 1
}

# ─── Verify ──────────────────────────────────────────────────────────────────

verify_install() {
    vlog_step "Verifying installation"

    local ok=true

    # Check source directory
    if [[ -d "${BOI_SRC_DIR}" ]] && [[ -f "${BOI_SRC_DIR}/boi.sh" ]]; then
        vlog_info "Source: ${BOI_SRC_DIR}"
    else
        log_error "Source directory missing or incomplete: ${BOI_SRC_DIR}"
        ok=false
    fi

    # Check state directories
    for dir in queue events logs hooks critic worktrees projects; do
        if [[ -d "${BOI_STATE_DIR}/${dir}" ]]; then
            vlog_info "Dir:    ${BOI_STATE_DIR}/${dir}"
        else
            log_error "Missing: ${BOI_STATE_DIR}/${dir}"
            ok=false
        fi
    done

    # Check workers
    if [[ "${SKIP_WORKERS}" == "false" ]]; then
        local worker_count=0
        for i in $(seq 1 "${WORKER_COUNT}"); do
            if [[ -d "${BOI_STATE_DIR}/worktrees/boi-worker-${i}" ]]; then
                worker_count=$((worker_count + 1))
            fi
        done
        if [[ "${worker_count}" -gt 0 ]]; then
            vlog_info "Workers: ${worker_count} ready"
        else
            log_warn "Workers: none created (run from a terminal: bash ${BOI_SRC_DIR}/install-public.sh)"
        fi
    fi

    # Check symlink
    if [[ "${NO_SYMLINK}" == "false" ]]; then
        if command -v boi &>/dev/null; then
            local ver
            ver=$(boi --version 2>/dev/null || echo "unknown")
            vlog_info "boi:    ${ver}"
        else
            log_warn "'boi' command not on PATH yet. See PATH instructions above."
        fi
    fi

    if [[ "${ok}" == "true" ]]; then
        return 0
    fi
    return 1
}

# ─── Main ────────────────────────────────────────────────────────────────────

main() {
    echo ""
    echo -e "${BOLD}BOI Installer${NC} — Beginning of Infinity"
    echo ""

    parse_args "$@"

    if [[ "${VERBOSE}" == "true" ]]; then
        log_info "Install prefix: ${PREFIX}"
        if [[ "${UPDATE_MODE}" == "true" ]]; then
            log_info "Mode: update"
        else
            log_info "Mode: fresh install"
        fi
        echo ""
    fi

    check_prerequisites
    clone_or_update_repo
    create_directories
    seed_runtime_config
    seed_guardrails
    step_ok "State"
    setup_workers
    merge_critic_checks
    sync_phase_files
    create_symlink
    install_plugin
    start_and_verify_daemon
    verify_install
    run_smoke_test

    if [[ "${VERBOSE}" == "true" ]]; then
        echo ""
        echo "  Source:  ${BOI_SRC_DIR}"
        echo "  State:   ${BOI_STATE_DIR}"
        if [[ "${NO_SYMLINK}" == "false" ]]; then
            echo "  Command: boi"
        fi
    fi
    echo ""
    echo "Next steps:"
    echo "  1. Dispatch a spec:    boi dispatch --spec spec.md"
    echo "  2. Check status:       boi status"
    echo "  3. Live dashboard:     boi status --watch"
    echo ""
}

main "$@"
