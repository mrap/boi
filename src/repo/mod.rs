//! SQLite persistence: connection pool, migrations, INSERT/UPDATE queries
//! for the 7 tables (specs, spec_versions, spec_runtime, task_runtime,
//! task_deps, phase_runs, decisions) + the `boi clean` cascade + the §7.2
//! composition query + the `boi dispatch` structural insert.
//!
//! This is the ONLY module allowed to use `sqlx::query` macros. JSON `Value`
//! navigation via `serde_json` is permitted in any layer.
//!
//! The `repo` layer of BOI v2's Layered Domain Architecture — depends only on
//! `crate::types` (and `crate::config`). Mutating tables (`spec_runtime`,
//! `task_runtime`) are written here but the event bus (Phase 4) is the sole
//! caller in production; the repo layer itself enforces no state-machine
//! legality.
//!
//! ## Re-export strategy — deviation from the plan's Task 3.12 `*` globs
//!
//! The plan's Task 3.12 prescribes `pub use spec_runtime::*; pub use
//! task_runtime::*; pub use phase_runs::*; pub use decisions::*; ...`. Glob
//! re-exporting every sibling table module is **un-compilable**: `fetch` is
//! defined by `spec_runtime`, `task_runtime`, AND `phase_runs`; `insert*` /
//! `initialize` / `update_*` collide similarly. Overlapping glob re-exports
//! make those names ambiguous at every use site.
//!
//! Resolution: re-export the non-colliding *types* and uniquely-named helpers
//! flat (below); leave the colliding table-operation verbs to be called
//! path-qualified (`spec_runtime::fetch`, `phase_runs::insert_start`,
//! `decisions::insert`, `task_runtime::update_state`, ...). Every table module
//! is `pub`, so the qualified path is always reachable. This honours the
//! plan's intent — a usable flat surface — without the broken glob.

pub mod clean;
pub mod composition;
pub mod db;
pub mod decisions;
pub mod dispatch;
pub mod ids;
pub mod phase_runs;
pub mod spec_runtime;
pub mod spec_versions;
pub mod specs;
pub mod task_deps;
pub mod task_runtime;

// --- Connection + error (db) ---
pub use db::{RepoError, connect};

// --- ID generation (ids + the per-table allocators) ---
// `allocate_task_id` / `allocate_phase_run_id` / `allocate_decision_id` live in
// their owning table modules — each needs that table's column set (the Task 3.3
// asymmetry) — but are re-exported here alongside `ids::*` so the allocator
// surface is in one place, as the plan's Task 3.12 `pub use ids::{...}` intends.
pub use decisions::allocate_decision_id;
pub use ids::{allocate_id, allocate_spec_id, random_id};
pub use phase_runs::allocate_phase_run_id;
pub use task_runtime::allocate_task_id;

// --- Row structs + typed helpers (non-colliding — safe to flatten) ---
pub use clean::{CleanReport, clean_phase_runs_older_than, clean_spec, clean_spec_forced};
pub use composition::{ComposedContext, compose_for_phase};
pub use decisions::{fetch_by_id, fetch_by_spec};
pub use dispatch::{DispatchDep, DispatchRows, DispatchTask, insert_dispatch};
pub use phase_runs::{PhaseRunRow, SpecPhaseMetrics, aggregate_metrics_for_spec};
pub use spec_runtime::{SpecIterationCounter, SpecRuntimeRow, TerminalReason};
pub use spec_versions::{SNAPSHOT_VERSION, VersionTrigger, append_version, fetch_snapshot};
pub use specs::{exists, insert_spec};
pub use task_deps::{add_dep, dependents_of, deps_of, remove_dep};
pub use task_runtime::{IterationCounter, TaskRuntimeRow};
// Colliding table verbs — call path-qualified (see the module doc above):
//   spec_runtime::{initialize, update_status, increment_iteration, fetch}
//   task_runtime::{insert_task, update_state, increment_iteration,
//                  reset_iterations, fetch, tasks_for_spec}
//   phase_runs::{insert_start, update_end, record_heartbeat, find_abandoned,
//                close_orphaned, fetch, fetch_latest_open_for_task,
//                fetch_history_for_spec}
//   decisions::insert
