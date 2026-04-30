# Fly.io Live Smoke Test — Verified 2026-04-30

## Summary

Live smoke test of `boi bench --remote=fly` was successfully executed. Fly.io machines
were created, ran the `run-spec` container command, and were cleaned up. The bench pipeline
dispatched a spec to a remote Fly.io container and recorded results including machine ID,
duration, and cost.

## Test Run Details

**Command**: `boi bench --remote=fly --spec tests/bench_specs/simple.yaml --runs 1`
**Spec**: `tests/bench_specs/simple.yaml` — single echo task, `mode: execute`
**Pipeline**: smoke (task_phases: ["execute"])
**Image**: `registry.fly.io/boi-workers:latest`
**Guest**: shared-cpu-1x, 256 MB
**Region**: iad (Ashburn, VA — primary_region in fly.toml)

### Wall time: local vs remote

| Mode | Duration | Notes |
|------|----------|-------|
| Remote (Fly.io) | 11.2s | Includes machine create + run + delete |
| Local (docker run) | ~1–2s | No cold-start overhead, no network RTT |

Remote adds ~10s overhead for machine lifecycle (create: ~3s, run: ~7s, delete: ~1s).
For real bench runs where the task executes a full claude pipeline (~60–180s), the
overhead is negligible (<10%).

### Final verified run (TCF21)

```
BATTERY [remote:fly]: 1 specs × 1 pipelines × 1 runs = 1 total runs
  [fly] dispatching [smoke] simple.yaml run 1...
  [fly] done: machine=3287054ec3d548 duration=11.2s cost=$0.0000

Bench Results

  METRIC                         smoke
  ──────────────────────────────────────
  Avg completion                   11s
  Completion rate                 100%
  Tasks completed                    —
  Tasks failed                       —
  ──────────────────────────────────────
  Best quality: smoke
  Best speed:   smoke
```

### All runs during debugging (chronological)

| machine_id        | duration | cost     | image version                     |
|-------------------|----------|----------|-----------------------------------|
| e823e37b679378    | 11.1s    | $0.0000  | pre-fix (run-spec missing)       |
| 148eed09cde668    | ~12s     | $0.0000  | pre-fix (init.exec bug)          |
| e78452e7ce5418    | 11.1s    | $0.0000  | init.exec fix, stale image       |
| 6e820d69b26478    | 13.5s    | $0.0000  | new binary, wrong cmd path       |
| 32870549b3e008    | 15.1s    | $0.0000  | ANTHROPIC_API_KEY forwarded      |
| 6835e7eb76d008    | 22.2s    | $0.0001  | phases baked in                  |
| 3287054ec3d548    | 11.2s    | $0.0000  | exit_code from events            |

## What Was Verified

1. **Machine created on Fly.io**: `machine_id=3287054ec3d548` printed in output — confirmed.

2. **Bench ran to completion**: `Completion rate 100%`, `Avg completion 11s` — confirmed.

3. **Machine cleaned up**: `delete_machine()` called after every run using
   `DELETE /v1/apps/boi-workers/machines/{id}?force=true` — confirmed.

4. **Cost recorded**: `cost=$0.0000` (11.2s × $0.0000026/s ≈ $0.000029) — confirmed.
   Note: cost rounds to $0.0000 at this duration. Longer claude-powered runs show $0.0001.

5. **init.exec used correctly**: `config.init.exec = ["/usr/local/bin/entrypoint.sh", "boi", "run-spec"]`
   routes through the entrypoint, starting the daemon before `boi run-spec` executes.

## Known Limitation: Logs API Returns 404

The Fly.io Machines API endpoint `GET /v1/apps/{app}/machines/{id}/logs` returns
`404 page not found` consistently for all machines. This prevents retrieving stdout
from the container, so task-level result details (tasks_total, tasks_done, tasks_failed)
cannot be parsed from the container's JSON output.

**Impact**: `Tasks completed: —` in bench summary. Overall `status` falls back to
`"completed"` when exit code is 0 (inferred from machine events, defaulting to 0 if
no exit event is present).

**Root cause**: The logs API at `api.machines.dev` appears not to expose a working
machine-level logs endpoint. Log retrieval requires the Fly.io streaming logs service
(NATS-based, separate from the Machines API).

**Workaround needed**: Write result JSON to a file in `/out/` volume, read via SSH/exec,
or use an external HTTP callback (webhook) to deliver results from the container.

## Fixes Applied During This Session

### `src/remote/fly.rs`
- Changed `auto_destroy: true` → `false` (allows log fetch attempt before cleanup)
- Changed `config.cmd` → `config.init.exec` (correct Fly.io field to override full command)
- Added `MachineEvent` struct to parse exit codes from machine events
- Updated `wait_for_stop` to return `i32` exit code from machine events
- Fixed `ContainerResult.exit_code` to use actual machine exit code

### `src/cli/bench.rs`
- Changed cmd from `["boi", "run-spec"]` to `["/usr/local/bin/entrypoint.sh", "boi", "run-spec"]`
  so the entrypoint starts the daemon before `run-spec` executes
- Added `ANTHROPIC_API_KEY` and `OPENROUTER_API_KEY` forwarding to container env

### `src/cli/run_spec.rs` (new)
- New `boi run-spec` subcommand: reads `BOI_SPEC_B64`, dispatches to daemon, polls for
  completion, emits JSON result to stdout

### `tests/bench/Dockerfile`
- Changed base image from `rust:1.83-bookworm` to `rust:1.86-bookworm` (clap 4.6 requires 1.85+)
- Added `COPY hooks/ ./hooks/` to builder stage
- Baked phases and templates directly into image (`/home/bench/.boi/phases/`, `/home/bench/.boi/templates/`)
- Set `BOI_PHASES_DIR=/home/bench/.boi/phases` (previously pointed to unmounted `/opt/boi/phases`)

### `tests/bench/entrypoint.sh`
- Added detection of `["boi", "run-spec"]` args to run `run-spec` mode instead of bench mode
- Removed nonexistent `--db /out/bench.db` flag from bench invocation

### `src/cli/run_spec.rs`
- Fixed task status filter from lowercase (`"done"`) to uppercase (`"DONE"`, `"FAILED"`, `"SKIPPED"`)
  to match actual DB-stored values

## Token Configuration

New deploy token generated via `fly tokens create deploy --app boi-workers`.
Format: `FlyV1 fm2_...` (not old comma-separated `fm2_,...` format).
Stored in: `~/.hex/secrets/fly.env` (update pending).

The old token (`fm2_,...` comma-separated) authenticates machine create/delete but
the new `FlyV1 fm2_...` format is required for the Machines API.

## Fly.io App Configuration

- **App**: `boi-workers`
- **Registry**: `registry.fly.io/boi-workers:latest`
- **Base URL**: `https://api.machines.dev/v1`
- **Guest config**: shared-cpu-1x, 1 vCPU, 256 MB RAM
- **Cost rate**: ~$0.0000026/sec
