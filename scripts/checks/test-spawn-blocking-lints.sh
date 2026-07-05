#!/usr/bin/env bash
# Regression harness for the spawn-blocking lints (Phase 10.6) —
# `duckdb-calls-spawn-blocking.sh` + `git2-calls-spawn-blocking.sh`.
#
# Proves each lint CATCHES a blocking-API call site with no `spawn_blocking`
# and PASSES one that has it — a false-green lint is worse than none.
set -uo pipefail
here="$(dirname "$0")"
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

reset() { rm -rf "${tmp}/src"; mkdir -p "${tmp}/src/cli" "${tmp}/src/runtime"; }

# === duckdb lint =========================================================

# 1. A duckdb call site with NO spawn_blocking → CAUGHT.
reset
cat > "${tmp}/src/cli/traces.rs" <<'RS'
use crate::runtime::open_duckdb;
pub async fn run() { let _h = open_duckdb("x"); }
RS
bash "${here}/duckdb-calls-spawn-blocking.sh" "${tmp}" >/dev/null 2>&1 \
    && { echo "FAIL: a bare duckdb call site was not caught"; exit 1; }

# 2. The same call site WITH spawn_blocking → PASSES.
reset
cat > "${tmp}/src/cli/traces.rs" <<'RS'
use crate::runtime::open_duckdb;
pub async fn run() {
    tokio::task::spawn_blocking(|| { let _h = open_duckdb("x"); }).await.ok();
}
RS
bash "${here}/duckdb-calls-spawn-blocking.sh" "${tmp}" >/dev/null 2>&1 \
    || { echo "FAIL: a spawn_blocking-wrapped duckdb call was rejected"; exit 1; }

# 3. The duckdb-wrapper DEFINITION (`runtime/duckdb.rs`) is exempt — it
#    defines the blocking primitive; a bare call there is not a violation.
reset
cat > "${tmp}/src/runtime/duckdb.rs" <<'RS'
pub fn open_duckdb(_p: &str) {}
RS
bash "${here}/duckdb-calls-spawn-blocking.sh" "${tmp}" >/dev/null 2>&1 \
    || { echo "FAIL: the runtime/duckdb.rs definition was wrongly flagged"; exit 1; }

# === git2 lint ===========================================================

# 4. A git2 call site with NO spawn_blocking → CAUGHT.
reset
cat > "${tmp}/src/runtime/worktree.rs" <<'RS'
pub async fn prepare() { let _r = git2::Repository::open("."); }
RS
bash "${here}/git2-calls-spawn-blocking.sh" "${tmp}" >/dev/null 2>&1 \
    && { echo "FAIL: a bare git2 call site was not caught"; exit 1; }

# 5. The same git2 call site WITH spawn_blocking → PASSES.
reset
cat > "${tmp}/src/runtime/worktree.rs" <<'RS'
pub async fn prepare() {
    tokio::task::spawn_blocking(|| { let _r = git2::Repository::open("."); })
        .await.ok();
}
RS
bash "${here}/git2-calls-spawn-blocking.sh" "${tmp}" >/dev/null 2>&1 \
    || { echo "FAIL: a spawn_blocking-wrapped git2 call was rejected"; exit 1; }

# 6. The `runtime/git_ops.rs` git2-layer DEFINITION is exempt — it defines the
#    blocking git primitives as plain sync fns.
reset
cat > "${tmp}/src/runtime/git_ops.rs" <<'RS'
pub fn open(_p: &str) { let _r = git2::Repository::open("."); }
RS
bash "${here}/git2-calls-spawn-blocking.sh" "${tmp}" >/dev/null 2>&1 \
    || { echo "FAIL: the runtime/git_ops.rs definition was wrongly flagged"; exit 1; }

echo "OK: spawn-blocking lints regression passed"
