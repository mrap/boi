# Getting Started

Operator quickstart: build the `boi` binary, bootstrap provider secrets, start the
daemon, dispatch a first spec, and watch it run. For the full command surface and
the spec format, see the CLI table in the root [AGENTS.md](../AGENTS.md).

## Prerequisites

- **Rust 1.85+** and a C compiler (the bundled DuckDB build needs one)
- **git** (worker phases run in git worktrees)
- **goose** on `PATH`, version `>=1.34, <2.0` — preflight gates every dispatch on it
- **A coding-agent CLI, installed and authenticated** — the default provider is
  [Claude Code](https://github.com/anthropics/claude-code); its token goes in
  `~/.boi/v2/secrets/claude.env` (the Secrets section below walks through it)

## Build

```bash
cargo build --release --locked
```

The binary lands at `target/release/boi`. Dev-loop variants (`cargo check
--no-default-features`, `just check`) are in the README and root AGENTS.md.

## Secrets

Provider credentials live in `~/.boi/v2/secrets/*.env` — never in the LaunchAgent
plist. Every `boi` process loads each `KEY=value` line from those files into its
environment at startup, before the async runtime spawns.

```bash
mkdir -p ~/.boi/v2/secrets && chmod 700 ~/.boi/v2/secrets
printf 'CLAUDE_CODE_OAUTH_TOKEN=...\n' > ~/.boi/v2/secrets/claude.env
chmod 600 ~/.boi/v2/secrets/claude.env
```

Loader rules (enforced, loud):

- The directory must be `0700` and every `*.env` file `0600` — a group/world-readable
  mode is a startup error naming the exact `chmod` to run. Symlinked files are fine;
  the symlink *target* must also be `0600`.
- A missing secrets directory is a valid state (skipped silently).
- Loaded key *names* are logged to stderr; values never are. Malformed lines (no `=`)
  are logged and skipped.
- The default `claude_code` provider authenticates through its own CLI session, not an
  env var; `ANTHROPIC_API_KEY` is only preflight-checked for the raw `anthropic` provider.

## Start the daemon

The daemon is the one long-running process (orchestrator + event bus). Write-side
commands (`dispatch`, `cancel`, `unblock`, `resolve-conflict`, `fail`) reach it over
the Unix control socket at `~/.boi/v2/daemon.sock` and **fail loud, non-zero** if no
daemon is running.

```bash
boi daemon start     # install (idempotent) + start the per-user background service
boi daemon status    # → com.boi.daemon: Running (pid …)
```

`boi daemon serve` runs the boot loop in the foreground — the installed service rides
on this exact form. `boi daemon stop` / `restart` manage the service (restart picks up
a newly built binary). The service is registered as `com.boi.daemon`, restarts on
crash, starts at login, and writes stdout/stderr to `~/.boi/v2/logs/daemon.log`.

## Dispatch your first spec

Specs are TOML; the runnable examples are `tests/fixtures/specs/*.toml`. The smallest,
`01_minimum.toml`, is just a title, a `[contract]`, and one `[[tasks]]` entry:

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

Copy it, point `workspace` at a real git repo of yours and `base_branch` at the
branch work should land on. Note: if the target workspace declares GitFlow (a
committed `.boi-policy.toml` marker at its root), use `base_branch = "develop"` —
the engine refuses to deliver onto the marker's protected branches (default:
`main`). For an unmanaged workspace (no marker), use its default branch (e.g.
`main`). Then:

```bash
boi dispatch my-spec.toml
# Persisted spec <spec-id> (1 task(s)) — queued.
# spec <spec-id> started
```

Dispatch parses + validates the TOML, persists the spec in one transaction, then asks
the daemon to start it. The daemon runs preflight (goose version, provider credentials)
before any phase spends tokens — a preflight failure marks the spec failed with the
detail, and zero phases run. If the daemon is down, the spec stays `queued` and the
command exits non-zero; start the daemon and dispatch again.

**Writing `command` verification gates for Rust workspaces:** the engine runs
every verification command (and every worker) with a shared, warm
`CARGO_TARGET_DIR` injected (`~/.boi/v2/cargo-target`) so fresh worktrees don't
rebuild ~1148 crates cold. Consequence: `cargo` artifacts do **not** land in the
worktree's `target/`. A gate written as

```toml
{ name = "bin-builds", command = "cargo build --release && test -x target/release/mybin" }
```

fails on every attempt — the build goes to the shared dir while the relative
`target/...` check resolves against the worktree. Write artifact checks as
`${CARGO_TARGET_DIR:-target}/release/mybin`, and when pre-testing a gate locally
before dispatch, export `CARGO_TARGET_DIR` so your local run matches the
engine's environment. Do **not** symlink the worktree's `target/` to the shared
dir: it holds binaries from other specs, so an existence check would silently
pass against a stale artifact. (A shell-level
`export CARGO_TARGET_DIR=…` inside the command still wins if a gate truly needs
a private target dir.)

## Watch progress

```bash
boi dashboard            # TUI — recent-specs picker; `boi dashboard <spec-id>` opens one spec
boi log <spec-id>        # phase-run history, in-flight rows marked [running]
```

Both are read-only (they read the DB and traces directly — no daemon needed). For
recovery commands (`cancel`, `unblock`, `fail`, `resolve-conflict`, `clean`,
`spec show`, `traces`, `failures`), see the CLI table in the root AGENTS.md.

## Where state lives

| Path | What |
|---|---|
| `~/.boi/v2/boi.db` | SQLite database (specs, tasks, phase runs) |
| `~/.boi/v2/daemon.sock` | control socket |
| `~/.boi/v2/logs/daemon.log` | daemon stdout/stderr |
| `~/.boi/v2/worktrees/<spec-id>/` | ephemeral integration + per-task worktrees — never edit; removed at teardown |
| `~/.boi/v2/traces/` | OTel JSONL traces (`boi traces` / `boi failures` query these) |
| `~/.boi/v2/secrets/` | operator `*.env` credential files |
| `~/.boi/v2/phases/`, `pipelines/`, `recipes/` | phase/pipeline declarations + Goose recipe scratch |

## Troubleshooting

Start with [docs/agents/debugging.md](agents/debugging.md) — log locations, common
failure shapes, and how to inspect a stuck spec.

## See also

Root [AGENTS.md](../AGENTS.md) (canonical CLI table + spec format) · `src/lib.rs`
(crate architecture doc) · [docs/security.md](security.md) ·
[docs/agents/glossary.md](agents/glossary.md) · [docs/agents/invariants.md](agents/invariants.md)
