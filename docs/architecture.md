# Architecture

BOI is a single Rust binary built around one idea: **every unit of LLM work runs in a
fresh, isolated process with no accumulated context**, and **nothing merges until a
verify gate proves it**. A background daemon owns all state and orchestration; the
`boi` binary you type at the prompt is a short-lived client that talks to it.

## System overview

```
                         ~/.boi/v2/daemon.sock (Unix control socket)
  boi dispatch  ───────────────┐
  boi cancel    ───────────────┤
  boi unblock   ───────────────┼──────►  boi daemon serve  (one long-running process)
  boi fail      ───────────────┤            │
  boi resolve-conflict ────────┘            │  EventBus ──► Orchestrator ──► Scheduler
                                             │     │              │
  boi dashboard  ─────┐                     │     ▼              ▼
  boi log        ─────┼──► reads directly   │  SQLite      per-task PhaseExecutor
  boi spec show  ─────┤    (no daemon        │  boi.db          │
  boi traces     ─────┘     needed)          │                  ├──► GooseRuntime
                                              │                  │      (LLM phases:
                                              │                  │       goose run
                                              │                  │       --recipe X.yaml)
                                              │                  │
                                              │                  └──► DeterministicExecutor
                                              │                         (native Rust:
                                              │                          git worktrees,
                                              │                          verify, merge)
                                              ▼
                                    ~/.boi/v2/worktrees/<spec-id>/
                                       integration/  <task-id>/  ...
```

Two kinds of `boi` invocation:

- **Write commands** (`dispatch`, `cancel`, `unblock`, `resolve-conflict`, `fail`) are
  control-socket clients. They connect to `~/.boi/v2/daemon.sock`, send a typed
  command, and **fail loud with a non-zero exit if no daemon is listening** — there is
  no silent fallback to a DB-only write.
- **Read commands** (`dashboard`, `log`, `spec show`, `traces`, `failures`) read
  `~/.boi/v2/boi.db` (and the OTel trace files) directly. No daemon required.

## A spec's path through the system

1. **`boi dispatch spec.toml`** parses and validates the TOML (`config::parse_spec`),
   mints a spec ID and a task ID per task, and persists everything —
   `specs` / `spec_versions` / `spec_runtime` / `task_runtime` / `task_deps` — in one
   SQLite transaction. This happens whether or not the daemon is up.
2. It then sends `DaemonCommand::Dispatch` over the control socket. If nothing is
   listening, the spec sits `queued` in the database and the command exits non-zero —
   dispatch must be re-run once the daemon is up.
3. The daemon marks the spec `running` and runs **preflight**: `goose` version check,
   provider-credential check, a live provider probe, and (for GitFlow workspaces) a
   branch-policy check against the spec's `base_branch`. A preflight failure is a legal
   `running → failed` transition — zero phases run, zero tokens spent.
4. The **orchestrator** drives the spec through the `standard` pipeline
   (`~/.boi/v2/pipelines/standard.toml`): spec-level phases run first
   (`workspace_prepare`, `plan`, `critique_plan`), then the pipeline hits the explicit
   `<tasks>` fan-out boundary — every task whose `blocked_by` deps are already
   satisfied starts in parallel, each in its own phase pipeline
   (`workspace_verify_in`, `write_red_tests`, `execute`, `validate`, `review`,
   `commit`, `workspace_verify_out`). Once every task settles, the remaining
   spec-level phases run (`validate`, `review`, `merge`, `teardown`).
5. **Each phase is a fresh process, not a fresh turn in a conversation.** LLM phases
   route through `GooseRuntime`, which spawns `goose run --recipe <phase>.yaml
   --output-format stream-json` and maps the streamed events into BOI's internal event
   type — no memory of the previous phase. Deterministic phases (worktree setup,
   running `verifications`, git commit/merge, worktree teardown) are native Rust in
   `DeterministicExecutor` — no LLM call at all.
6. **Verify gates decide what merges.** Every `{ command = "..." }` gate must exit 0;
   every `{ intent = "..." }` gate is judged by an LLM against the evidence the phase
   produced. A failing gate can route to a plan-revision phase instead of just failing
   the task — the spec isn't frozen the instant it's dispatched.
7. A task that passes its gates commits on its own branch
   (`spec/<spec-id>/<task-id>`) and merges into the spec's integration branch
   (`spec/<spec-id>/integration`). Once the whole spec passes, the spec-level `merge`
   phase fast-forward-merges the integration branch into `base_branch`, and
   `teardown` removes the worktrees.

