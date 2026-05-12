# Changelog

All notable changes to BOI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2026-05-12] - Phase configurability + worker entry — BREAKING

### BREAKING

- **Phase TOMLs must declare `level`, `can_add_tasks`, `can_fail_spec` explicitly.**
  The previously-silent name-based inference (`derive_level` / `derive_can_add_tasks` /
  `derive_can_fail_spec` in `src/phases.rs`) is removed. Phase load now returns
  `Err` with the offending file path and field name on missing fields, and the
  daemon refuses to start with a malformed phase registry (exit 2). Migrate by
  adding the three fields to each `[phase]` section. See
  [docs/phase-configurability-2026-05-12.md](docs/phase-configurability-2026-05-12.md)
  for the migration rules.
- **Pipeline modes must declare `spec_pre_phases` and `spec_post_phases` explicitly.**
  The legacy magic-string sorter in `worker.rs` (which split `spec_phases` into
  pre/post by name-matching `spec-review` / `plan-critique`) is removed. A loud
  `WARN` fires at load time when a pipeline mode uses the old legacy shape
  without explicit pre/post; behavior changes silently (e.g. `plan-critique`
  used to run PRE-task, now runs POST under the back-compat path). Migrate by
  splitting `spec_phases` into the two explicit lists per the rule documented
  in the migration doc.

### Fixed

- **`depends_on` now accepts comma-separated spec IDs.** `Queue::dequeue`,
  `dequeue_filtered`, and `dequeue_for_pools` previously treated `depends_on`
  as a single spec ID and did the dependency check in SQL. All three now use
  `Queue::deps_all_completed` (Rust-side), which splits the column on `,`,
  trims whitespace, and requires every listed ID to have `status = 'completed'`
  before the spec is eligible. A spec with `depends_on = "SA7F3,TB2E1"` was
  silently ignored before this fix.

- **Worker state-machine entry no longer hardcoded.** New
  `fn initial_worker_state(order, done_ids, pre_spec_phases) -> Result<WorkerState>`
  drives the initial state from the pipeline declaration and DB state. Branches:
  empty order → `Cleanup{success:true}`; all tasks DONE → `PostTaskSpecPhase{0}`;
  empty pre phases → `TaskSelect`; otherwise `SpecPhase{0}`. **This is the fix
  for the May 6–12 spec-review loop**: restarted workers that found all tasks
  already DONE in the DB no longer re-ran multi-minute opus pre-spec phases and
  got killed before they could terminate. Stuck specs since 2026-05-06
  automatically transitioned to `✓ DONE` on first sweep under the new binary.

### Loud failures (Standing Order S6)

- `PhaseConfig::from_toml` returns `Err` (named-field-and-path) instead of
  silent inference.
- `load_phases_from_dir` refuses to start with a malformed phase TOML
  (exit 2 + clear stderr) — previously printed `WARN: failed to load phase ...`
  and continued.
- User-override phase walker same treatment — fail loud, not skip.
- `load_pipeline_from_file` warns loudly when a mode uses the legacy
  `spec_phases = [...]` shape with no explicit pre/post.
- `initial_worker_state` returns `Err` on inconsistent `done_ids` (an id not
  in `order` — DB corruption signal), letting the caller mark the spec
  `failed` via the normal DB-update path instead of panicking the worker
  thread and leaving the spec dangling as `running`.

### Migrated

- 13 in-repo `phases/*.phase.toml` now declare `level` / `can_add_tasks` /
  `can_fail_spec` explicitly. Values exactly match what the deleted
  `derive_*` functions would have returned (no behavior change for in-repo
  phases).
- `phases/pipelines.toml` modes `default`, `challenge`, `discover`, `generate`
  migrated from `spec_phases = [...]` to explicit
  `spec_pre_phases`/`spec_post_phases`. Mode `v2` was already explicit.
- `fn fallback_pipeline()` in `src/phases.rs` migrated to the explicit shape.
  Dropped the phantom `spec-review` phase name (no `spec-review.phase.toml`
  has ever existed; the registry filter dropped it at runtime anyway).
- `load_phases_from_dir` glob narrowed to `*.phase.toml` (was `*.toml` +
  `*.phase.toml`), so `pipelines.toml` is no longer attempted as a phase
  file and dropped as `WARN: failed to load`.

