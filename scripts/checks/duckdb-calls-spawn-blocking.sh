#!/usr/bin/env bash
# 8c S2 (Batch-C-deferred → Phase 10.6) — the duckdb / spawn-blocking lint.
#
# `duckdb` is a BLOCKING library — its `open` / `query` calls do synchronous
# C++ FFI + disk I/O. Called bare from an `async fn` on a tokio worker thread
# they stall the runtime. Every duckdb call site must run inside
# `tokio::task::spawn_blocking`.
#
# This is a CO-OCCURRENCE heuristic (not a full data-flow analysis): a source
# file that USES the duckdb query API must ALSO contain a `spawn_blocking`.
# It cannot prove the duckdb call is *inside* the `spawn_blocking` closure —
# that is code-review's job — but it loudly catches a file that calls duckdb
# with no `spawn_blocking` anywhere, which is unambiguously wrong.
#
# `src/runtime/duckdb.rs` — the duckdb wrapper DEFINITION — is excluded: it
# defines the blocking primitives; its callers are what must wrap them.
#
# An optional first argument overrides the repo root (the regression harness
# `test-spawn-blocking-lints.sh` points this at a synthetic tree).
set -uo pipefail
root="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
cd "${root}"

# Files that USE the duckdb query API — `open_duckdb` / `failures_top` / a
# `runtime::duckdb` path / the `DuckHandle` type — EXCLUDING the wrapper
# definition `runtime/duckdb.rs` itself AND every `mod.rs` (a `mod.rs` only
# `pub use`-re-exports or `mod`-declares — it names the API but never CALLS
# it, so a `spawn_blocking` there would be meaningless).
users="$(
    grep -rlE 'open_duckdb|failures_top|runtime::duckdb|DuckHandle' \
        --include='*.rs' --exclude-dir=target src 2>/dev/null \
        | grep -v '^src/runtime/duckdb.rs$' \
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
    echo "LINT FAIL: these files call the blocking duckdb API with no"
    echo "tokio::task::spawn_blocking anywhere — a duckdb call on an async"
    echo "worker thread stalls the runtime:"
    printf "%b" "${violations}"
    exit 1
fi
echo "OK: every duckdb call site co-occurs with spawn_blocking"
