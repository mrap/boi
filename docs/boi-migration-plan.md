# BOI Migration Plan: Python → Rust

> Produced by q-917 iteration 4.
> Informed by: docs/boi-rust-architecture.md (t-2), docs/boi-hooks-spec.md (t-3).
> Date: 2026-04-27

---

## Overview

This plan describes a four-phase migration from the current Python/Bash BOI system (`~/.boi/boi.sh` + `daemon.py` + `worker.py` + 20+ lib modules) to a single self-contained Rust binary. The migration is designed for zero-downtime cutover: Python and Rust coexist on the same SQLite database and spec file formats throughout, so users can switch back at any time without data loss.

**Migration strategy:** Build → Validate → Cutover → Evolve. Each phase produces a checkpoint that can be rolled back independently.

---

## Phase 1: Rust Binary with CLI Parity

**Goal:** Replace the Python CLI for all user-facing commands. Python daemon continues running unchanged.

**Duration estimate:** 3–4 weeks of implementation.

### Scope

Implement all subcommands with identical behavior to the Python CLI:

| Command | Inputs | Expected output |
|---------|--------|----------------|
| `boi dispatch --spec FILE` | Spec file path | Spec queued; spec ID printed |
| `boi status` | — | Queue table (same columns) |
| `boi status --watch` | — | Auto-refreshing queue table |
| `boi status --json` | — | Machine-readable JSON |
| `boi queue [--json]` | — | Spec list |
| `boi log <id> [--full]` | Queue ID | Log output |
| `boi cancel <id>` | Queue ID | Spec cancelled |
| `boi outputs <id>` | Queue ID | Output file list |
| `boi telemetry <id> [--json]` | Queue ID | Iteration breakdown |
| `boi workers [--json]` | — | Worktree health |
| `boi purge [--dry-run]` | — | Completed spec cleanup |
| `boi stop` | — | Daemon + workers stopped |
| `boi install [--workers N]` | — | `~/.boi/` setup |
| `boi doctor` | — | Environment check |
| `boi spec <id> [add|skip|...]` | Queue ID | Spec task management |
| `boi project <create|list|...>` | Project name | Project management |
| `boi config [get|set]` | Key/value | Config read/write |
| `boi critic [status|run|...]` | — | Critic management |
| `boi review <id>` | Queue ID | Experiment review |

### SQLite Schema Compatibility

The Rust binary uses the **same `~/.boi/boi.db` SQLite schema** as Python. No schema changes in Phase 1. The Rust `db.rs` module reads and writes the existing tables with full WAL-mode compatibility.

### Validation Criteria

Before advancing to Phase 2:

- [ ] `boi dispatch` enqueues a spec and Python daemon picks it up
- [ ] `boi status` output format matches Python output (visual diff test)
- [ ] `boi cancel <id>` transitions spec to `canceled` in SQLite
- [ ] `boi log <id>` renders the last iteration's log file
- [ ] All 16 subcommands exit 0 on valid input, non-zero on invalid input
- [ ] E2E test suite passes (see Testing section)

### Backward Compatibility Wrapper

The existing `~/.boi/boi` is a bash wrapper that invokes the Python CLI. After Phase 1, this wrapper is replaced with a thin redirect to the Rust binary:

```bash
#!/usr/bin/env bash
# ~/.boi/boi — Phase 1+ wrapper
exec ~/.boi/bin/boi-rs "$@"
```