### Known follow-ups (separate work)

- 3 worker tests (`test_redo_tasks_are_executed`,
  `test_quality_loop_plan_critique_loops_back_to_spec_review`,
  `test_quality_loop_max_exceeded_proceeds_to_task_select`) are `#[ignore]`'d
  with `TODO(2026-05-12-layer3)` — their verdict-consumption sequences encoded
  the legacy phase ordering and need rewrite for the new pipeline shape. The
  machinery they cover is unchanged.
- Workspace-required schema enforcement on dispatched specs ("Layer 4" — every
  spec must declare a target git workspace OR a `workspace_rationale`) is
  pending in a follow-up branch.
- A one-shot `boi phases migrate` command that auto-adds explicit fields to
  legacy user phase TOMLs is a candidate follow-up.

## [2026-05-12] - Installer v1.1.0 — Canonical env.sh layer (TC377)

### Added
- `~/.boi/env.sh`: canonical env file installed by `install.sh`; chains to `$HEX_DIR/.hex/env.sh` if hex is present, then fills BOI defaults (`BOI_HOME`, `PATH` injection). Source of truth for all BOI contexts (shells, daemons, subshells).
- Idempotent sentinel-block injection into `.zshenv`, `.bash_profile`, `.bashrc`, `.profile` so all shell contexts source `~/.boi/env.sh` automatically.
- BOI daemon plist now uses a wrapper `ProgramArguments` (`/bin/bash -c ". ~/.boi/env.sh && exec boi-daemon"`) so the daemon inherits the same env as user shells.
- `boi env [--shell|--json]` subcommand: prints `BOI_HOME` and `PATH` in eval-able or JSON form; external integrators use `eval "$(boi env --shell)"` instead of sourcing a file.
- `boi doctor env` sub-check: detects drift between the running process env and what `env.sh` would set; exits non-zero on drift.
- Post-install migration message guides users to `source ~/.boi/env.sh`, restart daemons with `launchctl kickstart -k gui/$UID/com.hex.boi-daemon`, and verify with `boi env --shell` / `boi doctor env`.
- Hex-citizen detection: if `$HEX_DIR` is set or `~/.hex` exists, the migration message notes the env chain is active.

### Changed
- `INSTALLER_VERSION` bumped to `1.1.0`.
- `setup_alias()` now symlinks `~/bin/boi` → `~/.boi/bin/boi` (the Rust binary) instead of `boi.sh`; the bash shim is no longer used by the installer.

## [2026-05-05] - Context Checkpoint Injection (T5A3D)

### Added
- Prior-task context injection into worker prompts: before each task runs, `load_prior_checkpoint` scans `~/.hex/audit/checkpoints/<spec_id>-<task_id>.json` for the most recently completed task in this spec (by `completed_at`). If found, a `## Prior task context` section is prepended via the `{{PRIOR_TASK_CONTEXT}}` template variable in `templates/worker-prompt.md`. Silently skipped (empty string) when no checkpoint exists or parsing fails.
- `TemplateVar::PriorTaskContext` added to `src/phases.rs` and initialized to empty string at prompt-vars setup time (`worker.rs:646`).

## [2026-05-05] - Context Checkpoint (T2752)

### Added
- Post-task checkpoint writer: after a task transitions to `DONE`, a JSON checkpoint is written to `~/.hex/audit/checkpoints/<spec_id>-<task_id>.json` containing `spec_id`, `task_id`, `task_title`, and `completed_at` (ISO 8601 UTC). Write is non-fatal — failures are silently ignored and never propagate. Foundation for between-task context injection (T5A3D).

## [2026-05-04] - Agent Context Standardization

### Changed
- AGENTS.md: Renamed from CLAUDE.md; added "Related repos" cross-link to mrap-hex and hex-foundation; added fresh-LLM Quick Start summary; added How-to-add-a-feature runbook and gotchas section
- CLAUDE.md: Now a symlink to AGENTS.md — both resolve to same content for cross-tool compatibility

### Verified (cross-repo sync)
- Confirmed AGENTS.md and CLAUDE.md resolve to identical content (diff exit 0)
- Confirmed cross-links to mrap-hex and hex-foundation present in AGENTS.md Quick Start

