# boi

**Dispatch autonomous coding agents in parallel, from a spec.**

BOI ("Beginning of Infinity") is a single-binary Rust harness that orchestrates
LLM-powered software-engineering tasks. You hand it a spec — a TOML file
describing one or more tasks and a DAG of dependencies between them — and it
runs each task through a phase pipeline in its own isolated git worktree,
verifies the result against gates you define, and merges the work back. A live
TUI dashboard shows you exactly what every worker is doing while it runs.

Think of it as an orchestration layer for coding agents (currently
[Claude Code](https://github.com/anthropics/claude-code) via
[Goose](https://github.com/block/goose)): you describe the work and the
acceptance criteria, BOI fans it out to parallel workers, and nothing merges
until it proves itself.

## Why

Long agent sessions degrade — context fills up, instructions get lost, agents
drift. BOI runs every phase as a fresh, isolated process instead: no
accumulated context, durable state in SQLite and git, and a verify gate
between "the agent says it's done" and "the work actually lands." Multiple
tasks with dependencies run in parallel, each in its own worktree, so they
never clobber each other.

## Prerequisites

- **Rust 1.85+** and a C compiler — the bundled DuckDB dependency compiles
  from source on the first build, which takes several minutes.
- **git** — worker phases run in isolated git worktrees.
- **[goose](https://github.com/block/goose)**, `>=1.34, <2.0`, on `PATH` —
  preflight gates every dispatch on this exact version range.
- **A coding-agent CLI, installed and authenticated.** The default provider
  is [Claude Code](https://github.com/anthropics/claude-code): install it,
  authenticate it, and place a `CLAUDE_CODE_OAUTH_TOKEN` in
  `~/.boi/v2/secrets/claude.env` (see the Secrets section of
  [docs/getting-started.md](docs/getting-started.md) for the exact steps).
  Preflight runs a live authenticated probe against the provider before every
  dispatch, so an installed-but-unauthenticated CLI fails loud rather than
  silently.

## Quickstart

**Install (builds from source — no prebuilt binaries yet):**

```bash
curl -fsSL https://raw.githubusercontent.com/mrap/boi/main/install.sh | bash
```

Checks prerequisites, `cargo install`s `boi` onto your `PATH`, and seeds the
default phase/pipeline declarations `boi daemon` needs at boot. See
`install.sh --help`, or install manually:

```bash
git clone https://github.com/mrap/boi
cd boi
cargo build --release --locked   # first build compiles DuckDB from source — expect several minutes
```

```bash
target/release/boi daemon start          # install + start the background service
target/release/boi dispatch my-spec.toml # parse, validate, persist, and run a spec
target/release/boi dashboard             # watch it work
```

Full walkthrough — secrets setup, the daemon lifecycle, writing your first
spec, troubleshooting — is in **[docs/getting-started.md](docs/getting-started.md)**.
Realistically, first-time setup (installing `goose`, authenticating a
provider, the cold DuckDB compile) takes 20–40 minutes, not 10.

## Spec format

Specs are TOML. The runnable canonical examples are
[`tests/fixtures/specs/`](tests/fixtures/specs/) — start with
[`01_minimum.toml`](tests/fixtures/specs/01_minimum.toml), then
[`02_multi_task_dag.toml`](tests/fixtures/specs/02_multi_task_dag.toml) for a
multi-task DAG with dependencies.

```toml
title = "Add rate limiting to API"

[contract]
scope = "Add token-bucket rate limiting middleware to all /api routes"
base_branch = "develop"
workspace = "~/github.com/you/api"

[[tasks]]
ref = "setup-middleware"
behavior = "Create token_bucket middleware module"
verifications = [
  { intent = "Unit tests pass for new middleware::token_bucket module" },
]
```

- `[contract]` — the shared brief: scope, target workspace, base branch.
- `[[tasks]]` — one entry per unit of work; `blocked_by = ["other-ref"]` wires
  a dependency DAG.
- `verifications` — atomic gates, each either `{ command = "..." }` (shell,
  must exit 0) or `{ intent = "..." }` (judged by an LLM against the evidence
  the worker produced). Nothing merges until every gate for a task passes.

## CLI reference

| Command | Does |
|---|---|
| `boi dispatch <spec.toml>` | parse, validate, persist, and start a spec |
| `boi dashboard [spec-id]` | spec-observability TUI — live DAG/phase state, read-only |
| `boi log <spec-id>` | phase-run history for one spec |
| `boi cancel <id> --reason "…"` | cancel a spec or a single task |
| `boi unblock <task-id> [--reset-counter]` | force a blocked task back to active |
| `boi resolve-conflict <task-id>` | resolve a task's merge conflict in an interactive shell |
| `boi fail <spec-id> --reason "…"` | operator-marked failure |
| `boi clean <spec-id>` | delete a spec and its cascade |
| `boi spec show <spec-id>` | dump the stored spec snapshot |
| `boi daemon <serve\|install\|start\|stop\|status\|restart>` | service lifecycle |
| `boi traces` / `boi failures` | OTel-backed recurring-failure queries (needs the `duckdb` feature) |
| `boi completions <shell>` | emit a shell completion script |
| `boi mcp-serve` | one stdio MCP server bound to a single worker's phase run |

Run `boi <cmd> --help` for exact signatures. Full command + spec-format
reference: [AGENTS.md](AGENTS.md).

## How it works

1. `boi dispatch` parses and validates a spec, persists it, and asks the
   background daemon to start it.
2. Each task runs in its own git worktree. Independent tasks (per the DAG)
   run in parallel; dependent tasks wait on `blocked_by`.
3. Each task moves through a phase pipeline (plan → work → verify → merge),
   with every LLM phase a fresh, stateless agent process — no accumulated
   context between phases.
4. Verify gates run before anything merges. A failing gate can trigger a
   plan-revision phase rather than just failing outright — the spec isn't
   frozen once dispatched.
5. Passing tasks merge to a per-spec integration branch, then to the spec's
   `base_branch`.

Watch it happen live with `boi dashboard` — see
[docs/design/2026-05-21-boi-dashboard-tui-design.md](docs/design/2026-05-21-boi-dashboard-tui-design.md)
for the design rationale.

## Documentation

| Doc | Covers |
|---|---|
| [docs/getting-started.md](docs/getting-started.md) | Full operator quickstart |
| [AGENTS.md](AGENTS.md) | Canonical CLI + spec-format reference, architecture map |
| [docs/architecture.md](docs/architecture.md) | Crate-level architecture |
| [docs/security.md](docs/security.md) | Trust model — read this before dispatching untrusted spec content |
| [docs/telemetry.md](docs/telemetry.md) | OTel spans/traces, `boi traces` / `boi failures` |
| [docs/faq.md](docs/faq.md) | Philosophy and identity questions |
| [docs/agents/](docs/agents/) | Contributor-facing: glossary, conventions, debugging, invariants |

## Security

BOI workers run as you, with no per-action approval — a spec's
`verifications` `command` fields execute verbatim through `sh -c`, and
workers can run any shell command your user can. **Only dispatch specs you
wrote or reviewed.** Worktrees are source-control isolation, not a sandbox.
Read [docs/security.md](docs/security.md) before pointing BOI at anything
you don't fully trust.

## Status

BOI is under active development (currently v3.3.2). The test suite is 776
tests, green. Not yet shipped: prebuilt release binaries — `install.sh` (and
manual `cargo install`) both build from source for now.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).
