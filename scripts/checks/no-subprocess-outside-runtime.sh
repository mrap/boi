#!/usr/bin/env bash
# LDA invariant: subprocess spawning is a runtime concern only.
# Allowed only in src/runtime/. Anywhere else: violation.
# Fixes consensus convergent #1: excludes target/, catches re-exports.
# Optional $1 = repo root to scan (defaults to this script's repo); the
# test-no-subprocess.sh regression harness relies on this override.
set -euo pipefail
cd "${1:-$(dirname "$0")/../..}"

violations=$(grep -rnE \
    'std::process::(Command|Stdio)|tokio::process::|process::Command' \
    --include='*.rs' \
    --exclude-dir=target \
    src/ tests/ 2>/dev/null \
    | grep -v '^src/runtime/' \
    | grep -v '^tests/' \
    || true)

if [ -n "$violations" ]; then
    echo "LINT FAIL: subprocess imports outside src/runtime/:"
    echo "$violations"
    exit 1
fi

echo "OK: no subprocess imports outside src/runtime/"
