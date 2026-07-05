//! Execution surface: `DETERMINISTIC_STEPS` fn-pointer table for native
//! Rust phases (workspace verify, validate, commit, merge, teardown);
//! `GooseRuntime` adapter for LLM phases; OTel emission; bundled DuckDB
//! query layer; worktree git plumbing.
//!
//! This is the ONLY module allowed to spawn subprocesses (enforced via
//! `scripts/checks/no-subprocess-outside-runtime.sh`) and the only place
//! `git2` calls live.
//!
//! Phases 6 + 7 + 8 populate this module. Phase 6 landed the deterministic
//! half:
//!
//! - [`git_ops`] — the lowest-level `git2` layer (branch / worktree / merge /
//!   rebase / diff primitives).
//! - [`worktree`] — the §5 worktree-per-task mechanic: the 7 worktree
//!   deterministic-phase bodies.
//! - [`validate`] — verification-command execution; the `validate`
//!   deterministic phase.
//! - [`deterministic`] — the `DETERMINISTIC_STEPS` fn-pointer table.
//! - [`steps_executor`] — [`DeterministicExecutor`], the deterministic-phase
//!   `PhaseExecutor` adapter.
//! - [`tool_host`] — [`RuntimeToolHost`], the `WorkerToolHost` adapter.
//!
//! Phase 7 landed the Goose worker half:
//!
//! - [`recipe`] — [`build_recipe`], the Goose recipe builder
//!   ([`GooseRecipe`] serialized via `serde_yaml_ng`).
//! - `stream` — the `stream-json` → `BoiEvent` mapper (`runtime/`-internal —
//!   consumed only by [`goose`]).
//! - [`goose`] — [`GooseRuntime`], the worker-phase `PhaseExecutor` adapter:
//!   it spawns `goose run … --output-format stream-json` and drains the
//!   stream.
//! - [`mod@preflight`] — the pre-dispatch `goose`-version + provider-credential
//!   + provider-liveness-probe gate (the probe is the 429 hardening: a
//!     throttled/rejected credential refuses the dispatch pre-spend).
//! - [`mcp_server`] — [`BoiMcpServer`], the stdio MCP-server transport (one
//!   server child per worker — G14.4).
//! - [`executor`] — [`RuntimeExecutor`], the unified `PhaseExecutor` that
//!   dispatches a phase by `kind` to [`GooseRuntime`] or
//!   [`DeterministicExecutor`].
//!
//! Phase 8a landed the OTel `EmitObserver` adapter:
//!
//! - [`otel_export`] — [`init_tracing`], the canonical-OTLP/JSON file exporter
//!   + [`OtelGuard`].
//! - [`otel`] — [`OtelObserver`], the `EmitObserver` adapter mapping `BoiEvent`
//!   to the §8 span hierarchy.
//! - [`otel_hoover`] — [`hoover_worker_spans`], the worker-OTel hoover
//!   (re-parent + name-normalize).
//!
//! (The bus carries OTel observers only; no emit bridges are wired.)
//!
//! Phase 8c landed the bundled-DuckDB query layer (`#[cfg(feature = "duckdb")]`):
//!
//! - `duckdb` — `open_duckdb` (the DuckDB connection — `ATTACH boi.db`
//!   read-only + the `otlp` extension), `query` (the `boi traces query` SQL
//!   engine), `failures_top` (the `boi failures top` fingerprint aggregation).
//!
//! ## `runtime/` internal dependency graph (acyclic — by inspection)
//!
//! ```text
//! executor ──┬──> goose ──> recipe · stream · (mcp_server is wired by Phase 9)
//!            └──> steps_executor ──> deterministic ──> worktree · validate ──> git_ops
//! preflight                          (independent — called by Phase 9 dispatch)
//! otel ──> otel_hoover ──> otel_export      (otel wired into the bus by Phase 9)
//! duckdb ──> otel                           (the shared FAILURE_FINGERPRINT_ATTR
//!                                            const only; queried by Phase 9 CLI)
//! ```
//!
//! `duckdb → otel` is the only intra-`runtime/` edge Phase 8c adds — `duckdb`
//! reads `otel::FAILURE_FINGERPRINT_ATTR` so the failure-fingerprint emit key
//! and the `boi failures top` GROUP BY key are one symbol. `otel` does not
//! depend back on `duckdb`; the graph stays acyclic (Phase 8c exit gate).
//!
//! ## Re-export surface (review item 26)
//!
//! `runtime/mod.rs` re-exports only what `cli/` (Phase 9) and `boot` consume —
//! the adapters' public constructors, the deterministic-table contract, the
//! recipe builder, the preflight gate, and the MCP-server transport.
//! `runtime/`-internal helpers stay module-qualified: `stream`'s `StreamMapper`
//! / `StreamMapError` are `pub(crate)` (consumed only by [`goose`]). `StepError`
//! is NOT re-exported here — it is a `types/` layer-0 type (G14.1), reached as
//! `crate::types::StepError`.

