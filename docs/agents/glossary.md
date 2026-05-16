# Glossary

Quick-lookup definitions for BOI's core concepts. See [ARCHITECTURE.md](../../ARCHITECTURE.md) for how these fit together.

| Concept | Definition | Where in code |
|---------|-----------|---------------|
| **Spec** | A YAML file defining a unit of work: title, mode, tasks, dependencies | `src/spec.rs` |
| **Task** | One item within a spec; has id, title, spec text, verify command, status | `src/spec.rs:123` (`BoiTask` struct) |
| **Iteration** | One attempt at executing a task (retry = new iteration) | `src/queue.rs` (iterations table) |
| **Status: queued** | Spec is waiting for daemon to pick it up | `src/queue.rs` |
| **Status: assigning** | Atomic transitional state — daemon has claimed spec, preventing double-dispatch | `src/queue.rs:545` |
| **Status: running** | Worker is actively executing the spec | `src/worker.rs` |
| **Worker** | An OS thread (`LocalThreadPool`) or remote machine (Fly) executing one spec | `src/pool/` |
| **Daemon** | Background process that polls queue, dispatches workers, monitors health | `src/cli/daemon.rs` |
| **Worktree** | Ephemeral git worktree at `~/.boi/worktrees/<spec-id>/` — destroyed after spec | `src/workspace/git.rs` |
| **Phase** | A named execution step (e.g., `execute`, `task-verify`) configured via TOML | `src/phases.rs`, `phases/*.phase.toml` |
| **Pipeline** | Ordered sequence of phases for a mode (e.g., `generate` mode) | `phases/pipelines.toml` |
| **Hook** | Subprocess fired on lifecycle events; configured in `~/.boi/hooks.yaml` | `src/hooks.rs:20-33` |
| **Telemetry event** | Append-only JSONL entry at `~/.boi/telemetry/boi.jsonl` | `src/telemetry.rs` |
| **Mode** | Execution strategy selecting which pipeline to run (`execute`, `challenge`, `discover`, `generate`, `v2`) | `phases/pipelines.toml` |
| **Workspace backend** | Pluggable isolation strategy; default is `GitWorkspace` (git worktrees) | `src/workspace/mod.rs:21` |
| **Worker pool** | Pluggable execution backend; default is `LocalThreadPool` | `src/pool/mod.rs:52` |
| **Runtime** | LLM backend (`claude`, `codex`, `openrouter`) | `src/runtime/` |
