# Debugging a Failing Spec

How to diagnose and fix spec/task failures.

## Diagnostic commands

The read-side surfaces (`dashboard`, `log`, `spec show`, `traces`, `failures`) read SQLite / DuckDB directly — they work even when no daemon is running.

| Step | Command | What it shows |
|------|---------|---------------|
| 1 | `boi dashboard [spec-id]` | Live TUI — task structure from SQLite plus per-phase events tailed from the OTel traces. Omit the id for a recent-specs picker |
| 2 | `boi log <spec-id>` | Phase-run history in execution order; in-flight rows are marked `[running]`. An unknown spec id is a loud error, not empty output |
| 3 | `boi spec show <spec-id> [--version N]` | The stored `spec_versions` snapshot (JSON) — what the workers were actually told |
| 4 | `boi daemon status` | Service state: `Running (pid N)`, `Stopped`, `NotInstalled`, or `Failed: …` |
| 5 | `~/.boi/v2/logs/daemon.log` | Daemon stdout/stderr (the installed service's log path) |
| 6 | `boi traces query '<SQL>'`, `boi failures top [--last 7d] [--n 10]` | DuckDB SQL over the OTel JSONL under `~/.boi/v2/traces/`. Requires the `duckdb` build feature (on by default); a `--no-default-features` binary still parses these commands but exits non-zero with a loud message |
| 7 | `sqlite3 ~/.boi/v2/boi.db` | Raw state — see the queries below |

Spec `status` values: `queued`, `running`, `completed`, `failed`, `canceled`. Task `state` values: `not_started`, `active`, `blocked`, `passing`, `canceled`.

A blocked task's `task_runtime.blocked_reason` is tagged JSON (`{"type": …}`):

- `awaiting_deps` — reserved; no production code sets it today (definition + test-only construction). Seeing it live means a bug worth filing
- `cap_exceeded` — a bounded iteration loop hit its cap (carries the loop name, the cap, and the last error/why/fix)
- `merge_conflict` — a merge into the integration branch hit conflicts (carries the conflicting files)
- `workspace_unclean` — a clean-state precondition or postcondition failed
- `provider_failed` — the LLM provider failed (auth, rate limit, outage, …)
- `plan_revision_pending` — a task report needs a plan revision before this task continues
- `manual` — an operator blocked it

Read the reason before reaching for a fix.

### Useful SQLite queries

```sql
-- Why is this spec not finishing?
SELECT status, failure_reason, cancellation_reason FROM spec_runtime WHERE spec_id = 'S…';
-- Which tasks are blocked, and why?
SELECT task_id, state, blocked_reason FROM task_runtime WHERE spec_id = 'S…' AND state = 'blocked';
-- Which phase runs are still open (in-flight, or orphaned by a crash)?
SELECT id, task_id, phase, phase_iteration, started_at FROM phase_runs WHERE spec_id = 'S…' AND completed_at IS NULL;
```

## Common root causes

| Symptom | Cause | Fix |
|---------|-------|-----|
| Spec stuck — a task sits `blocked` | `blocked_reason` says why (see above) | `cap_exceeded`: fix the underlying error, then `boi unblock <task-id> --reset-counter` — forces the task back to `active`; the flag also zeroes its iteration counter. `awaiting_deps`: reserved (no producer) — treat as a bug, not an operator state |
| Task blocked with `merge_conflict` | The task branch hit conflicts merging into `spec/<SpecId>/integration` | `boi resolve-conflict <task-id>` — the daemon re-creates the conflict and opens an interactive shell; the command blocks until that shell exits. There is intentionally no `--ai` flag (LLM-driven resolution is deferred to v1.x) |
| `no boi daemon is running (control socket … unreachable)` | Write-side commands (`dispatch`, `cancel`, `unblock`, `resolve-conflict`, `fail`) require a live daemon — they fail loud rather than flip the DB while an orphan worker may still run | `boi daemon start` (installs the per-user service and starts it); `boi daemon restart` to pick up a new binary |
| `boi dispatch` exits non-zero: `the spec was persisted but NO daemon is running to start it` | Spec rows are persisted before the control-socket call, so the spec sits `queued` and never starts | Start the daemon and re-dispatch. Boot recovery never picks up `queued` specs; remove the orphaned row with `boi clean <spec-id> --force` |
| A spec is still `running` after a daemon crash | The daemon that owned it died mid-run | Restart the daemon — the boot-time recovery pass marks each such spec `failed` (`daemon_crash`) and stamps `completed_at` on orphaned `phase_runs` rows |
| A phase fails repeatedly | The same error recurs each iteration; the bounded loop will eventually block the task with `cap_exceeded` | `boi failures top --last 7d` for the recurring fingerprint; fix the cause, then unblock. To stop instead: `boi fail <spec-id> --reason "…"` or `boi cancel <id> --reason "…"` (the daemon resolves whether `id` names a spec or a task) |
| Worktree leftovers under `~/.boi/v2/worktrees/<SpecId>/` | The `teardown` step is best-effort and only runs when the pipeline reaches it. `boi clean` deletes DB rows only — never worktrees or branches | In the workspace repo: `git worktree remove --force <path>` per leftover, then `git branch -D` the `spec/<SpecId>/<TaskId>` and `spec/<SpecId>/integration` branches |

`boi clean <spec-id>` refuses a non-terminal spec unless `--force`; `--phase-runs-older-than <dur>` instead prunes only that spec's old completed `phase_runs` rows.

See also: the root [AGENTS.md](../../AGENTS.md) for the canonical CLI table and spec format (runnable spec examples in `tests/fixtures/specs/`), [invariants.md](invariants.md) for what not to break, [glossary.md](glossary.md) for states and reasons.