// GitFlow program (R-B4/R-B5) — the workspace branch-policy decision core +
// `.boi-policy.toml` loader. Pure matrix evaluation plus a committed-tree
// (libgit2 odb) marker read; consumed by the enforcement layers (dispatch
// gate, preflight, worktree re-checks) as they wire in.
pub mod branch_policy;
// Phase 9 — the runtime side of `boi resolve-conflict`: re-create a merge
// conflict and drop the operator into an interactive shell. The subprocess
// spawning lives here (`no-subprocess-outside-runtime.sh`); `cli/` only
// decides when to invoke it.
pub mod conflict;
pub mod deterministic;
// Phase 8c — the bundled-DuckDB query layer. Both the `mod` and its re-export
// (below) are `#[cfg(feature = "duckdb")]`-gated (review S11): bundled DuckDB
// is a ~30 s C++ compile, so a dev `cargo check --no-default-features` skips
// it, and without the gate that build hits `E0432` on the re-export.
#[cfg(feature = "duckdb")]
pub mod duckdb;
pub mod executor;
pub mod git_ops;
pub mod goose;
pub mod mcp_server;
// Conflict-resolver track — the `MergeStrategy` registry surface (salvaged
// from spec Syvwx7psx). The trait, `StrategyOutcome` taxonomy, the
// `ConflictCtx`/`ConflictedFile` invocation types, and the `non_overlapping`
// strategy live here. Staged foundation: the remaining strategies + the
// wiring into `worktree.rs` land when the resolver track activates.
pub mod merge_strategies;
// Phase 8a — the OTel `EmitObserver` adapter, split per review S12: the
// observer (`otel`), the OTLP/JSON file exporter + guard (`otel_export`), the
// worker-OTel hoover (`otel_hoover`). Task 8a.4 prunes the re-export surface.
pub mod otel;
pub mod otel_export;
pub mod otel_hoover;
pub mod preflight;
pub mod recipe;
// Audit C1 — worktree reclamation for failed/canceled specs: the
// `service::sweeper::SpecReclaimer` port's `runtime/` implementation plus
// the `boi clean` disk path.
pub mod reclaim;
pub mod secrets;
pub mod steps_executor;
// `stream` is `runtime/`-internal — consumed only by `goose.rs` (Task 7.3);
// its `StreamMapper` / `StreamMapError` are NOT on the `runtime/mod.rs`
// re-export surface (Task 7.7 / review item 26).
pub(crate) mod stream;
pub mod tool_host;
pub mod validate;
pub mod worktree;

// --- The workspace branch-policy core + loader (GitFlow program R-B4/R-B5) ---
pub use branch_policy::{
    BranchModel, BranchPolicy, PolicyContext, PolicySource, PolicyVerdict, evaluate, load_policy,
    load_policy_blocking,
};
// --- The deterministic-table contract (Tasks 6.2 + 6.4) ---
pub use deterministic::{DetStep, StepRun, resolve};
// --- The two `PhaseExecutor` / `WorkerToolHost` adapters (Tasks 6.5 + 6.6) ---
pub use steps_executor::DeterministicExecutor;
pub use tool_host::RuntimeToolHost;
// --- Typed errors + enums the adapters surface (Tasks 6.1 + 6.2 + 6.3) ---
pub use git_ops::{GitError, MergeOutcome, RebaseOutcome};
pub use validate::{ValidateError, run_command};
pub use worktree::WorktreeError;
// --- The Goose recipe builder (Task 7.1) ---
pub use recipe::{GooseRecipe, RecipeError, build_recipe};
// --- The Goose worker-phase `PhaseExecutor` adapter (Task 7.3) ---
pub use goose::GooseRuntime;
// --- The pre-dispatch preflight check (Task 7.4) ---
// `branch_policy_gate` is the GitFlow Layer-2 backstop (R-B7) the daemon's
// dispatch handler runs beside the goose/provider checks.
pub use preflight::{
    CurlProviderProbe, GOOSE_VERSION_REQ, PreflightError, ProbeOutcome, ProviderProbe,
    branch_policy_gate, preflight,
};
// --- The stdio MCP-server transport (Task 7.5) ---
pub use mcp_server::{BoiMcpServer, McpServerError};
// --- The unified worker-vs-deterministic `PhaseExecutor` (Task 7.6) ---
pub use executor::RuntimeExecutor;
// --- Interactive merge-conflict resolution (Phase 9 — `boi resolve-conflict`) ---
pub use conflict::{ConflictError, ResolveOutcome, resolve_interactively};
// --- Worktree reclamation for terminal specs (audit C1) ---
pub use reclaim::{SpecWorktreeReclaimer, reclaim_spec_worktrees};
// --- The OTel `EmitObserver` adapter (Phase 8a) ---
// `OtelObserver` is wired into the bus by `boot` (Phase 9); `FAILURE_FINGERPRINT_ATTR`
// is the failure-fingerprint key the Phase 8c `boi failures top` query shares.
pub use otel::{FAILURE_FINGERPRINT_ATTR, OtelObserver};
// `init_tracing` is called once at daemon boot; the daemon holds `OtelGuard`
// for its lifetime and drops it last (review S13).
pub use otel_export::{OtelError, OtelGuard, init_tracing};
// The worker-OTel hoover + the BOI-parent ref it re-parents onto — Phase 9
// wires the hoover into the post-worker step.
pub use otel_hoover::{BoiSpanRef, hoover_worker_spans};
// --- The bundled-DuckDB query layer (Phase 8c) ---
// `#[cfg(feature = "duckdb")]`-gated to match the gated `pub mod duckdb` above
// (review S11): a `cargo check --no-default-features` build has no `duckdb`
// module, so an ungated re-export would be an `E0432` unresolved import.
// Phase 9's `boi traces` / `boi failures` subcommands are gated the same way.
#[cfg(feature = "duckdb")]
pub use duckdb::{
    DuckError, DuckHandle, FailureRow, QueryResult, failures_top, open_duckdb, query,
};
