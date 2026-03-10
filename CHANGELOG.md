# Changelog

All notable changes to BOI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
