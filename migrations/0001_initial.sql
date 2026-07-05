-- BOI v2 initial schema — design §3.0, all 7 tables.
--
-- Forward-only. `sqlx::migrate!` never invokes down migrations; a `.down.sql`
-- would give false-confidence test coverage (Batch A review — L2). To change
-- the schema, add `0002_*.sql`, never edit this file.
--
-- ID CHECK constraints: SQLite GLOB has no `{n}` quantifier, so the
-- Crockford-base32 alphabet (lowercase, no confusables i/l/o/u) is written as
-- the GLOB class `[0-9a-hjkmnp-tv-z]` repeated 8× after the type prefix. The
-- design's `[0-9]{8}` shorthand is NOT valid GLOB grammar; the plan's
-- Task 3.1 erratum corrects it to the expanded form below.
--
-- All FKs are ON DELETE RESTRICT (design §11): `boi clean` cascades manually
-- in dependency order rather than relying on cascade deletes.

-- IMMUTABLE IDENTITY: one row per spec, never pruned by `boi clean` (audit
-- identity). FK anchor for every other table.
CREATE TABLE specs (
  spec_id    TEXT PRIMARY KEY
             CHECK(spec_id GLOB 'S[0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z]'),
  created_at TIMESTAMP NOT NULL
);

-- IMMUTABLE: every author-level change appends a new version. INSERT only.
-- `snapshot` JSON carries a top-level `snapshot_v: INTEGER` for cross-version
-- replay (B11). No `parent_version` column (S11).
CREATE TABLE spec_versions (
  spec_id      TEXT NOT NULL REFERENCES specs(spec_id) ON DELETE RESTRICT,
  version      INTEGER NOT NULL,
  snapshot     JSON NOT NULL,
  trigger      TEXT NOT NULL,   -- 'dispatch' | 'plan_revised'
  trigger_meta JSON,
  created_at   TIMESTAMP NOT NULL,
  PRIMARY KEY (spec_id, version)
);

-- MUTABLE: current execution state per spec. Mutated only via the event bus.
CREATE TABLE spec_runtime (
  spec_id             TEXT PRIMARY KEY REFERENCES specs(spec_id) ON DELETE RESTRICT,
  current_version     INTEGER NOT NULL,
  status              TEXT NOT NULL,    -- queued | running | completed | failed | canceled
  failure_reason      JSON,             -- FailureReason if failed
  cancellation_reason JSON,             -- CancellationReason if canceled
  started_at          TIMESTAMP,
  completed_at        TIMESTAMP,
  FOREIGN KEY (spec_id, current_version) REFERENCES spec_versions(spec_id, version),
  -- Status/reason mutex: exactly one of failure_reason/cancellation_reason per
  -- terminal status (B8).
  CHECK (
    (status = 'failed'   AND failure_reason      IS NOT NULL AND cancellation_reason IS NULL) OR
    (status = 'canceled' AND cancellation_reason IS NOT NULL AND failure_reason      IS NULL) OR
    (status IN ('queued','running','completed')
                         AND failure_reason IS NULL AND cancellation_reason IS NULL)
  )
);

-- MUTABLE: current execution state per task. Four typed iteration counters
-- replace v1's `iterations_used` JSON blob (C6). `blocked_by_task_ids` JSON is
-- gone — task dependency edges live in the `task_deps` table (Batch A review
-- L2). `materialized_commands`/`last_verify_results` removed per S7.
CREATE TABLE task_runtime (
  task_id             TEXT PRIMARY KEY
                      CHECK(task_id GLOB 'T[0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z]'),
  spec_id             TEXT NOT NULL REFERENCES specs(spec_id) ON DELETE RESTRICT,
  ref                 TEXT,
  state               TEXT NOT NULL,    -- not_started | active | blocked | passing | canceled
  -- `reason` stores either a TerminalReason (terminal states) or a
  -- BlockedReason (blocked state) — folded G16.7. `boi status` reads the
  -- persisted BlockedReason directly rather than reconstructing from OTel.
  blocked_reason      JSON,
  cancellation_reason JSON,
  current_phase       TEXT,
  evidence            JSON,
  iterations_plan_critique  INTEGER NOT NULL DEFAULT 0,
  iterations_task_adjust    INTEGER NOT NULL DEFAULT 0,
  iterations_execute_review INTEGER NOT NULL DEFAULT 0,
  iterations_spec_review    INTEGER NOT NULL DEFAULT 0,
  worktree_path       TEXT,
  branch_ref          TEXT,
  started_at          TIMESTAMP,
  completed_at        TIMESTAMP
);
CREATE INDEX idx_task_runtime_spec ON task_runtime(spec_id);

