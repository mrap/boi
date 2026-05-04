#!/usr/bin/env bash
# Tests for install_completions() in install.sh
set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SH="${REPO_DIR}/install.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  OK: $*"; }

# Create a mock boi binary at $1 that outputs deterministic completion content
make_mock_boi() {
    local bin="$1"
    cat > "$bin" << 'MOCKEOF'
#!/bin/bash
case "$1" in
    completions)
        case "${2:-}" in
            zsh)  printf '#compdef boi\n_boi() { : ; }\n_boi\n' ;;
            bash) printf '# bash completion for boi\ncomplete -F _boi boi\n' ;;
            fish) printf '# fish completion for boi\ncomplete -c boi -f\n' ;;
            *)    echo "unknown shell: ${2:-}" >&2; exit 1 ;;
        esac
        ;;
    *) echo "unknown command: ${1:-}" >&2; exit 1 ;;
esac
MOCKEOF
    chmod +x "$bin"
}

# Extract the install_completions function definition from install.sh
get_fn() {
    sed -n '/^install_completions() {/,/^}/p' "$INSTALL_SH"
}

# Run install_completions in an isolated subshell with a fake HOME.
# Stderr is redirected to $3; exits non-zero only on internal script errors.
run_fn() {
    local fake_home="$1" boi_bin="$2" stderr_file="$3"
    (
        set -uo pipefail
        HOME="$fake_home"
        BOI_BIN="$boi_bin"
        DRY_RUN=false
        fn_def="$(get_fn)"
        if [[ -z "$fn_def" ]]; then
            echo "could not extract install_completions from $INSTALL_SH" >&2
            exit 1
        fi
        eval "$fn_def"
        install_completions
    ) 2>"$stderr_file"
}

# ---- Setup ----
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

fake_home="${tmpdir}/home"
mkdir -p "$fake_home"

mock_boi="${tmpdir}/mock_boi"
make_mock_boi "$mock_boi"

zsh_file="${fake_home}/.zfunc/_boi"
bash_file="${fake_home}/.local/share/bash-completion/completions/boi"
fish_file="${fake_home}/.config/fish/completions/boi.fish"

echo "Testing install_completions..."

# ---- Test 1: First install creates all completion files ----
echo ""
echo "Test 1: First install"

run_fn "$fake_home" "$mock_boi" "${tmpdir}/stderr1.txt" \
    || fail "run_fn exited non-zero"

[[ -f "$zsh_file"  ]] || fail "zsh completion not created: $zsh_file"
[[ -f "$bash_file" ]] || fail "bash completion not created: $bash_file"
[[ -f "$fish_file" ]] || fail "fish completion not created: $fish_file"

grep -q 'installed' "${tmpdir}/stderr1.txt" \
    || fail "First run: expected 'installed' in stderr; got: $(cat "${tmpdir}/stderr1.txt")"

ok "All 3 completion files created"
ok "'installed' message in stderr"

# ---- Test 2: Second install is idempotent ----
echo ""
echo "Test 2: Second install (idempotency)"

cp "$zsh_file"  "${tmpdir}/zsh.before"
cp "$bash_file" "${tmpdir}/bash.before"
cp "$fish_file" "${tmpdir}/fish.before"

run_fn "$fake_home" "$mock_boi" "${tmpdir}/stderr2.txt" \
    || fail "run_fn exited non-zero on second run"

diff -q "${tmpdir}/zsh.before"  "$zsh_file"  >/dev/null 2>&1 \
    || fail "zsh file changed on second run"
diff -q "${tmpdir}/bash.before" "$bash_file" >/dev/null 2>&1 \
    || fail "bash file changed on second run"
diff -q "${tmpdir}/fish.before" "$fish_file" >/dev/null 2>&1 \
    || fail "fish file changed on second run"

grep -q 'up to date' "${tmpdir}/stderr2.txt" \
    || fail "Second run: expected 'up to date' in stderr; got: $(cat "${tmpdir}/stderr2.txt")"

ok "All files unchanged on second run"
ok "'up to date' message in stderr"

# ---- Test 3: User-modified file is preserved ----
echo ""
echo "Test 3: User-modified file preservation"

echo "# user-custom entry" >> "$zsh_file"

run_fn "$fake_home" "$mock_boi" "${tmpdir}/stderr3.txt" \
    || fail "run_fn exited non-zero after user edit"

grep -q 'user-custom entry' "$zsh_file" \
    || fail "zsh file was clobbered (user edit lost)"

grep -q 'user file differs' "${tmpdir}/stderr3.txt" \
    || fail "No 'user file differs' message for modified zsh file; stderr: $(cat "${tmpdir}/stderr3.txt")"

ok "User-modified zsh file not clobbered"
ok "'user file differs' message in stderr"

echo ""
echo "All tests passed."
