#!/bin/bash
# install.sh — One-time setup for BOI (Beginning of Infinity).
#
# Creates N git worktrees, writes ~/.boi/config.json,
# creates runtime directories, and sets up the boi command alias.
#
# Usage:
#   bash install.sh              # Default: 3 workers
#   bash install.sh --workers 5  # Custom worker count (max 5)
#   bash install.sh --repo /path/to/repo  # Specify repo to create worktrees from
#   bash install.sh --dry-run    # Show what would happen without doing it
#   bash install.sh --skip-worktrees  # Skip worktree creation (use existing)

set -uo pipefail

# Constants
BOI_STATE_DIR="${HOME}/.boi"
BOI_CONFIG="${BOI_STATE_DIR}/config.json"
WORKTREE_DIR="${BOI_STATE_DIR}/worktrees"
WORKTREE_PREFIX="${WORKTREE_DIR}/boi-worker-"
MAX_WORKERS=5
DEFAULT_WORKERS=3
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

# Flags
WORKER_COUNT="${DEFAULT_WORKERS}"
DRY_RUN=false
SKIP_WORKTREES=false
NO_PLUGIN=false
WORKTREE_PATHS=""
REPO_PATH=""

usage() {
    echo "Usage: bash install.sh [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  --workers N              Number of worker worktrees (1-${MAX_WORKERS}, default: ${DEFAULT_WORKERS})"
    echo "  --repo PATH              Git repo to create worktrees from (default: detect current repo)"
    echo "  --worktree-paths P1,P2   Comma-separated existing worktree paths (skips creation)"
    echo "  --dry-run                Show what would happen without doing it"
    echo "  --skip-worktrees         Skip worktree creation (use pre-existing worktrees)"
    echo "  --no-plugin              Skip installing Claude Code plugin"
    echo "  -h, --help               Show this help"
}

log_info()  { echo -e "${GREEN}[boi]${NC} $1"; }
log_warn()  { echo -e "${YELLOW}[boi]${NC} $1"; }
log_error() { echo -e "${RED}[boi]${NC} $1" >&2; }
log_step()  { echo -e "${BOLD}==> $1${NC}"; }

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --workers)
            [[ -z "${2:-}" ]] && { log_error "--workers requires a number"; exit 1; }
            WORKER_COUNT="$2"
            shift 2
            ;;
        --repo)
            [[ -z "${2:-}" ]] && { log_error "--repo requires a path"; exit 1; }
            REPO_PATH="$2"
            shift 2
            ;;
        --dry-run)
            DRY_RUN=true
            shift
            ;;
        --skip-worktrees|--skip-clone)
            SKIP_WORKTREES=true
            shift
            ;;
        --no-plugin)
            NO_PLUGIN=true
            shift
            ;;
        --worktree-paths|--checkout-paths)
            [[ -z "${2:-}" ]] && { log_error "--worktree-paths requires a comma-separated list of paths"; exit 1; }
            WORKTREE_PATHS="$2"
            shift 2
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

