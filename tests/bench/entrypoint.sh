#!/usr/bin/env bash
# Container entrypoint: initialize BOI, start daemon, run bench, emit results.
# All args are forwarded to `boi bench`.
#
# Expected mounts:
#   /opt/boi  (RO) — BOI source: phases/, pipelines/, bench_specs/
#   /out      (RW) — results + logs written here

set -uo pipefail

# ── Init BOI home ─────────────────────────────────────────────────────────────
mkdir -p "$BOI_HOME/worktrees" \
         "$BOI_HOME/logs" \
         "$BOI_HOME/telemetry" \
         "$BOI_HOME/phases"

# Silence git dubious-ownership warnings on root-owned mounted source
git config --global --add safe.directory /opt/boi 2>/dev/null || true
git config --global user.email "bench@boi.local" 2>/dev/null || true
git config --global user.name "BOI Bench" 2>/dev/null || true

# Sync phases + pipelines.toml from mounted source so boi picks up current state
if [[ -d /opt/boi/phases ]]; then
    cp -r /opt/boi/phases/. "$BOI_HOME/phases/" 2>/dev/null || true
fi
if [[ -f /opt/boi/phases/pipelines.toml ]]; then
    cp /opt/boi/phases/pipelines.toml "$BOI_HOME/pipelines.toml" 2>/dev/null || true
fi

# ── Start BOI daemon in background ───────────────────────────────────────────
boi daemon start
# Give the daemon poll loop time to initialize before enqueuing work
sleep 3

# ── Run bench with caller-supplied args ──────────────────────────────────────
boi bench --db /out/bench.db "$@"
rc=$?

# ── Teardown ──────────────────────────────────────────────────────────────────
boi daemon stop 2>/dev/null || true

exit $rc
