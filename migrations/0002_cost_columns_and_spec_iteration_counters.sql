-- BOI v2 schema amendment 0002 — cost/token columns + spec-level iteration
-- counters.
--
-- Forward-only (see 0001's header). Two unrelated data-layer gaps closed in
-- one migration, both surfaced by the Phase 8b execution flags + the code
-- review:
--
--   G25.1 — `phase_runs` had no cost/token columns, so the values
--           `BoiEvent::PhaseCompleted` carries (`cost_usd`/`tokens_in`/
--           `tokens_out`) were dropped on the floor by the bus's persist arm,
--           leaving Phase 8b's §10 `metrics` block shipping zeros.
--
--   G21.1 — the spec-level iteration caps (`CAP_PLAN_CRITIQUE`,
--           `CAP_SPEC_REVIEW`) were unenforceable: their counters lived only
--           on `task_runtime`, but `plan`/`critique_plan` and the spec-level
--           `review` are spec-level phases with no task row. The counters
--           belong on `spec_runtime`.

-- G25.1: per-phase cost/token columns. Nullable — a phase run that has not
-- completed (or a deterministic phase with no LLM cost) leaves them NULL;
-- `phase_runs::update_end` writes them at PhaseCompleted from the event.
ALTER TABLE phase_runs ADD COLUMN tokens_in  INTEGER;
ALTER TABLE phase_runs ADD COLUMN tokens_out INTEGER;
ALTER TABLE phase_runs ADD COLUMN cost_usd   REAL;

-- G21.1: spec-level iteration counters. NOT NULL DEFAULT 0 — every existing
-- and future `spec_runtime` row starts both at zero; `spec_runtime`'s
-- increment functions bump them, mirroring `task_runtime`'s counters.
ALTER TABLE spec_runtime ADD COLUMN iterations_plan_critique INTEGER NOT NULL DEFAULT 0;
ALTER TABLE spec_runtime ADD COLUMN iterations_spec_review   INTEGER NOT NULL DEFAULT 0;