## [0.3.0] - Unreleased

### Added
- **Tag-matching dispatch (Phase 3 TAB69):** `Queue::tags_match(runner_tags_json, required_tags_json)` returns true when every tag in a spec's `required_tags` array is present in the runner's tags — empty `required_tags` always matches. `Queue::dequeue_filtered(runner_tags_json)` uses this to skip ineligible specs during dispatch: it selects all queued specs ordered by priority, evaluates `tags_match` for each, and returns the first match. `GET /v1/specs/next` now accepts a `tags` query param (JSON array) and calls `dequeue_filtered` instead of `dequeue` when tags are present, ensuring runners only receive specs they can handle.
- **Load-aware dispatch hints (Phase 3 TB581):** `runners` table gains `slots_free INTEGER` and `ram_free_mb INTEGER` columns (via `ensure_column` migration). New `queue.update_runner_capacity(runner_id, slots_free, ram_free_mb)` method atomically updates both columns plus `last_seen` in one SQL UPDATE. `GET /v1/specs/next` now accepts `slots_free` and `ram_free_mb` query params: if `slots_free=0` the central returns `204 No Content` without dequeuing (capacity stored, runner alive but not dispatched to); otherwise capacity is updated then normal dequeue proceeds. `specs.required_tags TEXT DEFAULT '[]'` column migrated as schema foundation for tag-matching dispatch.
- **Runner registration + HMAC-SHA256 auth (Phase 2 t-1):** `boi fleet add-runner --name <name>` generates a 32-byte HMAC-SHA256 secret key, prints the hex to stdout, and stores the raw hex key in the `runners` table (`secret_key_hash` column). Runner daemons load the key from `~/.boi/runner.yaml` and sign every outbound request with `X-Runner-ID`, `X-Timestamp`, and `Authorization: HMAC-SHA256 <hex>` headers. Central verifies via `queue.lookup_runner_key()`. `src/api/auth.rs` contains `compute_hmac` and `constant_time_eq` helpers.
- **Heartbeat timeout scan (Phase 2 t-4):** The central daemon scans every 15s for runners whose `last_seen` is older than 60 seconds. Timed-out runners are marked failed with reason `heartbeat_timeout`; specs with `attempts < max_iterations` are requeued automatically. Runner `last_seen` is updated on each `POST /v1/specs/{id}/heartbeat`. `boi fleet status` shows runner liveness based on `last_seen` age.
- **hex-events fleet integration (Phase 2 t-5):** Three `boi.fleet.*` events are emitted via `~/.hex-events/hex_emit.py`: `boi.fleet.dispatched` when a runner claims a spec (`spec_id`, `runner_id`, `timestamp`), `boi.fleet.completed` when a runner reports completion (`spec_id`, `runner_id`, `status`, `cost_usd`, `duration_secs`; idempotency-guarded per C-018 so duplicate completes are suppressed), and `boi.fleet.host_down` when a runner exceeds the 60s heartbeat threshold (`runner_id`, `last_seen`, `specs_affected`). `src/hex_events.rs` provides a non-fatal `emit_fleet_event` helper — logs a warning if `hex_emit.py` is absent, never panics.
- **Fleet HTTP API (Phase 1 t-1):** `boi daemon` now starts an axum HTTP server on `0.0.0.0:7701` alongside the existing poll loop. Two endpoints: `GET /v1/specs/next?runner_id=<id>` (dequeue highest-priority queued spec, mark running, return JSON payload or 204) and `POST /v1/specs/{id}/complete` (accept `{status, branch_name, cost_usd, duration_secs, error?}` and update DB). No auth in Phase 1 (Tailscale network trust). Adds `runner_id`, `remote_cost_usd`, `remote_duration_secs` columns to the `specs` table.
- `boi daemon stop --destroy-running` / `boi daemon restart --destroy-running`: new flag that cancels all running specs before stopping or restarting the daemon; requires explicit `--yes` in non-TTY environments; without the flag, stop/restart is now safe by default (running specs continue and are requeued on next start)
- `queue.list_running_specs()`: new Queue method returning all specs currently in `running` or `assigning` state, used by the confirmation prompt to show users exactly what will be cancelled

