# AGENTS.md — BOI

Single-binary Rust harness that orchestrates LLM-powered software-engineering tasks:
you hand it a spec (a TOML file of tasks), it runs each task through a phase pipeline
in an isolated git worktree, verifies, and merges. This file is the cold-start map for
an agent working **in this repo**. It is canonical for **commands and spec format** —
where it disagrees with `docs/`, trust this file and `boi --help`.

This checkout is the **canonical BOI engine** (TOML specs, control-socket daemon,
`~/.boi/v2/boi.db`; version: `boi --version` / `Cargo.toml`).

## Start here

| To understand… | Read |
|---|---|
| The whole crate's shape | `src/lib.rs` (the `//!` — Layered Domain Architecture) |
| Why it's built this way | `docs/architecture.md` |
| One subsystem | that module's `//!` doc (every `src/*/mod.rs` and most files carry one — e.g. `src/runtime/goose.rs` is the exemplar) |
| Domain terms | `docs/agents/glossary.md` |
| Trust model / what workers can touch | `docs/security.md` |

## Build, test, verify (run these)

```bash
cargo build --locked            # full build (bundled DuckDB needs a C compiler)
cargo check --no-default-features   # fast dev loop, skips DuckDB
just check                      # fmt + clippy -D warnings + test + doc (the gate)
just ci                         # just check + the LDA/script lint suite
just lint-scripts               # the architecture/guardrail check suite (scripts/checks/)
```

- CI mirrors `just check` + `just lint-scripts` (`.github/workflows/ci.yml`).
- The `repo` layer uses `sqlx::query!` macros verified at compile time against a
  committed `.sqlx/` cache; CI builds with `SQLX_OFFLINE=true`. Regenerate the cache
  only when a `query!` changes: `just prep-sqlx` (needs `sqlx-cli` + a `.env` with
  `DATABASE_URL=sqlite://.dev.db`). Commit the `.sqlx/` delta with the query change.
- `cargo test` is unit + integration. Real-`goose` runs are Docker, out-of-band:
  `just e2e` (Ollama) / `just e2e-openrouter` — they need Docker + network, never in `just ci`.

## Architecture (one rule)

```
src/types → src/config → src/repo → src/service → src/runtime → src/cli
```

Forward-only dependencies, enforced by `scripts/checks/module-dep-audit.sh`. LLM phases
route through `runtime::goose::GooseRuntime` (`goose run --recipe X.yaml`); deterministic
phases (workspace verify, validate, commit, merge, teardown) are native Rust in
`runtime::deterministic` (`DETERMINISTIC_PHASES` + `resolve()`). Detail: `docs/agents/conventions.md`,
`docs/agents/adding-features.md`.

**Each `src/<layer>/` has its own `AGENTS.md`** — a thin router for that layer (what it
owns, its boundary, the file map, local invariants) pointing to the module `//!` for
depth. Working inside a layer? Read that layer's `AGENTS.md` first.

## CLI (authoritative — from `boi --help`)

| Command | Does |
|---|---|
| `boi dispatch <spec.toml>` | parse, validate, persist, and start a spec |
| `boi log <spec-id>` | phase-run history for one spec |
| `boi dashboard` | spec-observability TUI |
| `boi cancel <id> --reason "…"` | cancel a spec or a single task (reason mandatory) |
| `boi unblock <task-id> [--reset-counter]` | force a blocked task back to active (optionally zero its iteration counter) |
| `boi resolve-conflict <task-id>` | resolve a task's merge conflict in an interactive shell (no `--ai` — deliberate) |
| `boi fail <spec-id> --reason …` | operator-marked failure |
| `boi clean <spec-id>` | delete a spec + cascade (retention) |
| `boi spec show <spec-id>` | dump the stored spec snapshot |
| `boi daemon <serve\|install\|start\|stop\|status\|restart>` | service lifecycle; `serve` is the boot loop (the LaunchAgent rides this) |
| `boi traces` / `boi failures` | OTel queries (needs the `duckdb` build feature) |
| `boi completions <shell>` | emit a shell completion script |
| `boi mcp-serve` | one stdio MCP server bound to a single worker's phase run |

Run `boi <cmd> --help` for the exact signature. There is no top-level `boi status` in
v2 (`boi daemon status` exists for the service) — use `boi dashboard` or query
`~/.boi/v2/boi.db`.

## Spec format (TOML)