-- Task dependency DAG edges (Batch A review L2 — promoted from
-- `task_runtime.blocked_by_task_ids` JSON to a real table). FK on BOTH columns
-- so a plan revision cannot write a dangling dep. Phase 5a's scheduler indexes
-- this instead of JSON-parsing every tick.
CREATE TABLE task_deps (
  task_id    TEXT NOT NULL REFERENCES task_runtime(task_id) ON DELETE RESTRICT,
  depends_on TEXT NOT NULL REFERENCES task_runtime(task_id) ON DELETE RESTRICT,
  PRIMARY KEY (task_id, depends_on)
);
-- Reverse-direction lookup for `dependents_of` (the forward direction is the PK).
CREATE INDEX idx_task_deps_depends_on ON task_deps(depends_on);

-- Execution record. One row per phase execution. Two-phase: INSERT at
-- PhaseStarted (completed_at = NULL), UPDATE at PhaseCompleted (synopsis,
-- verdict, files_touched, completed_at). No DELETE. `outcome` column removed —
-- filter via json_extract(verdict,'$.type') if needed (S1). Authored decisions
-- use phase_run_id = NULL — no synthetic dispatch phase_run is created (C7).
CREATE TABLE phase_runs (
  id                TEXT PRIMARY KEY
                    CHECK(id GLOB 'P[0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z]'),
  spec_id           TEXT NOT NULL REFERENCES specs(spec_id) ON DELETE RESTRICT,
  task_id           TEXT NULL REFERENCES task_runtime(task_id) ON DELETE RESTRICT,
  phase             TEXT NOT NULL,
  phase_iteration   INTEGER NOT NULL,
  spec_version      INTEGER NOT NULL,   -- which authored intent this ran against (B3)
  provider          TEXT NOT NULL,      -- 'claude_code' | 'openrouter' | 'human'
  worker_id         TEXT NULL,
  files_touched     JSON NOT NULL DEFAULT '[]',
  synopsis          TEXT NOT NULL DEFAULT '',
  verdict           JSON NULL,
  last_heartbeat_at TIMESTAMP NULL,     -- worker pings every 30s; sweeper detects abandonment (B7)
  started_at        TIMESTAMP NOT NULL,
  completed_at      TIMESTAMP NULL,     -- NULL while in-progress; UPDATE at phase end
  FOREIGN KEY (spec_id, spec_version) REFERENCES spec_versions(spec_id, version),
  UNIQUE(spec_id, task_id, phase, phase_iteration)  -- crash-recovery dedup (B3)
);
CREATE INDEX idx_phase_runs_spec ON phase_runs(spec_id, started_at);
CREATE INDEX idx_phase_runs_task ON phase_runs(task_id, started_at) WHERE task_id IS NOT NULL;

-- Decisions. Workers EMIT via the `decision.record(...)` MCP tool; they never
-- read this table. Append-only — new decisions can `supersede` prior ones, but
-- rows never UPDATE or DELETE. origin='authored' => phase_run_id IS NULL;
-- origin IN ('runtime','human') => phase_run_id IS NOT NULL (C7).
CREATE TABLE decisions (
  id           TEXT PRIMARY KEY
               CHECK(id GLOB 'D[0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z][0-9a-hjkmnp-tv-z]'),
  spec_id      TEXT NOT NULL REFERENCES specs(spec_id) ON DELETE RESTRICT,   -- denormalized (C7)
  phase_run_id TEXT NULL REFERENCES phase_runs(id) ON DELETE RESTRICT,       -- NULL for authored (C7)
  origin       TEXT NOT NULL CHECK(origin IN ('authored','runtime','human')),
  title        TEXT NOT NULL,
  summary      TEXT NOT NULL,
  rationale    TEXT NOT NULL,
  alternatives JSON NOT NULL,         -- [{name, reason}]
  supersedes   TEXT NULL REFERENCES decisions(id) ON DELETE RESTRICT,
  created_at   TIMESTAMP NOT NULL,
  -- origin/phase_run_id mutex.
  CHECK (
    (origin = 'authored' AND phase_run_id IS NULL) OR
    (origin IN ('runtime','human') AND phase_run_id IS NOT NULL)
  )
);
CREATE INDEX idx_decisions_phase_run ON decisions(phase_run_id) WHERE phase_run_id IS NOT NULL;
CREATE INDEX idx_decisions_spec ON decisions(spec_id, created_at);
-- Only one decision can supersede a given prior decision (B9).
CREATE UNIQUE INDEX idx_decisions_supersedes ON decisions(supersedes)
  WHERE supersedes IS NOT NULL;