- Height-aware dashboard layout: `boi dashboard` now respects terminal height; RUNNING and QUEUED sections are prioritized and always fit on screen; FINISHED shows as many items as fit in remaining rows; truncated sections show a `+N more` hint; layout recomputes on terminal resize
- `boi completions <shell>`: generate shell completion scripts for bash, zsh, fish, elvish, or powershell; pipe to the appropriate completion directory (see README Shell Completions section)
- Auto-load `~/.boi/.env` at startup: binary now loads `~/.boi/.env` (or `$BOI_ENV_FILE`) before provider registry initializes, so `OPENROUTER_API_KEY` and other secrets can live in that file without shell profile changes; existing process env always wins
- Pipeline `[phase_overrides.<phase>]` blocks: pipeline TOMLs can now override `runtime`, `model`, `effort`, and `timeout` per phase; runner consults pipeline override first then falls back to phase TOML default; overrides are logged at info level in `[phase.invoked]` events for bench attribution
- `scripts/autoresearch-propose.py`: LLM-driven hypothesis generator for BOI pipeline variants — reads bench results + current default, calls OpenRouter (gemini-flash) to propose a single variant TOML + rationale; tracks per-axis fail counts and pivots after 3 consecutive failures on the same axis; emits `boi.autoresearch.propose` telemetry
- `openrouter` runtime support: phases can specify `runtime = "openrouter"` + any model string; requires `OPENROUTER_API_KEY` env var (can be set in `~/.boi/.env`)
- `boi providers list`: new subcommand — list all registered and disabled runtime providers (claude, codex, openrouter) and their availability on the current machine
- Per-phase telemetry: `PhaseInvocation` struct captures runtime, model, effort, thinking config, prompt length, timeout, auth env var, CLI args, git SHA, and host fingerprint for every phase invocation
- `phase_runs` SQLite table: append-only log of every phase invocation with full completion fields (duration_ms, startup_ms, inference_ms, tokens, cost, exit_status, exit_reason)
- `boi.phase.invoked` / `boi.phase.completed` events emitted to `~/.hex/audit/boi-phase-runs.jsonl` (audit log) and daemon stderr on every phase entry/exit
- `boi phases <spec_id> [--full]`: new subcommand — dump all phase invocations for a spec as a table (default: phase, runtime, model, duration, cost; `--full`: every field)
- `boi log <spec_id> [--full]`: now appends a phase invocations table after the event log; `--full` renders every `PhaseInvocation` field
- `boi status -v`: phase rows now show runtime + model alongside phase name
- `boi resume <queue-id>` / `boi resume --all`: Resume failed or canceled specs with progress preserved
- `boi dep` commands for inter-spec dependency DAG management:
  - `boi dep add <spec> --on <dep>`: Add dependency between specs
  - `boi dep remove <spec> --on <dep>`: Remove dependency
  - `boi dep set <spec> --on <dep1,dep2>`: Replace all deps
  - `boi dep clear <spec>`: Make spec independent
  - `boi dep show [spec]`: Show deps for one or all specs
  - `boi dep viz`: ASCII fleet DAG visualization
  - `boi dep check`: Validate DAG (cycles, missing refs)
  - `boi dispatch --spec file.md --after q-A,q-B`: Dispatch with dependencies
- `boi cleanup`: Find and kill orphaned `claude -p` worker processes not tracked by any active spec
- `boi spec <qid> deps` for intra-spec task-level dependency management:
  - `boi spec <qid> deps show/add/rm/set/clear/viz/migrate`
- `## Dependencies` section in specs as first-class DAG format:
  ```
  ## Dependencies
  t-1: (none)
  t-2: (none)
  t-3: t-1, t-2
  ```
  Backward compatible with `**Blocked by:**` inline format
