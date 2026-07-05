# AGENTS.md — repo (LDA layer 2, persistence)

SQLite persistence for BOI's 7 tables. Depends only on `types` (+ `config`).
**The ONLY layer allowed to use `sqlx::query!` macros.**

- Enter at `mod.rs` — the layer `//!` (tables, boundary, re-export strategy).
- One table per file: `specs.rs`, `spec_versions.rs`, `spec_runtime.rs`,
  `task_runtime.rs`, `task_deps.rs`, `phase_runs.rs`, `decisions.rs`.
- Cross-cutting: `db.rs` (pool + migrations), `ids.rs`, `composition.rs` (§7.2 query),
  `dispatch.rs` (structural insert), `clean.rs` (`boi clean` cascade).
- Invariants: **migrations are append-only** (never edit an applied `migrations/NNNN_*.sql`;
  add a new numbered file — `sqlx::migrate!` tracks versions, no down migration); pragmas
  set in `db.rs` are `journal_mode=WAL` + `foreign_keys=ON`.
- Changing a `sqlx::query!` recompiles against the live schema → regenerate the offline
  cache: `just prep-sqlx`, commit the `.sqlx/` delta with the change.
