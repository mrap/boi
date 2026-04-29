# Pipeline Configuration (`phases/pipelines.toml`)

Pipelines define which phases run and in what order for each execution mode.

## Schema

```toml
[mode.<name>]
# Legacy: spec-level phases (pre- and post-task combined).
# If spec_post_phases is not set, spec_phases is used as spec_post_phases.
spec_phases = ["critic"]          # optional, backward compat

# v2: explicit pre/post split
spec_pre_phases  = ["spec-critique", "spec-improve"]   # run before task execution
spec_post_phases = ["doc-update", "critic", "merge", "cleanup"]  # run after all tasks

# Per-task phases (run for each task in sequence)
task_phases = ["execute", "review", "commit"]

# Max iterations of the spec-pre loop before proceeding to task execution (default: 3)
max_loops = 3
```

## Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `spec_phases` | `string[]` | `[]` | Legacy combined spec phases. Maps to `spec_post_phases` if `spec_post_phases` is empty. |
| `spec_pre_phases` | `string[]` | `[]` | Phases that run before tasks (looped up to `max_loops` times). |
| `spec_post_phases` | `string[]` | `[]` | Phases that run after all tasks complete. |
| `task_phases` | `string[]` | `["execute"]` | Phases that run per-task, in order. |
| `max_loops` | `u32` | `3` | Max iterations of the `spec_pre_phases` loop. |

## Backward Compatibility

Modes that only define `spec_phases` (v1 layout) continue to work. The parser
treats `spec_phases` as `spec_post_phases` automatically, and `spec_pre_phases`
defaults to empty (no pre-task loop).

## Pipeline Layouts

### v1 (default, challenge, discover, generate)

```
Spec-post phases (spec_phases):  [plan-critique →] critic [→ evaluate]
Per-task phases (task_phases):   execute → task-verify
```

All spec-level phases run after tasks complete. No pre-task spec loop.

### v2

```
Spec-pre loop (max 3):   spec-critique ↔ spec-improve
Per-task phases:         execute → review → commit   (commit is deterministic)
Spec-post phases:        doc-update → critic → merge → cleanup
                                              ^         ^       ^
                                              Claude    det.    det.
```

Deterministic phases (`commit`, `merge`, `cleanup`) run as plain shell operations
— they never spawn Claude, which eliminates cold-start latency for those steps.

## File Resolution

The pipeline registry file is resolved in this order:

1. `BOI_PIPELINES_FILE` environment variable
2. `~/.boi/pipelines.toml` (user override)
3. Compiled-in fallback defaults (no file required)

The repo's `phases/pipelines.toml` is loaded at build time via `CARGO_MANIFEST_DIR`.
To override a single mode without modifying the repo, copy just the relevant
`[mode.*]` section to `~/.boi/pipelines.toml`.

## v1 vs v2: When to Use Each

| | v1 (default) | v2 (opt-in) |
|---|---|---|
| **When** | Well-defined specs, fast iteration, no need for spec refinement | Complex specs that benefit from pre-task critique, or where commit/merge latency matters |
| **Spec-pre loop** | None | spec-critique ↔ spec-improve (≤3 iterations) |
| **Per-task** | execute → task-verify | execute → review → commit |
| **Spec-post** | critic (+ optional doc-update) | doc-update → critic → merge → cleanup |
| **Deterministic steps** | None | commit, merge, cleanup (no Claude cold-start) |
| **Cold-start hits** | 1 per task (execute) | 1 per task (execute) + 0 for commit/merge/cleanup |
| **Status** | Stable, default | Opt-in; default after A/B benchmarks confirm speedup |

**Choose v1 when:** Your spec tasks are already well-scoped, you want the shortest possible critical path, or you're prototyping.

**Choose v2 when:** You're running long multi-task specs where cold-start on commit/merge/cleanup adds up, or you want the pre-task critique loop to improve spec quality before execution begins.

To opt into v2, set `mode: v2` in your spec header:

```yaml
title: My Feature
mode: v2
```

Or pass `--mode v2` at dispatch time:

```bash
boi dispatch --spec my-feature.yaml --mode v2
```

## Adding a Custom Mode

```toml
# ~/.boi/pipelines.toml
[mode.my-mode]
spec_pre_phases  = ["spec-critique"]
task_phases      = ["execute", "code-review"]
spec_post_phases = ["critic"]
max_loops        = 2
```

Then dispatch with `boi dispatch --spec spec.yaml --mode my-mode`.