If `~/.boi/bin/boi-rs` is absent (e.g., on a machine that hasn't been updated), the wrapper falls back to the Python CLI:

```bash
#!/usr/bin/env bash
RUST_BIN="$HOME/.boi/bin/boi-rs"
if [[ -x "$RUST_BIN" ]]; then
    exec "$RUST_BIN" "$@"
else
    exec python3 "$HOME/.boi/lib/main.py" "$@"
fi
```

This means users with the Rust binary on PATH get it automatically; machines without it continue using Python with no change required.

---

## Phase 2: Worker Management

**Goal:** Replace the Python daemon (`daemon.py`) and worker (`worker.py`) with Rust equivalents. Hooks are still wired to `hex_emit.py` calls baked into the Rust binary at this phase; full hook externalization comes in Phase 3.

**Duration estimate:** 3–4 weeks.

### Scope

Implement `boi daemon` and `boi worker` subcommands:

**`boi daemon [--foreground]`**
- Polls every 5s (configurable via `poll_interval` in `~/.boi/config.yaml`)
- Dequeues specs with `status IN ('queued', 'requeued')` respecting `cooldown_until`
- Spawns workers via `tmux new-session -d -s boi-<spec_id> "boi worker <spec_id> ..."`
- Monitors worker completion by polling tmux `has-session`
- Fires `on_complete` / `on_fail` on spec terminal transitions
- Writes recovery record on startup to reset `status=running` → `requeued`

**`boi worker <spec_id> --worktree W --iter N [--timeout T]`**
- Parses spec (YAML or Markdown auto-detect)
- Selects next PENDING task respecting `depends:` DAG
- Assembles and writes prompt to `~/.boi/worktrees/<W>/boi-prompt.md`
- Spawns agent runtime (default: `claude -p`)
- Waits for agent exit with configurable timeout
- Diffs spec task counts (pre/post) to detect DONE transitions
- Writes per-iteration JSON telemetry
- Collects modified files into `~/.boi/outputs/<spec_id>/`
- Runs spec outcome verify commands; resets last DONE on failure

### Worker Timeout Handling

```
timeout_seconds (default: 600)
  ↓
Worker calls: tokio::time::timeout(Duration::from_secs(T), agent_future)
  → Err(_) → kill tmux session for agent → write exit code 124
  → daemon sees exit 124 → consecutive_failures++ → cooldown 60s → requeue
```

### Stall Detection

The daemon monitor checks `last_iteration_at` against `now` on every poll cycle. If `(now - last_iteration_at) > stall_threshold_minutes * 60`, and spec is still `status=running`, fire `on_stall` hook and log a warning. Does not automatically cancel; escalation is the hook's responsibility.

### Validation Criteria

Before advancing to Phase 3:

- [ ] `boi daemon` starts, assigns a spec, worker completes, spec transitions to `completed`
- [ ] Worker timeout fires after configured seconds and requeues spec
- [ ] `consecutive_failures >= 5` transitions spec to `failed`
- [ ] Python daemon can be stopped; Rust daemon picks up existing queued specs
- [ ] Stall detection fires `on_stall` after stall threshold
- [ ] Daemon crash recovery: restart with `status=running` entries resets them to `requeued`
- [ ] E2E test suite passes for daemon + worker paths

### Cutover from Python Daemon

1. `boi stop` — kills Python daemon and all Python worker sessions
2. Start Rust daemon: `boi daemon --foreground &` (or via launchd/systemd service)
3. Any queued specs with `status='queued'` or `'requeued'` are picked up automatically
4. `boi status` confirms Rust daemon is managing the queue

---

## Phase 3: Hook Interface

**Goal:** Replace all hardcoded `hex_emit.py` calls in the Rust source with configurable hooks. After this phase, BOI source contains zero references to hex, hex-events, or any external system.

**Duration estimate:** 1 week (hooks are architected in Phase 1/2; this phase activates them).

### Scope

Three places in the Python source hardcode hex-events calls. The Rust port must convert all three to hook firings:

| Python location | Hardcoded call | Rust replacement |
|----------------|---------------|-----------------|
| `lib/cli_ops.py: _emit_dispatched_event()` | `hex_emit.py boi.spec.dispatched` | `hooks::fire(HookPoint::OnDispatch, &payload)` |
| `lib/daemon_ops.py: _on_spec_complete()` | `hex_emit.py boi.spec.completed` | `hooks::fire(HookPoint::OnComplete, &payload)` |
| `lib/daemon_ops.py: _on_spec_fail()` | `hex_emit.py boi.spec.failed` | `hooks::fire(HookPoint::OnFail, &payload)` |

In addition, two legacy shell hooks exist at `~/.boi/hooks/on-complete.sh` and `on-fail.sh` with the old positional-args interface. These are superseded by the new YAML hooks.

### Migration for Existing Users

When `boi install` or `boi migrate` runs on an existing system:

1. **Detect legacy hex-events**: check if `~/.hex-events/hex_emit.py` exists.
2. **Auto-generate `~/.boi/config.yaml` hooks section** (if not already present):

```yaml
# Auto-generated by boi migrate — replace hardcoded hex-events calls
hooks:
  on_dispatch:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.dispatched"
    blocking: false
    timeout: 10
  on_complete:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.completed"
    blocking: false
    timeout: 10
  on_fail:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.failed"
    blocking: false
    timeout: 10
```

3. **Detect legacy shell hooks**: check if `~/.boi/hooks/on-complete.sh` or `on-fail.sh` exist. Emit a migration warning:

```
WARNING: Legacy hook scripts detected at ~/.boi/hooks/on-*.sh
These used positional args (queue_id spec_path). Migrate to config.yaml hooks:

  hooks:
    on_complete:
      command: "bash ~/.boi/hooks/on-complete.sh"
      blocking: false

Your new hook will receive JSON on stdin and BOI_SPEC_ID / BOI_SPEC_PATH as env vars.
See docs/boi-hooks-spec.md for the full payload schema.
```

### Validation Criteria

Before advancing to Phase 4:

- [ ] `grep -r "hex_emit" boi/src/` returns zero results
- [ ] `grep -r "hex_events" boi/src/` returns zero results  
- [ ] Dispatch a spec; verify `on_dispatch` hook fires (check hex-events log)
- [ ] Complete a spec; verify `on_complete` hook fires
- [ ] Fail a spec (force `consecutive_failures >= 5`); verify `on_fail` hook fires
- [ ] Remove hooks from config.yaml; all lifecycle points pass through silently (no-op)
- [ ] Hook failure (misconfigured command) logs warning but does not stall spec

---

## Phase 4: Identifier Redesign

**Goal:** Replace `q-NNN` / `t-N` identifiers with `SNNNNNN` / `TNNNNNN`. Carried out by `boi migrate` after full Rust cutover.

**Duration estimate:** 1–2 days (single `boi migrate` run + validation).

### ID Format

| Old | New | Example |
|-----|-----|---------|
| `q-001` | `S0000001` | Spec identifier |
| `q-917` | `S0000917` | This spec |
| `t-1` | `T0000001` | Task identifier |
| `t-4` | `T0000004` | This task |

### `boi migrate` Command

Atomic, reversible, requires no running specs.

```
boi migrate [--dry-run] [--yes]
```

**Pre-conditions checked:**
- `boi` binary is Rust (not Python wrapper) — checked via `boi --version` metadata
- No specs with `status IN ('running', 'queued', 'requeued')` — must quiesce queue first
- `~/.boi/boi.db` exists and is readable

**Migration steps:**

```
1. Lock queue     → fcntl lock on ~/.boi/queue/.lock
2. Backup         → cp ~/.boi/boi.db ~/.boi/boi.db.pre-migrate
                  → cp -r ~/.boi/queue/ ~/.boi/queue-backup-<timestamp>/
3. Assign new IDs → for each spec: S + zero-pad(numeric(q-NNN), 7)
4. Update SQLite  → UPDATE specs SET id='SNNNNNN' WHERE id='q-NNN'
5. Rename files   → mv q-NNN.spec.md SNNNNNN.spec.md (in ~/.boi/queue/)
6. Update task IDs→ regex: s/### t-(\d+):/### T000000\1:/ within each spec file
7. Update depends → regex: s/depends: \[t-(\d+)\]/depends: [T000000\1]/ 
8. Write event    → ~/.boi/events/event-NNNNN.json { type: "migration_complete" }
9. Release lock
```

**Dry-run output (example):**
```
[DRY RUN] Would rename:
  q-001.spec.md  →  S0000001.spec.md  (5 tasks: t-1→T0000001, t-2→T0000002 ...)
  q-917.spec.md  →  S0000917.spec.md  (4 tasks: t-1→T0000001, t-4→T0000004)
  [14 more specs]

  SQLite rows: 16 specs, 87 tasks

Run without --dry-run to apply.
```

### Parser Backward Compatibility

The Rust parser accepts both old and new ID formats indefinitely:

```rust
fn is_valid_task_id(s: &str) -> bool {
    // Old: t-N (t-1, t-10, t-123)
    // New: TNNNNNN (T0000001, T0000042)
    let old = Regex::new(r"^t-\d+$").unwrap();
    let new = Regex::new(r"^T\d{7}$").unwrap();
    old.is_match(s) || new.is_match(s)
}
```

Spec files using old-style IDs continue to work forever. `boi migrate` is opt-in.

### Validation Criteria

- [ ] `boi migrate --dry-run` lists all renames without modifying any file
- [ ] `boi migrate` completes; `boi status` shows all specs with `S` prefix IDs
- [ ] All spec files have been renamed; all task IDs within specs updated
- [ ] `boi dispatch --spec some-new-spec.md` creates `S` prefix ID for new spec
- [ ] Rollback: restore `boi.db.pre-migrate` and `queue-backup-*`; old Python or Rust CLI reads queue correctly

---

## Data Migration: Coexistence Strategy

The migration is designed so Python and Rust can operate on the same data simultaneously:

| Data store | Format | Compatibility |
|-----------|--------|---------------|
| `~/.boi/boi.db` | SQLite WAL | Read/write from both Python and Rust (WAL allows concurrent readers) |
| `~/.boi/queue/*.spec.md` | YAML and Markdown | Both parsers accept both formats |
| `~/.boi/logs/` | Plain text per iteration | No format change |
| `~/.boi/events/*.json` | Append-only JSONL | Rust appends same format; Python reads cleanly |
| `~/.boi/outputs/` | Directory of files | No format change |
| `~/.boi/config.yaml` | YAML | Python ignores `hooks:` section it doesn't know about |

**No schema migrations are required in Phase 1 or Phase 2.** The first schema migration (if any) is deferred to after Phase 4 has been in production for at least 2 weeks.

---

## Backward Compatibility

### CLI Interface

All existing commands, flags, and output formats are preserved exactly. Scripts and tools that call `boi` will continue to work without changes.

The wrapper at `~/.boi/boi` (Phase 1) ensures:
- On updated machines: calls Rust binary
- On non-updated machines or rollback: calls Python
- `boi --version` prints the binary type: `boi 1.0.0 (rust)` vs `boi 0.9.3 (python)`

### Spec File Format

Both YAML and Markdown spec formats are supported by the Rust parser. Existing spec files are read without modification. The Rust binary never silently rewrites spec files except when a worker marks a task DONE (same as Python behavior).

### Queue Database

The SQLite schema is unchanged through Phase 3. The Rust binary adds versioned migrations (tracked by `PRAGMA user_version`) but only runs them if the version number has advanced. An unmodified `boi.db` from Python has `user_version = 0`; the Rust binary does not alter it unless explicitly migrated.

### Config Files

`~/.boi/config.yaml` (Rust) is a superset of the Python config. Python ignores unknown YAML keys (`hooks:`, `stall_threshold_minutes`, etc.) via its `**kwargs` loading. Rust ignores Python-only keys via serde's `#[serde(deny_unknown_fields)]` being intentionally absent.

---

## Testing: Containerized E2E Tests

### Test Harness

Each CLI command has a corresponding E2E test in `tests/e2e/`. Tests run in a temporary `~/.boi/` directory (via `BOI_HOME` env override) and do not touch the user's real queue.

```
tests/
  e2e/
    test_dispatch.sh        — boi dispatch creates SQLite record + fires on_dispatch
    test_status.sh          — boi status renders correct columns
    test_status_json.sh     — boi status --json is valid JSON with expected fields
    test_log.sh             — boi log <id> reads correct log file
    test_cancel.sh          — boi cancel transitions spec to canceled
    test_outputs.sh         — boi outputs <id> lists collected files
    test_telemetry.sh       — boi telemetry <id> renders iteration breakdown
    test_worker_lifecycle.sh— full spec dispatch → daemon → worker → complete cycle
    test_timeout.sh         — worker timeout fires on_fail, increments failures
    test_hooks.sh           — hook payloads are correct JSON on stdin
    test_migrate.sh         — boi migrate renames files, updates SQLite
    test_rollback.sh        — restore boi.db.pre-migrate; CLI reads correctly
    harness.sh              — shared setup/teardown (creates temp BOI_HOME, fake claude stub)
```

### Container Environment

```dockerfile
FROM rust:1.78-slim
RUN apt-get install -y tmux git sqlite3 python3
COPY . /boi
WORKDIR /boi
RUN cargo build --release
ENV BOI_HOME=/tmp/boi-test
ENV BOI_NO_TMUX=1    # workers spawn directly, no tmux, for CI
RUN cargo test && bash tests/e2e/run_all.sh
```

`BOI_NO_TMUX=1` makes the daemon spawn workers via `tokio::process::Command::spawn()` instead of `tmux`, enabling headless CI runs.

### Fake Agent Stub

E2E tests use a `fake-claude` stub that immediately marks the next PENDING task DONE in the spec file:

```bash
#!/usr/bin/env bash
# tests/stubs/fake-claude
# Reads BOI_SPEC_PATH from env; marks first PENDING task DONE; exits 0
python3 - <<'EOF'
import os, re, sys

spec_path = os.environ.get("BOI_SPEC_PATH") or sys.argv[-1]
content = open(spec_path).read()
# Mark first PENDING → DONE
updated = content.replace("status: PENDING", "status: DONE", 1)
open(spec_path, "w").write(updated)
EOF
```

Configure via:
```yaml
# BOI_HOME/config.yaml (test)
runtime:
  default: fake-claude
```

### Coverage Targets

| Area | Test type | Required coverage |
|------|-----------|------------------|
| CLI subcommands (16) | E2E | 100% — every command has at least one happy-path test |
| Worker lifecycle | E2E | dispatch → assign → worker → complete cycle |
| Hook firing | Unit + E2E | All 9 hook points fire with correct JSON payload |
| Failure paths | E2E | timeout, consecutive_failures, max_iterations |
| Migration | E2E | dry-run, migrate, rollback |
| Parser | Unit | YAML + Markdown, both ID formats, `depends:` DAG |
| SQLite | Unit | enqueue, dequeue, status transition, WAL coexistence |

---

## Rollback Plan

### Phase 1 Rollback (CLI only)

Revert `~/.boi/boi` wrapper to call Python:

```bash
echo '#!/usr/bin/env bash\nexec python3 "$HOME/.boi/lib/main.py" "$@"' > ~/.boi/boi
chmod +x ~/.boi/boi
```

No database or spec file changes in Phase 1; rollback is instantaneous.

### Phase 2 Rollback (Daemon + Worker)

1. `boi stop` — stops Rust daemon
2. Revert `~/.boi/boi` to Python wrapper (above)
3. Restart Python daemon: `~/.boi/lib/daemon.py &`
4. Any `status=running` specs are reset to `requeued` by Python daemon recovery on startup

No schema changes in Phase 2; Python reads the same SQLite schema.

### Phase 3 Rollback (Hooks)

Phase 3 adds `hooks:` to `config.yaml`. Python ignores unknown YAML keys. Rolling back to Python daemon leaves the hooks section in place but inoperative (Python continues to call hex-events directly from source). No destructive changes.

### Phase 4 Rollback (ID Migration)

`boi migrate` writes backups before making any change:

```
~/.boi/boi.db.pre-migrate           ← full SQLite backup
~/.boi/queue-backup-<timestamp>/    ← full copy of queue directory
```

To roll back:

```bash
cp ~/.boi/boi.db.pre-migrate ~/.boi/boi.db
cp -r ~/.boi/queue-backup-<timestamp>/* ~/.boi/queue/
```

Both the Python CLI and Rust CLI (with old-style ID support) will function correctly after restore. The `boi migrate` command is intentionally irreversible via normal operation but trivially reversible via backup restore.

### General Rollback Decision Tree

```
Problem observed
  ↓
Is it a CLI rendering issue?
  → Phase 1 rollback → Python wrapper
Is it a daemon/worker lifecycle issue?
  → Phase 2 rollback → Python daemon
Is it a hook mis-fire or missing event?
  → Edit ~/.boi/config.yaml hooks section (no rollback needed)
Is it an ID confusion or missing spec after migrate?
  → Phase 4 rollback → restore boi.db.pre-migrate + queue-backup
```

---

## Milestones and Go/No-Go Gates

| Milestone | Gate criteria |
|-----------|--------------|
| Phase 1 complete | All 16 CLI commands pass E2E tests; `boi status` output matches Python visually |
| Phase 2 complete | Worker lifecycle E2E passes; Python daemon shut down; Rust daemon runs queue for 1 week without incident |
| Phase 3 complete | Zero hex/hex-events references in `boi/src/`; hook E2E tests pass |
| Phase 4 complete | `boi migrate` dry-run + live run succeed; all existing specs queryable with new IDs |
| Migration done | Python `~/.boi/lib/` archived to `~/.boi/lib.python-archive/`; Python not on PATH for BOI |

---

## Decision Rationale

**Decision:** Four-phase sequential migration rather than big-bang rewrite

| Option | Description | Score (1-5) |
|--------|-------------|:-----------:|
| **Four-phase sequential** | CLI → Daemon → Hooks → IDs; each phase independently rollback-able | 4.8 |
| Big-bang rewrite | Full Rust port in one shot; Python deleted on cutover day | 2.5 |
| Parallel operation (permanent) | Python and Rust run side-by-side indefinitely | 2.0 |

**Margin:** 4.8 vs 2.5 — clear winner

**Key trade-off:** Sequential migration extends the transition period (8–12 weeks vs 3–4 weeks big-bang) but eliminates the risk of losing an entire production queue if the Rust port has a latent bug.

**Assumptions that could change the verdict:**
- If the Python codebase has zero active production specs, big-bang becomes more viable.
- If the Rust port has a comprehensive test suite before Phase 1 ships, the risk of big-bang drops significantly.

**Dissenting view:** The four-phase approach requires maintaining two code paths simultaneously during the transition window. If a bug is found in Phase 2 (daemon), the fix must be verified against both the Python behavior and the Rust behavior, doubling debugging effort. A big-bang approach with a thorough pre-cutover test run avoids this dual-maintenance cost.
