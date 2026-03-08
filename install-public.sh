#!/bin/bash
# install-public.sh — Public installer for BOI (Beginning of Infinity).
#
# Install BOI on any macOS or Linux machine:
#   curl -fsSL https://raw.githubusercontent.com/your-org/boi/main/install-public.sh | bash
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

BOI_REPO="https://github.com/your-org/boi.git"
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
UPDATE_MODE=false

# ─── Logging ─────────────────────────────────────────────────────────────────

log_info()  { echo -e "${GREEN}[boi]${NC} $1"; }
log_warn()  { echo -e "${YELLOW}[boi]${NC} $1"; }
log_error() { echo -e "${RED}[boi]${NC} $1" >&2; }
log_step()  { echo -e "\n${BOLD}==> $1${NC}"; }

# ─── Usage ───────────────────────────────────────────────────────────────────

usage() {
    cat <<EOF
BOI Installer — Beginning of Infinity

Usage:
  bash install-public.sh [OPTIONS]
  curl -fsSL <url>/install-public.sh | bash

Options:
  --prefix <path>    Install location (default: ~/.boi)
  --no-symlink       Skip creating the 'boi' symlink in PATH
  --update           Update an existing installation
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
            --no-symlink)
                NO_SYMLINK=true
                shift
                ;;
            --update)
                UPDATE_MODE=true
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

    log_info "Python: ${version} (${python_cmd})"
    return 0
}

check_prerequisites() {
    log_step "Checking prerequisites"

    local failed=false

    if ! check_command "bash" "bash is required."; then
        failed=true
    else
        log_info "bash: $(bash --version | head -1 | sed 's/GNU bash, version //' | cut -d' ' -f1)"
    fi

    if ! check_python_version; then
        failed=true
    fi

    if ! check_command "git" "Install: https://git-scm.com/downloads"; then
        failed=true
    else
        log_info "git: $(git --version | sed 's/git version //')"
    fi

    if ! check_command "tmux" "Install: sudo apt install tmux (Linux) or brew install tmux (macOS)"; then
        failed=true
    else
        log_info "tmux: $(tmux -V | sed 's/tmux //')"
    fi

    if [[ "${failed}" == "true" ]]; then
        echo ""
        log_error "Missing prerequisites. Install the above and retry."
        exit 1
    fi

    log_info "All prerequisites met."
}

# ─── Clone / Update Repo ────────────────────────────────────────────────────

clone_or_update_repo() {
    log_step "Setting up BOI source"

    if [[ -d "${BOI_SRC_DIR}/.git" ]]; then
        if [[ "${UPDATE_MODE}" == "true" ]]; then
            log_info "Updating existing installation at ${BOI_SRC_DIR}"
            if ! git -C "${BOI_SRC_DIR}" pull --rebase --quiet 2>/dev/null; then
                log_warn "git pull failed. Trying fetch + reset."
                git -C "${BOI_SRC_DIR}" fetch origin
                git -C "${BOI_SRC_DIR}" reset --hard origin/main
            fi
            log_info "Source updated."
        else
            log_info "BOI source already exists at ${BOI_SRC_DIR}"
            log_info "Use --update to pull latest changes."
        fi
    else
        log_info "Cloning BOI to ${BOI_SRC_DIR}"
        mkdir -p "$(dirname "${BOI_SRC_DIR}")"
        if ! git clone "${BOI_REPO}" "${BOI_SRC_DIR}" 2>&1; then
            log_error "Failed to clone BOI repository."
            log_error "Check your network connection and try again."
            exit 1
        fi
        log_info "Source cloned."
    fi
}

# ─── Directory Structure ────────────────────────────────────────────────────

create_directories() {
    log_step "Creating state directories"

    local dirs=(
        "${BOI_STATE_DIR}"
        "${BOI_STATE_DIR}/queue"
        "${BOI_STATE_DIR}/events"
        "${BOI_STATE_DIR}/logs"
        "${BOI_STATE_DIR}/hooks"
        "${BOI_STATE_DIR}/worktrees"
        "${BOI_STATE_DIR}/projects"
    )

    for dir in "${dirs[@]}"; do
        if [[ ! -d "${dir}" ]]; then
            mkdir -p "${dir}"
            log_info "Created: ${dir}"
        else
            log_info "Exists:  ${dir}"
        fi
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
        log_info "Skipping symlink creation (--no-symlink)"
        return 0
    fi

    log_step "Creating boi command"

    determine_symlink_dir

    local target="${BOI_SRC_DIR}/boi.sh"
    local symlink="${SYMLINK_DIR}/boi"

    if [[ ! -f "${target}" ]]; then
        log_error "boi.sh not found at ${target}"
        log_error "The clone may have failed. Check ${BOI_SRC_DIR}/"
        return 1
    fi

    # Ensure boi.sh is executable
    chmod +x "${target}"

    if [[ -L "${symlink}" ]]; then
        local existing
        existing=$(readlink "${symlink}")
        if [[ "${existing}" == "${target}" ]]; then
            log_info "Symlink already correct: ${symlink} -> ${target}"
        else
            ln -sf "${target}" "${symlink}"
            log_info "Updated symlink: ${symlink} -> ${target}"
        fi
    elif [[ -e "${symlink}" ]]; then
        log_warn "${symlink} exists but is not a symlink. Skipping."
        log_warn "You can manually link: ln -sf ${target} ${symlink}"
        return 0
    else
        ln -s "${target}" "${symlink}"
        log_info "Created symlink: ${symlink} -> ${target}"
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
}

# ─── Verify ──────────────────────────────────────────────────────────────────

verify_install() {
    log_step "Verifying installation"

    local ok=true

    # Check source directory
    if [[ -d "${BOI_SRC_DIR}" ]] && [[ -f "${BOI_SRC_DIR}/boi.sh" ]]; then
        log_info "Source: ${BOI_SRC_DIR}"
    else
        log_error "Source directory missing or incomplete: ${BOI_SRC_DIR}"
        ok=false
    fi

    # Check state directories
    for dir in queue events logs hooks worktrees projects; do
        if [[ -d "${BOI_STATE_DIR}/${dir}" ]]; then
            log_info "Dir:    ${BOI_STATE_DIR}/${dir}"
        else
            log_error "Missing: ${BOI_STATE_DIR}/${dir}"
            ok=false
        fi
    done

    # Check symlink
    if [[ "${NO_SYMLINK}" == "false" ]]; then
        if command -v boi &>/dev/null; then
            local ver
            ver=$(boi --version 2>/dev/null || echo "unknown")
            log_info "boi:    ${ver}"
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
    echo "============================================"
    echo ""

    parse_args "$@"

    log_info "Install prefix: ${PREFIX}"
    if [[ "${UPDATE_MODE}" == "true" ]]; then
        log_info "Mode: update"
    else
        log_info "Mode: fresh install"
    fi
    echo ""

    check_prerequisites
    clone_or_update_repo
    create_directories
    create_symlink
    verify_install

    echo ""
    echo -e "${GREEN}${BOLD}BOI installed successfully!${NC}"
    echo ""
    echo "  Source:  ${BOI_SRC_DIR}"
    echo "  State:   ${BOI_STATE_DIR}"
    if [[ "${NO_SYMLINK}" == "false" ]]; then
        echo "  Command: boi"
    fi
    echo ""
    echo "Next steps:"
    echo "  1. Set up workers:     boi install --workers 3"
    echo "  2. Dispatch a spec:    boi dispatch --spec spec.md"
    echo "  3. Check status:       boi status"
    echo "  4. Live dashboard:     boi status --watch"
    echo ""
}

main "$@"