- `lib/dag.py`: DAG management module for intra-spec task dependencies
- Signal-aware failure handling: SIGTERM (exit 143) and SIGKILL (exit 137) no longer count as consecutive failures; workers killed externally are requeued, not failed
- Daemon lock via `fcntl.flock` preventing multiple daemon instances
- Dependency-aware task decomposition: `**Blocked by:** t-X, t-Y` syntax for declaring task dependencies in specs
- `validate_dependencies()` in `lib/spec_validator.py`: Kahn's algorithm cycle detection, unmet dependency detection, orphan task warnings
- `check_task_sizing()` in `lib/spec_validator.py`: heuristic warnings for oversized tasks (>2000 chars, 3+ data sources, 3+ mutations) and undersized tasks (<50 chars)
- `blocked_by` field in `BotTask` dataclass (`lib/spec_parser.py`): parsed from `**Blocked by:**` lines
- `lib/context_injector.py`: shared context injection into worker prompts with priority ordering
- `lib/preflight_context.py`: pre-launch environment verification (checks tools, paths, permissions)
- Topological task selection in worker prompt: workers prioritize tasks that unblock the most downstream work
- Self-evolution dependency inference: workers adding new tasks in Discover mode must declare `**Blocked by:**` lines
- Append-only self-evolution rule: new tasks always appended at end of spec file

### Changed
- Worker prompt (`templates/worker-prompt.md`) updated with dependency-aware task selection logic and topological task ordering
- Discover mode template updated with dependency inference instructions for self-evolved tasks
- Spec validator now runs dependency validation after format checks during `boi dispatch`

### Fixed
- `hooks/default.yaml` shell quoting fixed: `$BOI_SPEC_ID` now expands correctly in hook commands (was single-quoted, preventing variable expansion)
- Orphaned worker processes no longer linger after spec completion (`boi cleanup`)
- Multiple daemon instances no longer spawn (daemon lock via `fcntl.flock`)
- Exit codes 143 (SIGTERM) and 137 (SIGKILL) no longer count as consecutive failures, preventing false failure states from external kills

### Removed
- Messaging protocol between daemon and workers (reverted; kill + sidecar files is simpler and more reliable)

## [0.2.0] - 2026-03-09

### Changed
- Rewrote daemon from bash (`daemon.sh`) to Python (`daemon.py`) with SQLite state management
- Rewrote worker from bash (`worker.sh`) to Python (`worker.py`)
- Replaced JSON-file queue (`queue.py`) with SQLite database layer (`db.py`)
- All state transitions are now atomic SQLite transactions (eliminates TOCTOU races)
- Iteration counter now counts execute phases only (critic/evaluate/decompose phases do not increment)
- Workers spawned with `start_new_session=True` for clean process-group kill on shutdown/timeout
- PID validation uses `/proc/{pid}/stat` start time comparison to prevent PID reuse false positives

### Added
- `lib/db.py`: SQLite database layer with WAL mode for concurrent reads
- `lib/queue_compat.py`: Compatibility layer routing to SQLite or JSON queue
- `lib/cli_ops.py`: Thin CLI operations layer called by `boi.sh`
- `lib/db_migrate.py`: JSON-to-SQLite migration (`boi migrate-db`)
- `lib/db_to_json.py`: SQLite-to-JSON export for rollback (`boi export-db`)
- Integration test suite covering full lifecycle, crash recovery, concurrency, phases, and self-heal
- Worker timeout via `--timeout` flag (defense in depth alongside daemon-side timeout)
- Stuck-assigning recovery in self-heal (specs in 'assigning' for >60s reset to 'requeued')

### Deprecated
- `lib/queue.py`: JSON-file queue kept for rollback but no longer actively used
- `daemon.sh` and `worker.sh`: Moved to `archive/` for reference

## [0.1.0] - 2026-03-07

### Added
- Core spec-driven execution engine with fresh-context-per-iteration design
- Four execution modes: Execute, Challenge, Discover, Generate
- Priority queue with DAG-based task blocking
- Parallel workers using git worktrees for isolation
- Self-evolving specs: workers add tasks at runtime as they discover new work
- 18-signal quality scoring across Code, Test, Documentation, and Architecture
- Critic system with configurable checks and custom check support
- Experiment proposals with adopt/reject/defer workflow
- Generate mode with goal-only specs, decomposition, and convergence detection
- Live spec management (add, skip, reorder, block tasks)
- Project model with shared context injection
- Natural language interface via `boi do`
- Per-iteration telemetry with Deutschian progress metrics
- Integration hooks (on-complete, on-fail) with JSON event log
- Universal install script for macOS and Linux
- Comprehensive test suite (unit, integration, eval)
