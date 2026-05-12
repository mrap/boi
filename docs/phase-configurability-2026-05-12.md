# Phase configurability — 2026-05-12

## Why

BOI's worker was getting stuck in a `spec-review` loop forever. Root cause: the
worker state machine entered `WorkerState::SpecPhase { phase_idx: 0 }`
unconditionally on every worker start, even after restarts where all tasks were
already `DONE` in the DB. Restarted workers kept re-running the pre-spec
phases (multi-minute opus calls) and getting killed before completion.

Underneath that was a deeper architectural leak: phases advertised themselves
as declaratively configurable (`[phase] level = "spec"|"task"`, etc.), but
whenever a field was omitted, BOI silently inferred semantics from the phase
**name** using a hardcoded 7-name vocabulary. Rename `spec-review` to
`spec-plan` in a TOML without explicitly setting `level`, and the entire
pipeline shape silently changed.

## What changed

### 1. Worker entry state — declarative

`fn initial_worker_state(order, done_ids, pre_spec_phases) -> Result<WorkerState, String>`
in `src/worker.rs` computes the entry state from the pipeline declaration plus
the DB. Branches:

| Condition | Initial state |
|---|---|
| `order` empty | `Cleanup { success: true }` |
| `done_ids.len() == order.len()` (all tasks done — typical restart case) | `PostTaskSpecPhase { phase_idx: 0 }` |
| `pre_spec_phases` empty | `TaskSelect` |
| default | `SpecPhase { phase_idx: 0 }` |
| `done_ids` contains an id not in `order` | **Err** (loud DB-corruption signal; caller marks spec failed) |

### 2. Phase TOML fields — explicit required

The three magic-string derive functions (`derive_level`, `derive_can_add_tasks`,
`derive_can_fail_spec`) were deleted from `src/phases.rs`. `PhaseConfig::from_toml`
now returns `Result<Self, String>` with `Err` naming the missing field and the
file path. All three are now required:

```toml
[phase]
level         = "spec"   # or "task"
can_add_tasks = false    # or true
can_fail_spec = false    # or true
```

Old derive rules (preserved as the migration default — every in-repo phase TOML
was migrated to keep its prior behavior):

- `can_add_tasks = true` for `critic`, `decompose`, `evaluate`, `plan-critique`,
  `code-review`, `review`, `spec-review`, `spec-critique`; otherwise `false`.
- `can_fail_spec = true` for `plan-critique`, `critic`; otherwise `false`.

### 3. Pipeline TOML — explicit pre/post

Pipeline modes must declare `spec_pre_phases` and `spec_post_phases` explicitly.
The legacy `spec_phases = [...]` field is still parsed (backward compat for old
in-repo experiments) but `worker.rs` no longer infers pre/post placement from
name strings. A loud `WARN` fires at load time when a pipeline mode uses the
old legacy shape with no explicit pre/post — including a pointer to this doc.

Migration from legacy `spec_phases` to explicit pre/post (the rule the deleted
worker.rs:595-613 fallback used):

- `spec_pre_phases` ← any spec-level phases named `spec-review` or `plan-critique`
- `spec_post_phases` ← all other spec-level phases

In-repo `phases/pipelines.toml` and the hardcoded `fallback_pipeline()` in
`src/phases.rs` have both been migrated.

### 4. Loud-failure on phase load

`load_phases_from_dir` and the user-override walker no longer print
`WARN: failed to load phase ...` and continue. A malformed phase TOML now
prints `[boi] FATAL: ...` and exits non-zero. The choice: refusing to start
is a louder, more recoverable failure than silently substituting fallback
phases that may mismatch user expectations.

### 5. Phase-walker glob — `*.phase.toml` only

The `*.toml` glob was removed from `load_phases_from_dir`. It used to catch
`pipelines.toml` and try to load it as a phase file, failing validation, and
quietly skipping (the exact "swallowed error" pattern). Pipelines are loaded
exclusively by `load_pipeline_from_file`.

## Migration steps (existing users)

### If you have a custom `~/.boi/pipelines.toml`

If any mode uses `spec_phases = [...]` without explicit pre/post, you'll see a
loud `WARN` on next BOI start. Migrate:

```toml
# Before
[mode.mymode]
spec_phases = ["plan-critique", "critic"]

# After
[mode.mymode]
spec_pre_phases  = ["plan-critique"]
spec_post_phases = ["critic"]
```

Apply the rule: `plan-critique` and `spec-review` go to pre; everything else
goes to post.

### If you have custom phase TOMLs in `~/.boi/phases/`

Add the three required fields:

```toml
[phase]
level         = "task"    # or "spec"
can_add_tasks = false     # default for most phases
can_fail_spec = false     # default for most phases
```

If unsure, look up the old derive rules above. BOI will refuse to start with
a clear stderr message naming the missing field and file if any phase TOML is
incomplete after upgrade.

## Verification after upgrade

```bash
boi --version           # confirms binary loaded
boi status              # confirms daemon reads pipelines + phases ok
```

Then dispatch a no-op probe spec and confirm it reaches ✓ DONE within ~60s
(this was the live verification that landed the fix; if the probe loops,
something is still misconfigured).