Watch any of this live, read-only, with `boi dashboard`.

## Isolation: git worktrees, not sandboxes

Every task runs in its own git worktree under `~/.boi/v2/worktrees/<spec-id>/`, on its
own branch. Worktrees share git objects with the main checkout (cheap to create,
disk-efficient) but give each task's phases a private working copy — parallel tasks in
the same spec never clobber each other's uncommitted files. Worktrees are **ephemeral
and engine-managed**: never edit files inside one, they're destroyed at teardown.

This is source-control isolation, not a security sandbox — a worker runs shell
commands as the invoking user with no per-action approval. See
[docs/security.md](security.md).

## Process & state model

| Where | What lives there |
|---|---|
| `~/.boi/v2/boi.db` | SQLite (WAL mode). Tables: `specs`, `spec_versions`, `spec_runtime`, `task_runtime`, `task_deps`, `phase_runs`, `decisions`. Every state transition is an atomic transaction; illegal transitions are rejected loudly by the state machine, never silently dropped. |
| `~/.boi/v2/daemon.sock` | The Unix control socket between short-lived CLI clients and the one long-running daemon. |
| `~/.boi/v2/worktrees/<spec-id>/` | Per-task and per-spec-integration git worktrees. Ephemeral. |
| `~/.boi/v2/phases/*.toml`, `pipelines/<name>.toml` | Phase and pipeline declarations the daemon loads at boot. `standard` is the only pipeline today. |
| `~/.boi/v2/recipes/` | Scratch directory for the Goose recipes `GooseRuntime` generates per worker invocation. |
| `~/.boi/v2/traces/` | OTel JSONL traces — `boi traces` / `boi failures` query these (see [docs/telemetry.md](telemetry.md)). |
| `~/.boi/v2/secrets/*.env` | Provider credentials, loaded into the process environment at startup — never passed on a command line or held in the daemon's supervisor plist. |

The daemon (`boi daemon serve`) is the only long-running process: it owns the event
bus, the orchestrator, and the one connection pool. Everything else — `dispatch`,
`dashboard`, `cancel`, `log`, and so on — is a fresh OS process that exits after one
command.

## Why it's built this way

**Fresh process per phase, not a long agent session.** Long-running agent
conversations degrade — context fills up, instructions get lost, the model drifts.
Every phase in BOI starts a brand-new `goose run` (or a brand-new deterministic step)
that reads its context from the composed `<phase_context>` and the spec/task rows in
SQLite. State lives in git and SQLite, never in an LLM's context window.

**A verify gate between "the agent says it's done" and "it merges."** `verifications`
are declared in the spec, not decided by the worker. Nothing lands on `base_branch`
without passing every gate — command gates are `sh -c` shell checks, intent gates are
LLM-judged against concrete evidence, but both must pass before `commit` / `merge`
runs.

**A DAG, not a linear queue.** `blocked_by` lets independent tasks in the same spec
run in parallel, each in its own worktree, while dependent tasks wait. The spec isn't
"one task at a time" — it's a graph the scheduler walks.

**One daemon, many short-lived clients.** Keeping the event bus and orchestrator in a
single long-running process avoids coordinating state across multiple processes; every
CLI invocation being a thin client (control-socket write, or direct DB read) keeps
`boi dispatch` / `boi dashboard` / `boi cancel` fast and stateless themselves.

**GitFlow-aware by default.** A workspace can declare its branch model in a committed
`.boi-policy.toml`. Under GitFlow, the engine refuses to deliver directly to protected
branches (`main`) — specs land on `develop`, and promotion to `main` is a separate,
human-run release ceremony.

## Where to go deeper

| Doc | Covers |
|---|---|
| [AGENTS.md](../AGENTS.md) | Canonical CLI + spec-format reference, module dependency rule, build/test commands |
| `src/lib.rs` and each `src/*/mod.rs` | The Layered Domain Architecture (`types → config → repo → service → runtime → cli`) and per-module design notes — the ground truth if anything here goes stale |
| [docs/getting-started.md](getting-started.md) | Build, secrets, daemon lifecycle, first dispatch |
| [docs/security.md](security.md) | Trust model — what a worker can touch |
| [docs/telemetry.md](telemetry.md) | OTel spans, `boi traces` / `boi failures` |
| [docs/design/2026-05-21-boi-dashboard-tui-design.md](design/2026-05-21-boi-dashboard-tui-design.md) | The dashboard TUI's data model |
