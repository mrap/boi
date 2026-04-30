# Remote Container Dispatch

BOI can run bench containers and spec verify phases on remote infrastructure instead of
local Docker. The current supported backend is **Fly.io Machines**.

## When to Use Local vs Fly

| Scenario | Use |
|----------|-----|
| Quick iteration on a single spec | `local` (default) |
| Parallel bench battery (N conditions) | `fly` — N machines in parallel, no local resources consumed |
| E2E verify that needs a clean environment | `fly` — isolated, reproducible |
| CI/CD or unattended overnight runs | `fly` — scales to zero when idle |
| You don't have Docker installed locally | `fly` |

`local` is the default. `fly` requires a one-time account setup (see [fly-io-setup.md](fly-io-setup.md)).

## Architecture

```
boi bench --remote=fly
    │
    ▼
FlyDispatcher (src/remote/fly.rs)
    │  POST /v1/apps/boi-workers/machines   ← create machine
    │  GET  /v1/apps/boi-workers/machines/{id}  ← poll until stopped
    │  DELETE /v1/apps/boi-workers/machines/{id} ← cleanup
    │
    ▼
Fly.io Machine (shared-cpu-1x, 256 MB, iad)
    │  image: registry.fly.io/boi-workers:latest
    │  entrypoint: /usr/local/bin/entrypoint.sh boi run-spec
    │  env: BOI_SPEC_B64=<base64-encoded spec yaml>
    │       FLY_API_TOKEN, ANTHROPIC_API_KEY, OPENROUTER_API_KEY
    │
    ▼
boi run-spec (inside container)
    │  decode BOI_SPEC_B64 → temp spec file
    │  boi dispatch <temp-spec>
    │  poll until all tasks DONE/FAILED
    │  emit JSON: { status, tasks_total, tasks_done, tasks_failed, ... }
    │
    ▼
ContainerResult back to host
    exit_code, duration_ms, machine_id, cost_usd
```

## Cost Model

The cost guard in `FlyDispatcher::check_cost_guard()` uses:

```
estimated_cost = machine_size_rate × estimated_runtime_secs × runs
```

- **shared-cpu-1x rate**: `$0.0000026/sec` (~$0.0094/hr)
- **Default max_cost_usd**: `$10.00`
- **Typical bench run (60s)**: `~$0.0002`
- **Typical bench run (180s)**: `~$0.0005`

Dispatch is refused if `estimated_cost > max_cost_usd`. Override with `--max-cost`:

```sh
boi bench --remote=fly --max-cost 50.0 --spec battery/ --runs 10
```

## Parallel Dispatch

When running a bench battery with N conditions × M runs, `boi bench --remote=fly`
spawns up to `--concurrency` (default: 4) machines in parallel:

```sh
boi bench --remote=fly \
  --pipeline smoke:pipelines/smoke.toml \
  --pipeline full:pipelines/default.toml \
  --spec tests/bench_specs/ \
  --runs 3 \
  --concurrency 8
```

Each machine is independent — crash isolation, no shared state.

## Image Management

The container image at `registry.fly.io/boi-workers:latest` is the bench runtime.
It bakes in the BOI binary, phases, and templates so containers need no network access
beyond the Fly.io API.

Rebuild and push with:
```sh
scripts/fly-push.sh           # build, tag :latest + :v<version>, push
scripts/fly-push.sh --no-latest  # push version tag only
```

Images are tagged by BOI version (`v1.3.0`, etc.) for reproducible pinning.

## Known Limitations

- **Log retrieval**: The Fly.io Machines API `/logs` endpoint returns 404. Task-level
  details (`tasks_done`, `tasks_failed`) are not available in the bench summary —
  only overall exit code and duration. A future workaround will use an output volume
  or HTTP callback.
- **No worktree mounting**: The container clones the spec from `BOI_SPEC_B64`; it does
  not mount the host worktree. Changes made inside the container are lost after cleanup.
- **Cold start**: ~3–10s machine lifecycle overhead per run. Negligible for
  claude-powered tasks (60–180s), visible for trivial tasks (<10s).

## Setup

See [fly-io-setup.md](fly-io-setup.md) for account creation, token configuration,
and app setup.
