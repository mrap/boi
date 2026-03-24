# Changelog

All notable changes to BOI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - Unreleased

### Added
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