# Parse and validate --worktree-paths if provided
CUSTOM_WORKTREE_PATHS=()
if [[ -n "${WORKTREE_PATHS}" ]]; then
    IFS=',' read -ra CUSTOM_WORKTREE_PATHS <<< "${WORKTREE_PATHS}"
    WORKER_COUNT=${#CUSTOM_WORKTREE_PATHS[@]}

    if [[ "${WORKER_COUNT}" -lt 1 ]]; then
        log_error "--worktree-paths requires at least one path"
        exit 1
    fi
    if [[ "${WORKER_COUNT}" -gt "${MAX_WORKERS}" ]]; then
        log_error "Too many worktree paths (max ${MAX_WORKERS}). Got: ${WORKER_COUNT}"
        exit 1
    fi

    for p in "${CUSTOM_WORKTREE_PATHS[@]}"; do
        if [[ "${DRY_RUN}" == "false" ]] && [[ ! -d "${p}" ]]; then
            log_error "Worktree path does not exist or is not a directory: ${p}"
            exit 1
        fi
    done

    SKIP_WORKTREES=true
fi

# Validate worker count
if ! [[ "${WORKER_COUNT}" =~ ^[0-9]+$ ]] || [[ "${WORKER_COUNT}" -lt 1 ]] || [[ "${WORKER_COUNT}" -gt "${MAX_WORKERS}" ]]; then
    log_error "Worker count must be between 1 and ${MAX_WORKERS}. Got: ${WORKER_COUNT}"
    exit 1
fi

detect_environment() {
    local env_type="unknown"
    if [[ "$(uname)" == "Linux" ]]; then
        env_type="linux"
    elif [[ "$(uname)" == "Darwin" ]]; then
        env_type="macos"
    fi
    echo "${env_type}"
}

check_not_in_claude() {
    if [[ -n "${CLAUDECODE:-}" ]]; then
        log_error "install.sh must run OUTSIDE Claude Code."
        log_error "Run this from a terminal instead."
        exit 1
    fi
}

detect_repo_root() {
    if [[ -n "${REPO_PATH}" ]]; then
        if [[ ! -d "${REPO_PATH}" ]]; then
            log_error "Repo path does not exist: ${REPO_PATH}"
            exit 1
        fi
        if ! git -C "${REPO_PATH}" rev-parse --git-dir &>/dev/null; then
            log_error "Not a git repository: ${REPO_PATH}"
            exit 1
        fi
        REPO_PATH="$(cd "${REPO_PATH}" && git rev-parse --show-toplevel)"
        return 0
    fi

    if git rev-parse --git-dir &>/dev/null; then
        REPO_PATH="$(git rev-parse --show-toplevel)"
        return 0
    fi

    log_error "Not inside a git repository. Use --repo <path> to specify one."
    exit 1
}

check_prerequisites() {
    local missing=false

    if [[ ${#CUSTOM_WORKTREE_PATHS[@]} -eq 0 ]] && ! command -v git &>/dev/null; then
        log_error "git not found. Use --worktree-paths to provide existing worktrees, or install git."
        missing=true
    fi

    if ! command -v tmux &>/dev/null; then
        log_error "tmux not found. Install with: sudo apt install tmux (Linux) or brew install tmux (macOS)"
        missing=true
    fi

    if ! command -v claude &>/dev/null; then
        log_warn "claude CLI not found. Workers will not be able to run."
    fi

    if [[ "${missing}" == "true" ]]; then
        exit 1
    fi
}

create_worktree() {
    local index="$1"
    local dest="${WORKTREE_PREFIX}${index}"

    if [[ -d "${dest}" ]]; then
        log_info "Worktree ${dest} already exists, skipping."
        return 0
    fi

    log_info "Creating worktree ${index}/${WORKER_COUNT}: ${dest}"
    if [[ "${DRY_RUN}" == "true" ]]; then
        log_info "[dry-run] Would run: git -C ${REPO_PATH} worktree add ${dest}"
        return 0
    fi

    if ! git -C "${REPO_PATH}" worktree add "${dest}" 2>/dev/null; then
        log_error "Failed to create worktree at ${dest}"
        return 1
    fi

    log_info "Worktree ${dest} created."
}

create_worktrees() {
    log_step "Creating ${WORKER_COUNT} worker worktrees"

    # Ensure the worktree parent directory exists
    if [[ "${DRY_RUN}" == "false" ]]; then
        mkdir -p "${WORKTREE_DIR}"
    fi

    local failed=0
    for i in $(seq 1 "${WORKER_COUNT}"); do
        if ! create_worktree "${i}"; then
            failed=$((failed + 1))
        fi
    done

    if [[ "${failed}" -gt 0 ]]; then
        log_error "${failed} worktree(s) failed to create."
        return 1
    fi
}

create_directories() {
    log_step "Creating runtime directories"

    local dirs=(
        "${BOI_STATE_DIR}"
        "${BOI_STATE_DIR}/queue"
        "${BOI_STATE_DIR}/events"
        "${BOI_STATE_DIR}/logs"
        "${BOI_STATE_DIR}/hooks"
        "${BOI_STATE_DIR}/critic/custom"
        "${WORKTREE_DIR}"
    )

    for dir in "${dirs[@]}"; do
        if [[ "${DRY_RUN}" == "true" ]]; then
            log_info "[dry-run] Would create: ${dir}"
        else
            mkdir -p "${dir}"
            log_info "Created: ${dir}"
        fi
    done

    # Create critic config with defaults
    if [[ "${DRY_RUN}" == "true" ]]; then
        if [[ ! -f "${BOI_STATE_DIR}/critic/config.json" ]]; then
            log_info "[dry-run] Would create critic config with defaults"
        fi
    else
        if [[ ! -f "${BOI_STATE_DIR}/critic/config.json" ]]; then
            cat > "${BOI_STATE_DIR}/critic/config.json" << 'CRITIC_EOF'
{
  "enabled": true,
  "trigger": "on_complete",
  "max_passes": 2,
  "checks": ["spec-integrity", "verify-commands", "code-quality", "completeness", "fleet-readiness"],
  "custom_checks_dir": "custom",
  "timeout_seconds": 600
}
CRITIC_EOF
            log_info "Created critic config with defaults"
        fi
    fi
}

write_config() {
    log_step "Writing config to ${BOI_CONFIG}"

    local json_workers="["
    local first=true
    for i in $(seq 1 "${WORKER_COUNT}"); do
        local path
        if [[ ${#CUSTOM_WORKTREE_PATHS[@]} -gt 0 ]]; then
            path="${CUSTOM_WORKTREE_PATHS[$((i - 1))]}"
        else
            path="${WORKTREE_PREFIX}${i}"
        fi
        if [[ "${first}" == "true" ]]; then
            first=false
        else
            json_workers+=","
        fi
        json_workers+="{\"id\":\"w-${i}\",\"worktree_path\":\"${path}\",\"status\":\"idle\"}"
    done
    json_workers+="]"

    local env_type
    env_type=$(detect_environment)
    local timestamp
    timestamp=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

    local repo_path_json="${REPO_PATH:-}"

    local config
    config=$(cat <<ENDJSON
{
  "version": "1",
  "tool": "boi",
  "created_at": "${timestamp}",
  "environment": "${env_type}",
  "worker_count": ${WORKER_COUNT},
  "worktree_prefix": "${WORKTREE_PREFIX}",
  "repo_path": "${repo_path_json}",
  "workers": ${json_workers},
  "daemon": {
    "poll_interval_s": 5,
    "pid_file": "${BOI_STATE_DIR}/daemon.pid",
    "log_dir": "${BOI_STATE_DIR}/logs"
  },
  "boi_dir": "${SCRIPT_DIR}"
}
ENDJSON
)

    if [[ "${DRY_RUN}" == "true" ]]; then
        log_info "[dry-run] Would write config:"
        echo "${config}"
        return 0
    fi

    local tmp="${BOI_CONFIG}.tmp"
    echo "${config}" > "${tmp}"
    mv "${tmp}" "${BOI_CONFIG}"

    log_info "Config written."
}

setup_alias() {
    log_step "Setting up boi command"

    local bin_dir="${HOME}/bin"
    local symlink_path="${bin_dir}/boi"
    local target="${SCRIPT_DIR}/boi.sh"

    if [[ "${DRY_RUN}" == "true" ]]; then
        log_info "[dry-run] Would create: ${symlink_path} -> ${target}"
        return 0
    fi

    if [[ ! -d "${bin_dir}" ]]; then
        mkdir -p "${bin_dir}"
        log_info "Created: ${bin_dir}"
    fi

    if [[ -L "${symlink_path}" ]]; then
        local existing
        existing=$(readlink "${symlink_path}")
        if [[ "${existing}" == "${target}" ]]; then
            log_info "Symlink already correct: ${symlink_path} -> ${target}"
        else
            ln -sf "${target}" "${symlink_path}"
            log_info "Updated symlink: ${symlink_path} -> ${target}"
        fi
    elif [[ -e "${symlink_path}" ]]; then
        log_warn "${symlink_path} exists but is not a symlink. Skipping."
        return 0
    else
        ln -s "${target}" "${symlink_path}"
        log_info "Created symlink: ${symlink_path} -> ${target}"
    fi

    if [[ ":${PATH}:" != *":${bin_dir}:"* ]]; then
        log_warn "${bin_dir} is not on your PATH."
        log_warn "Add to your shell config: export PATH=\"\${HOME}/bin:\${PATH}\""
    fi

    # Also add shell alias to .zshrc/.bashrc
    local shell_rc=""
    if [[ -f "${HOME}/.zshrc" ]]; then
        shell_rc="${HOME}/.zshrc"
    elif [[ -f "${HOME}/.bashrc" ]]; then
        shell_rc="${HOME}/.bashrc"
    fi

    if [[ -n "${shell_rc}" ]]; then
        if ! grep -q "alias boi=" "${shell_rc}" 2>/dev/null; then
            echo "" >> "${shell_rc}"
            echo "# BOI (Beginning of Infinity)" >> "${shell_rc}"
            echo "alias boi='bash ${target}'" >> "${shell_rc}"
            log_info "Added boi alias to ${shell_rc}"
        else
            log_info "boi alias already exists in ${shell_rc}"
        fi
    fi
}

install_plugin() {
    if [[ "${NO_PLUGIN}" == "true" ]]; then
        log_info "Skipping Claude Code plugin (--no-plugin)"
        return 0
    fi

    if [[ "${DRY_RUN}" == "true" ]]; then
        log_info "[dry-run] Would install Claude Code plugin"
        return 0
    fi

    log_step "Installing Claude Code plugin"

    local claude_dir="${HOME}/.claude"
    if [[ -d "${claude_dir}" ]]; then
        mkdir -p "${claude_dir}/skills/boi" "${claude_dir}/commands"

        # Copy skill
        cp "${SCRIPT_DIR}/plugin/skills/boi/SKILL.md" "${claude_dir}/skills/boi/SKILL.md"

        # Copy command
        cp "${SCRIPT_DIR}/plugin/commands/boi.md" "${claude_dir}/commands/boi.md"

        log_info "Claude Code plugin installed (/boi command + BOI skill)"
    else
        log_info "Claude Code not detected. Skip plugin install."
        log_info "To install later: cp -r ${SCRIPT_DIR}/plugin/skills/boi ~/.claude/skills/"
    fi
}

verify_install() {
    log_step "Verifying installation"

    local ok=true

    if [[ ! -f "${BOI_CONFIG}" ]]; then
        log_error "Config file missing: ${BOI_CONFIG}"
        ok=false
    else
        log_info "Config: ${BOI_CONFIG}"
    fi

    for dir in queue events logs hooks critic; do
        if [[ ! -d "${BOI_STATE_DIR}/${dir}" ]]; then
            log_error "Directory missing: ${BOI_STATE_DIR}/${dir}"
            ok=false
        fi
    done

    local worktree_count=0
    for i in $(seq 1 "${WORKER_COUNT}"); do
        local dest
        if [[ ${#CUSTOM_WORKTREE_PATHS[@]} -gt 0 ]]; then
            dest="${CUSTOM_WORKTREE_PATHS[$((i - 1))]}"
        else
            dest="${WORKTREE_PREFIX}${i}"
        fi
        if [[ -d "${dest}" ]]; then
            worktree_count=$((worktree_count + 1))
            log_info "Worktree ${i}: ${dest}"
        else
            log_warn "Worktree ${i} missing: ${dest}"
        fi
    done

    log_info "Worktrees: ${worktree_count}/${WORKER_COUNT} available"

    if [[ "${ok}" == "true" && "${worktree_count}" -gt 0 ]]; then
        return 0
    else
        return 1
    fi
}

main() {
    echo ""
    echo -e "${BOLD}BOI Installer${NC} — Beginning of Infinity"
    echo "============================================"
    echo ""

    local env_type
    env_type=$(detect_environment)
    log_info "Environment: ${env_type}"
    log_info "Workers: ${WORKER_COUNT}"
    if [[ ${#CUSTOM_WORKTREE_PATHS[@]} -gt 0 ]]; then
        log_info "Worktree paths: custom (${WORKER_COUNT} provided)"
    else
        log_info "Worktree prefix: ${WORKTREE_PREFIX}"
    fi

    if [[ "${DRY_RUN}" == "true" ]]; then
        log_warn "DRY RUN — no changes will be made"
    fi

    echo ""

    check_not_in_claude
    check_prerequisites

    # Detect repo root if we need to create worktrees
    if [[ "${SKIP_WORKTREES}" == "false" ]] && [[ ${#CUSTOM_WORKTREE_PATHS[@]} -eq 0 ]]; then
        detect_repo_root
        log_info "Repo: ${REPO_PATH}"
    fi

    create_directories
    echo ""

    if [[ "${SKIP_WORKTREES}" == "true" ]]; then
        log_info "Skipping worktree creation (--skip-worktrees)"
    else
        create_worktrees
    fi
    echo ""

    write_config
    echo ""

    setup_alias
    echo ""

    install_plugin
    echo ""

    if [[ "${DRY_RUN}" == "false" ]]; then
        verify_install
        echo ""
    fi

    if [[ "${DRY_RUN}" == "true" ]]; then
        log_info "Dry run complete. No changes were made."
    else
        echo -e "${GREEN}${BOLD}BOI installed successfully.${NC}"
        echo ""
        echo "  State dir:  ${BOI_STATE_DIR}"
        echo "  Config:     ${BOI_CONFIG}"
        if [[ ${#CUSTOM_WORKTREE_PATHS[@]} -gt 0 ]]; then
            echo "  Worktrees:  ${WORKER_COUNT} custom paths"
        else
            echo "  Worktrees:  ${WORKTREE_PREFIX}{1..${WORKER_COUNT}}"
        fi
        echo "  Command:    boi"
        echo ""
        echo "Next steps:"
        echo "  1. Dispatch a spec:    boi dispatch --spec spec.md"
        echo "  2. Check status:       boi status"
        echo "  3. Live dashboard:     boi status --watch"
    fi
}

main
