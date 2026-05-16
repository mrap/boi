# Code Conventions

Patterns and standards used across the BOI codebase.

## Coding patterns

| Concern | Pattern | Exemplary file |
|---------|---------|---------------|
| Error handling (main crate) | `anyhow::Result` | `src/config.rs` |
| Error handling (lib crates) | `thiserror` enums | `crates/boi-cluster/src/client.rs` |
| Async runtime | tokio `rt-multi-thread` | `src/cli/daemon.rs` |
| Logging (main crate) | `boi_log!` macro → `eprintln!` with timestamp | `src/worker.rs:17-21` |
| Logging (cluster crates) | `tracing::{info,warn,debug,error}` | `crates/boi-node/src/main.rs` |
| Tests | Inline `#[cfg(test)]` modules at bottom of file | `src/queue.rs` |
| SQLite test isolation | `serial_test::serial` attribute | `src/queue.rs` |
| Atomic file writes | Write to `.tmp` then `mv` | TODO: verify — prescribed in BOI worker spec, no existing src/ example found |
| Config | YAML via `serde_yml` into typed structs | `src/config.rs:244` |
| CLI | `clap` derive with `Commands` enum | `src/main.rs:54` |
| Formatting | `cargo fmt` (default rustfmt) | — |

## Git conventions

| Item | Pattern |
|------|---------|
| Commit prefixes | `feat:`, `fix:`, `test:`, `release:`, `salvage:`, `merge:` |
| Branch naming | `mrap/<feature-name>`, `fix/<description>`, `boi/<spec-id>` (auto-created for worktrees) |

**Note:** `CONTRIBUTING.md` describes the Python era — it is stale. Use `cargo` commands for all development. See [guardrails.md](guardrails.md).

See also: [adding-features.md](adding-features.md) for change recipes, [ARCHITECTURE.md](../../ARCHITECTURE.md) for system structure.
