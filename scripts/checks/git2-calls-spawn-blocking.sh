#!/usr/bin/env bash
# 8c S2 (Batch-C-deferred → Phase 10.6) — the git2 / spawn-blocking lint.
#
# `git2` (libgit2 bindings) is a BLOCKING library — repository / worktree /
# merge operations are synchronous FFI + disk I/O. Called bare from an
# `async fn` on a tokio worker thread they stall the runtime. Every git2 call
# site must run inside `tokio::task::spawn_blocking`.
#
# This is a CO-OCCURRENCE heuristic (not a full data-flow analysis): a source
# file that names `git2::` must ALSO contain a `spawn_blocking`. It cannot
# prove the git2 call is *inside* the `spawn_blocking` closure — that is
# code-review's job — but it loudly catches a file that touches git2 with no
# `spawn_blocking` anywhere, which is unambiguously wrong.
#
# `src/runtime/git_ops.rs` — the lowest-level git2 LAYER definition — is
# excluded: it defines the blocking git primitives as plain (sync) `fn`s; its
# `async` callers (`worktree.rs`, `tool_host.rs`, `steps_executor.rs`) are
# what must wrap them in `spawn_blocking`.
#
# An optional first argument overrides the repo root (the regression harness
# `test-spawn-blocking-lints.sh` points this at a synthetic tree).
set -uo pipefail
root="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
cd "${root}"

# Files that name `git2::` — EXCLUDING the `runtime/git_ops.rs` definition
# AND every `mod.rs` (a `mod.rs` only re-exports / declares — never calls).
users="$(
    grep -rlE 'git2::' \
        --include='*.rs' --exclude-dir=target src 2>/dev/null \
        | grep -v '^src/runtime/git_ops.rs$' \
        | grep -v '/mod\.rs$' \
        || true
)"

violations=""
for f in ${users}; do
    if ! grep -q 'spawn_blocking' "${f}"; then
        violations="${violations}  ${f}\n"
    fi
done

if [ -n "${violations}" ]; then
    echo "LINT FAIL: these files use git2 with no tokio::task::spawn_blocking"
    echo "anywhere — a blocking git2 call on an async worker thread stalls"
    echo "the runtime:"
    printf "%b" "${violations}"
    exit 1
fi
echo "OK: every git2 call site co-occurs with spawn_blocking"
