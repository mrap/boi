# BOI — Agent Onboarding

BOI (Beginning of Infinity) is a Rust binary that dispatches Claude Code workers to execute spec-defined tasks in isolated git worktrees. It manages a SQLite-backed queue, a phase-based execution pipeline, lifecycle hooks, and structured telemetry. BOI has zero hex-specific code — [hex](~/github.com/mrap/hex-foundation/AGENTS.md) is the operator layer that configures BOI hooks and wires lifecycle events to system automation.

## How do I run it?

Prerequisites: Rust toolchain, `ANTHROPIC_API_KEY` in `~/.boi/.env`.

```bash
cargo build --release
cp target/release/boi ~/.local/bin/boi   # or: cargo install --path .

boi daemon start          # must be running before dispatch
boi dispatch spec.yaml    # queue a spec
boi status                # check all specs
boi status <spec-id> -v   # detailed status for one spec
```

Without a running daemon, dispatched specs queue silently and never execute. Use `boi doctor` to verify.

## How do I verify it?

```bash
cargo build --release   # must compile without errors
cargo test              # must pass (one #[ignore]'d test — see docs/agents/debugging.md)
cargo clippy            # check for warnings
boi doctor              # daemon + DB + worktree health
```

## Repo layout (one-liner)

`src/` is the main binary. `crates/` has auxiliary libs (cluster, identity, proto, plugins). `phases/` has pipeline configs. `docs/` has design docs + `docs/agents/` for agent-facing topic docs.

## Where to read next

| Doc | What's in it |
|-----|-------------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | System map: crates, lifecycle, state files, hook points |
| [docs/agents/invariants.md](docs/agents/invariants.md) | What NOT to break — migrations, SQLite, hook payloads |
| [docs/agents/guardrails.md](docs/agents/guardrails.md) | Things not to do |
| [docs/agents/debugging.md](docs/agents/debugging.md) | When a spec fails; diagnostic commands; known issues |
| [docs/agents/adding-features.md](docs/agents/adding-features.md) | Recipes: CLI subcommand, crate, hook event, migration, phase |
| [docs/agents/conventions.md](docs/agents/conventions.md) | Error handling, async, logging, testing, git conventions |
| [docs/agents/spec-format.md](docs/agents/spec-format.md) | Spec YAML schema and pipeline modes |
| [docs/agents/cli-reference.md](docs/agents/cli-reference.md) | Full CLI command reference with all flags |
| [docs/agents/glossary.md](docs/agents/glossary.md) | Spec / task / iteration / worker / phase / pipeline defined |
