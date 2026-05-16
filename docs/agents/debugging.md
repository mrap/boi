# Debugging a Failing Spec

How to diagnose and fix spec/task failures.

## Diagnostic commands

| Step | Command | What it shows |
|------|---------|---------------|
| 1 | `boi status <spec-id> -v` | Current phase, task states, runtime info |
| 2 | `boi log <spec-id> --debug` | Claude output, verify results |
| 3 | `boi log <spec-id> -f` | Live tail of daemon log filtered to this spec |
| 4 | `~/.boi/telemetry/boi.jsonl` | Structured lifecycle events with cost/token data |
| 5 | `~/.boi/logs/<spec-id>/` | Raw per-spec log files |
| 6 | `boi doctor` | Daemon liveness, DB integrity, worktree state, config validity |
| 7 | `boi status --all` | All specs — spot stuck `assigning` states |

## Common root causes

| Symptom | Cause | Fix |
|---------|-------|-----|
| Spec stuck as `running`/`assigning` | Daemon crashed mid-dispatch | `boi daemon restart` — auto-recovers via `recover_stuck_specs()` (`src/queue.rs:1530`) |
| Daemon refuses to start (exit 2) | Phase TOML missing required fields | Add `level`, `can_add_tasks`, `can_fail_spec` to the offending `.phase.toml` (`src/phases.rs:198`) |
| Verify passes once then fails on retry | Verify command not idempotent (e.g., `CREATE TABLE`) | Rewrite verify to be idempotent — see [invariants.md](invariants.md) #5 |
| Worktree creation fails | Branch `boi/<spec-id>` already exists from a prior run | `boi prune-orphans --apply` or `git worktree remove` + `git branch -D` |
| Claude subprocess timeout | Task too large for timeout setting | Increase `--timeout` on dispatch |
| Hook subprocess hung | Missing `timeout` in hook config | Add `timeout: 10` to hook entry in `~/.boi/hooks.yaml` |
| Two daemons against same DB | `busy_timeout` causes 5s stall then error | Kill one daemon; check `~/.boi/daemon.pid` |

## Known issues

**BUG M-5** (`src/worker.rs:2789`): `test_redo_tasks_are_executed` — task stays `in_progress` after Redo, blocks re-selection. Test is `#[ignore]`'d by default; run with `--include-ignored` to surface.

See also: [cli-reference.md](cli-reference.md) for full command flags, [invariants.md](invariants.md) for what not to break.
