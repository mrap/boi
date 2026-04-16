#!/bin/bash
# test_e2e.sh — Hermetic Docker E2E test for BOI.
#
# Validates a clean install from install-public.sh and asserts post-install
# invariants: directory structure, CLI responsiveness, no personal references,
# and unit test suite integrity.
#
# Usage (via Docker):
#   docker build -f tests/Dockerfile.e2e -t boi-e2e-test .
#   docker run --rm boi-e2e-test

set -uo pipefail

# ── Harness ────────────────────────────────────────────────────────────────────
PASS=0
FAIL=0
TOTAL=0

check() {
    local label="$1"
    local result="$2"   # "pass" or "fail"
    local detail="${3:-}"
    TOTAL=$((TOTAL + 1))
    if [[ "$result" == "pass" ]]; then
        PASS=$((PASS + 1))
        echo "  [PASS] $label"
    else
        FAIL=$((FAIL + 1))
        echo "  [FAIL] $label"
        if [[ -n "$detail" ]]; then
            echo "         $detail"
        fi
    fi
}

section() {
    echo ""
    echo "==> $1"
}

BOI_HOME="${HOME}/.boi"
BOI_SRC="/tmp/boi-setup"

# ── Section 1: Install ─────────────────────────────────────────────────────────
section "1. Install — running install-public.sh"

# Pre-seed ~/.boi/src to bypass git clone (no network in Docker).
# install-public.sh skips git clone when the src dir already has a .git dir.
mkdir -p "${BOI_HOME}"
cp -r "${BOI_SRC}" "${BOI_HOME}/src"

# Add ~/.local/bin to PATH so the boi symlink is findable after install.
export PATH="${HOME}/.local/bin:${PATH}"

# BOI_NO_TMUX=1: Docker has no TTY, so tmux new-session fails.
# This flag makes workers run scripts directly instead of wrapping in tmux.
export BOI_NO_TMUX=1

INSTALL_LOG="${BOI_HOME}/install-e2e.log"
mkdir -p "$(dirname "${INSTALL_LOG}")"

bash "${BOI_SRC}/install-public.sh" \
    --workers 1 \
    --no-plugin \
    2>&1 | tee "${INSTALL_LOG}"
INSTALL_EXIT=${PIPESTATUS[0]}

echo ""
echo "install-public.sh exited with code ${INSTALL_EXIT}"

if [[ "${INSTALL_EXIT}" -eq 0 ]]; then
    check "install-public.sh exits 0" pass
else
    check "install-public.sh exits 0" fail "exit code: ${INSTALL_EXIT}"
    # Fatal: nothing else can pass if install failed.
    echo ""
    echo "========================================"
    echo "Results: ${PASS} passed, ${FAIL} failed (${TOTAL} total)"
    echo "========================================"
    exit 1
fi

if grep -q "BOI installed successfully" "${INSTALL_LOG}"; then
    check "installer printed success message" pass
else
    check "installer printed success message" fail "expected 'BOI installed successfully' in output"
fi

# ── Section 2: Directory structure ────────────────────────────────────────────
section "2. Directory structure — verifying ~/.boi/ layout"

for dir in "" queue events logs hooks critic worktrees projects; do
    target="${BOI_HOME}${dir:+/${dir}}"
    if [[ -d "${target}" ]]; then
        check "~/.boi/${dir:-<root>} exists" pass
    else
        check "~/.boi/${dir:-<root>} exists" fail "missing: ${target}"
    fi
done

if [[ -f "${BOI_HOME}/config.json" ]]; then
    check "~/.boi/config.json exists" pass
else
    check "~/.boi/config.json exists" fail
fi

# ── Section 3: CLI works ───────────────────────────────────────────────────────
section "3. CLI — verifying boi command produces output"

# Try boi via symlink first, fall back to boi.sh directly.
BOI_CMD=""
if command -v boi &>/dev/null; then
    BOI_CMD="boi"
elif [[ -f "${HOME}/.local/bin/boi" ]]; then
    BOI_CMD="bash ${HOME}/.local/bin/boi"
elif [[ -x "${BOI_HOME}/src/boi.sh" ]]; then
    BOI_CMD="bash ${BOI_HOME}/src/boi.sh"
fi

if [[ -n "${BOI_CMD}" ]]; then
    check "boi command is locatable" pass
    help_output=$(${BOI_CMD} --help 2>&1 || true)
    if [[ -n "${help_output}" ]]; then
        check "boi --help produces output" pass
    else
        check "boi --help produces output" fail "empty output"
    fi
else
    check "boi command is locatable" fail "not found in PATH or expected locations"
    check "boi --help produces output" fail "skipped — boi not found"
fi

# ── Section 4: No personal references in public source ────────────────────────
section "4. Personal references — checking lib/ and boi.sh for sensitive names"

# Scan text files only; exclude binary cache files.
# The grep searches for literal names; exit 1 (no match) is the passing condition.
PII_FOUND=0
while IFS= read -r -d '' file; do
    if grep -qil "rapadas" "${file}" 2>/dev/null; then
        echo "  WARNING: 'rapadas' found in ${file}"
        PII_FOUND=1
    fi
done < <(find "${BOI_SRC}/lib" -type f -name "*.py" -print0 2>/dev/null)

if grep -qil "rapadas" "${BOI_SRC}/boi.sh" 2>/dev/null; then
    echo "  WARNING: 'rapadas' found in boi.sh"
    PII_FOUND=1
fi

if [[ "${PII_FOUND}" -eq 0 ]]; then
    check "no 'rapadas' references in lib/*.py or boi.sh" pass
else
    check "no 'rapadas' references in lib/*.py or boi.sh" fail "personal surname found in public source"
fi

# ── Section 5: Unit tests ──────────────────────────────────────────────────────
section "5. Unit tests — running test_spec_parser.py and test_extract_target_repo.py"

cd "${BOI_SRC}"

# Install minimal test deps (pytest for collection; tests themselves use stdlib).
python3 -m pip install --quiet pytest 2>&1 | tail -3

PYTEST_OUT=$(python3 -m pytest \
    tests/test_spec_parser.py \
    tests/test_extract_target_repo.py \
    -v \
    2>&1)
PYTEST_EXIT=$?

echo "${PYTEST_OUT}" | tail -20

if [[ "${PYTEST_EXIT}" -eq 0 ]]; then
    check "test_spec_parser.py and test_extract_target_repo.py pass" pass
else
    check "test_spec_parser.py and test_extract_target_repo.py pass" fail "pytest exited ${PYTEST_EXIT}"
fi

# ── Summary ────────────────────────────────────────────────────────────────────
echo ""
echo "========================================"
echo "Results: ${PASS} passed, ${FAIL} failed (${TOTAL} total)"
echo "========================================"

[[ ${FAIL} -eq 0 ]]