`[contract]` + `[[tasks]]`; `pipeline = "standard"`, `delivery = "merge"`. **The
canonical, runnable examples are `tests/fixtures/specs/*.toml`** — read those rather
than a prose schema (start with `01_minimum.toml`, then `02_multi_task_dag.toml`).
Validate before dispatch: `python3 -c "import tomllib; tomllib.load(open('s.toml','rb'))"`.

## Branching & releases (GitFlow)

This repo uses GitFlow. Work lands on `develop` via PR with the required checks green;
`main` moves **only** by a maintainer-run release/hotfix ceremony. Manual
`chore: bump version` commits and hand-made tags are retired — the ceremony owns the
gate battery, the `Cargo.toml` version bump, the merge to main, the tag, and the
back-merge to develop. A push to `main` outside the ceremony is an **incident**:
remediation is to merge `main` into `develop` (restores the main-is-ancestor-of-develop
invariant), then re-run the ceremony.

| Namespace | Created by | Merges to |
|---|---|---|
| `feature/<slug>`, `fix/<slug>` | developers | `develop` (PR, required checks green) |
| `release/X.Y.Z` | release ceremony only | `main` (tagged `vX.Y.Z`), back-merged to `develop` |
| `hotfix/*` | release ceremony (hotfix mode) only | `main` (tagged), back-merged to `develop` |
| `spec/<SpecId>/integration`, `spec/<SpecId>/<TaskId>` | BOI engine | engine-managed — never create by hand |
| `salvage/<SpecId>` | operator (manual rescue of stranded `spec/*` work) | `develop`, never `main` |

**Branch-policy marker:** `.boi-policy.toml` at a workspace root declares the branch
model to the BOI engine — `model = "gitflow" | "trunk"`; `protected` lists branches the
engine must never deliver to (default `["main"]` under gitflow, `[]` under trunk). The
engine reads the marker from the **committed tree of the spec's `base_branch`**
(checkout-independent), and refuses protected-branch deliveries. No marker = unmanaged =
pre-GitFlow behavior. A present-but-invalid marker is a hard, typed error — never
silently ignored.

**Spec-author rule:** BOI specs targeting **this repo** MUST set `base_branch = "develop"`.

## Critical invariants (full list with enforcement citations: `docs/agents/invariants.md`)

- **Migrations are append-only** — never edit an applied `migrations/NNNN_*.sql`; add a
  new numbered file. `sqlx::migrate!("./migrations")` (`src/repo/db.rs`) tracks applied
  versions; there is no down migration.
- **DB pragmas** (`src/repo/db.rs`): `journal_mode=WAL` (concurrent reads during writes)
  + `foreign_keys=ON` (every FK is `ON DELETE RESTRICT`).
- **Worktrees in `~/.boi/v2/worktrees/` are ephemeral** — never edit files there; they're destroyed on cleanup.
- **Verify commands must be idempotent** — a worker may re-run verify on retry.
- **Verification commands run with the shared `CARGO_TARGET_DIR` injected**
  (`~/.boi/v2/cargo-target`, OBS-032) — `cargo` artifacts do NOT land in the
  worktree's `target/`. A gate checking a build artifact must use
  `${CARGO_TARGET_DIR:-target}/release/<bin>`, never a bare `target/...` path
  (that gate now fails deterministically). Never symlink a worktree `target/`
  to the shared dir — it holds other specs' binaries, so `test -x` would
  false-pass.
- A malformed routing graph / phase config is a **loud startup rejection**, never a silent mid-run stall.

## Docs map

| Where | What | When to read |
|---|---|---|
| `docs/agents/` | adding-features, conventions, debugging, glossary, guardrails, invariants | working in this repo — task-specific, on demand |
| `docs/getting-started.md` | operator quickstart (build → secrets → daemon → first dispatch) | running BOI for the first time |
| `docs/architecture.md` | crate-level architecture overview | understanding how the layers fit together |
| `docs/security.md` | trust model — what workers can touch | before dispatching untrusted spec content |
| `docs/telemetry.md` | OTel spans/traces, `boi traces` / `boi failures` | wiring up or querying observability |
| `docs/faq.md` | common operator questions | quick lookups |
| `docs/design/2026-05-21-boi-dashboard-tui-design.md` | design rationale for the shipped `boi dashboard` TUI | understanding the dashboard's data model |

This is a curated subset of the engineering docs that accumulated during development;
it's kept intentionally small for a public audience. If a claim here surprises you,
verify against the source — `src/lib.rs` and each module's `//!` doc are the
ground truth.
