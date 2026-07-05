# AGENTS.md — runtime (LDA layer 4, execution)

Where work actually runs: native deterministic phases + LLM phases + OTel + DuckDB +
git plumbing. **The ONLY layer allowed to spawn subprocesses or call `git2`** (enforced by
`scripts/checks/no-subprocess-outside-runtime.sh` + `git2-calls-spawn-blocking.sh`).

- Enter at `mod.rs` for the layer `//!`.
- Deterministic half: `deterministic.rs` (`DETERMINISTIC_PHASES` + `resolve()`),
  `steps_executor.rs`, `git_ops.rs` (lowest-level `git2`), `worktree.rs` (the §5
  worktree-per-task mechanic), `conflict.rs`, `preflight.rs`, `validate.rs`,
  `branch_policy.rs` (the workspace branch-model decision core + the
  `.boi-policy.toml` committed-tree loader).
- LLM half: `goose.rs` (the `GooseRuntime` adapter — **read its `//!`**: subprocess
  lifecycle, kill-on-drop, retry/cancel guarantees), `recipe.rs`, `executor.rs`,
  `stream.rs`, `tool_host.rs`, `mcp_server.rs`, `secrets.rs` (provider credentials
  from `~/.boi/v2/secrets/*.env`, loaded at daemon startup).
- Observability: `otel.rs`, `otel_export.rs`, `otel_hoover.rs`, `duckdb.rs` (gated on the
  `duckdb` feature).
- Rule: long-running `git2`/DuckDB calls must use `spawn_blocking` (lint-enforced).
