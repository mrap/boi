# Glossary

Quick-lookup definitions for BOI's core concepts. For how the pieces fit together,
read the crate doc in `src/lib.rs` (the Layered Domain Architecture) and the root
[AGENTS.md](../../AGENTS.md) — the canonical CLI table and spec format live there.
The runnable spec examples are `tests/fixtures/specs/*.toml`.

| Concept | Definition | Where in code |
|---------|-----------|---------------|
| **Contract** | The immutable authored intent of a spec — the `[contract]` block: `scope`, `workspace` (XOR `workspace_rationale`), `base_branch`, `exclusions`, `verifications`, `must_emit`. Each task carries its own `TaskContract` (`behavior` + verifications) | `src/types/context.rs` (`SpecContract`, `TaskContract`) |
| **DAG / `blocked_by`** | Task-dependency edges declared as `[[tasks]].blocked_by = ["ref", …]`. Cycles, dangling refs, and duplicate refs are rejected at validation; dispatch persists the edges to the `task_deps` table with foreign keys on both columns, so a dangling dep is impossible | `src/config/validate.rs`, `src/repo/task_deps.rs` |
| **Daemon** | The long-running process (`boi daemon serve` — what the installed service rides) hosting the orchestrator, event bus, and control socket at `~/.boi/v2/daemon.sock`. Write-side commands (`cancel`, `unblock`, `resolve-conflict`, `fail`) are control-socket clients; no daemon means a loud non-zero exit, never a DB-only flip | `src/cli/daemon.rs`, `src/cli/recover.rs` |
| **Dashboard** | The read-only spec-observability TUI (`boi dashboard`). Reads SQLite (`phase_runs`) for structure and tails the OTel trace JSONL for per-phase events; needs no daemon. Replaces v1's `boi status` | `src/cli/dashboard/` |
| **Delivery** | How a finished spec's integration branch lands. Only `merge` (fast-forward to the base branch, the default) is supported today — `pr` and `branch-only` parse but are rejected at dispatch (`UnsupportedDelivery`) | `src/config/spec.rs` (`Delivery`) |
| **Event bus** | The single chokepoint for state-machine transitions and the observational event log, with a multi-phase emit. `transitions.rs` arbitrates inside the emit — an illegal transition fails loudly, never a silent flip | `src/service/bus.rs`, `src/service/transitions.rs` |
| **Integration branch** | `spec/<SpecId>/integration` — the branch each task branch merges into; its worktree is `<worktree_root>/<SpecId>/integration`. Delivery decides how it lands | `src/runtime/worktree.rs` (`integration_branch`) |
| **Orchestrator** | The daemon's event loop driving the `standard` pipeline end-to-end: consumes bus events, clocks phases in and out, and routes each verdict to the next phase | `src/service/orchestrator.rs`, `src/service/routing.rs` |
| **Phase** | One named pipeline step, declared in `~/.boi/v2/phases/<name>.toml`. `level` = `spec` or `task`; `kind` = `worker` (LLM, routed to `GooseRuntime`) or `deterministic` (native Rust fn resolved via `deterministic::resolve()` from the `DETERMINISTIC_PHASES` set — workspace prep, verify, commit, merge, teardown). Verdict routing lives under `[on.<verdict>]` | `src/config/phase.rs`, `src/runtime/deterministic.rs` |
| **Phase run** | One execution of a phase — a row in the `phase_runs` table, inserted at `PhaseStarted` and completed with synopsis / verdict / files touched. `UNIQUE(spec_id, task_id, phase, phase_iteration)` catches retry storms; a live run heartbeats every ~30 s | `src/repo/phase_runs.rs` |
| **Pipeline** | An ordered composition of phase names in `~/.boi/v2/pipelines/<name>.toml`; the literal `<tasks>` entry marks where the orchestrator fans out the per-task lifecycle in parallel. v1.0 ships the `standard` pipeline only. Per-phase provider/model overrides go under `[overrides.<phase>.runtime]` | `src/config/pipeline.rs` |
| **Spec** | A TOML file — `title`, `pipeline`, `delivery`, `[contract]`, `[[tasks]]` — parsed with `deny_unknown_fields`, validated, and normalized to the typed `Spec`. There is no `mode` field: it gets a typed rejection ("modes were removed in v1.0") | `src/config/spec.rs` |
| **Spec status / Task state** | Spec: `queued` · `running` · `completed` · `failed` · `canceled`. Task: `not_started` · `active` · `blocked` · `passing` · `canceled`. Stable lowercase strings — the SQLite storage contract | `src/types/state.rs` |
| **SpecId / TaskId** | 9-char IDs: an uppercase type prefix (`S` spec, `T` task, `P` phase run, `D` decision) + 8 random Crockford-base32 chars (lowercase, no `i`/`l`/`o`/`u`), e.g. `Sxk3m9p2q`. Newtypes validate the format; generation retries PK collisions, capped at 5 | `src/types/ids.rs`, `src/repo/ids.rs` |
| **Sweeper** | The daemon's periodic check for abandoned phase runs — rows still open whose heartbeat went stale (a dead or hung worker). Emits `TaskBlocked` (or `SpecFailed` for a spec-level run) for operator recovery; never auto-retries | `src/service/sweeper.rs` |
| **Task** | One work unit in a spec: an optional `ref` slug (used by `blocked_by`), a one-line `behavior`, `blocked_by` refs, and at least one verification | `src/config/spec.rs` (`RawTask`, `TaskDef`) |
| **Verdict** | A worker phase's structured output: a mandatory `synopsis` plus an outcome — `passing` (with evidence), `redo`, `blocked`, or `fail`. The phase TOML routes the next phase by matching on the outcome | `src/types/verdict.rs` |
| **Verification** | One atomic check — exactly one of `command` (shell; exit code checked deterministically) or `intent` (LLM-judged); setting both or neither is a typed parse error | `src/types/context.rs` (`Verification`), `src/config/validate.rs` |
| **Worker** | The LLM side of a `worker` phase: `GooseRuntime` renders a Goose recipe and spawns `goose run --recipe <file>.yaml --output-format stream-json`, mapping the child's stream into events and a final verdict | `src/runtime/goose.rs`, `src/runtime/recipe.rs` |
| **Worktree** | An ephemeral git worktree under `~/.boi/v2/worktrees/<SpecId>/` — `integration` plus one per task at `<root>/<SpecId>/<TaskId>` on branch `spec/<SpecId>/<TaskId>`. Never edit files there by hand; the teardown phase removes them | `src/runtime/worktree.rs` |

## Lifecycle in one breath

`boi dispatch` persists a new spec (`specs`, `spec_versions`, `spec_runtime`,
`task_runtime`, `task_deps`) in one transaction; it lands `queued`. The daemon's
`Dispatch` handler emits `SpecStarted` (`queued → running`), then runs preflight — a
preflight failure is a legal `running → failed` with zero phase runs. The orchestrator
then walks the pipeline's spec phases, fans out the per-task lifecycle in parallel at
the `<tasks>` boundary, and routes each phase's verdict until the spec terminates.

## Runtime layout (`~/.boi/v2/`)

The full path-by-path table of what lives under `~/.boi/v2/` is in
[docs/getting-started.md](../getting-started.md) ("Where state lives") — read it
when you need to find state on disk.

See also: the root [AGENTS.md](../../AGENTS.md) (commands + spec format),
[invariants.md](invariants.md), [conventions.md](conventions.md),
[debugging.md](debugging.md), [guardrails.md](guardrails.md),
[adding-features.md](adding-features.md), [docs/security.md](../security.md),
[docs/getting-started.md](../getting-started.md).
