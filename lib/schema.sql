-- schema.sql — SQLite schema for BOI queue and state management.
--
-- Replaces the file-based JSON queue with a single SQLite database.
-- All mutable state lives here. WAL mode enables concurrent reads
-- (boi status) without blocking writes (daemon).

-- Specs table: one row per dispatched spec.
CREATE TABLE IF NOT EXISTS specs (
    id TEXT PRIMARY KEY,                    -- q-001
    spec_path TEXT NOT NULL,                -- Queue copy path
    original_spec_path TEXT,
    worktree TEXT,
    priority INTEGER NOT NULL DEFAULT 100,
    status TEXT NOT NULL,                   -- queued|assigning|running|completed|failed|canceled|needs_review|requeued
    phase TEXT DEFAULT 'execute',           -- execute|critic|evaluate|decompose
    submitted_at TEXT NOT NULL,
    first_running_at TEXT,
    last_iteration_at TEXT,
    last_worker TEXT,
    iteration INTEGER NOT NULL DEFAULT 0,
    max_iterations INTEGER NOT NULL DEFAULT 30,
    consecutive_failures INTEGER DEFAULT 0,
    cooldown_until TEXT,
    tasks_done INTEGER DEFAULT 0,
    tasks_total INTEGER DEFAULT 0,
    sync_back INTEGER DEFAULT 1,
    project TEXT,
    initial_task_ids TEXT,                  -- JSON array
    worker_timeout_seconds INTEGER,
    failure_reason TEXT,
    needs_review_since TEXT,
    assigning_at TEXT,                      -- Timestamp when status set to 'assigning' (for stuck recovery)
    critic_passes INTEGER DEFAULT 0,       -- Number of critic passes run
    pre_iteration_tasks TEXT,              -- JSON object
    experiment_tasks TEXT,                 -- JSON array of experiment task IDs
    max_experiment_invocations INTEGER DEFAULT 0,
    experiment_invocations_used INTEGER DEFAULT 0,
    decomposition_retries INTEGER DEFAULT 0,
    CHECK (status IN ('queued','assigning','running','completed','failed','canceled','needs_review','requeued')),
    CHECK (phase IN ('execute','critic','evaluate','decompose'))
);

-- Spec dependency DAG: blocks_on must complete before spec_id can run.
CREATE TABLE IF NOT EXISTS spec_dependencies (
    spec_id TEXT NOT NULL,
    blocks_on TEXT NOT NULL,
    PRIMARY KEY (spec_id, blocks_on),
    FOREIGN KEY (spec_id) REFERENCES specs(id) ON DELETE CASCADE,
    FOREIGN KEY (blocks_on) REFERENCES specs(id) ON DELETE CASCADE
);

-- Workers table: one row per configured worker slot.
CREATE TABLE IF NOT EXISTS workers (
    id TEXT PRIMARY KEY,
    worktree_path TEXT NOT NULL,
    current_spec_id TEXT,
    current_pid INTEGER,
    start_time TEXT,
    current_phase TEXT,                    -- Phase being executed (for crash recovery)
    FOREIGN KEY (current_spec_id) REFERENCES specs(id) ON DELETE SET NULL
);

-- Process tracking: which PID ran which spec/iteration/phase.
CREATE TABLE IF NOT EXISTS processes (
    pid INTEGER NOT NULL,
    spec_id TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    iteration INTEGER NOT NULL,
    phase TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    exit_code INTEGER,
    PRIMARY KEY (pid, spec_id, iteration, phase)
);

-- Iteration metadata: one row per (spec, iteration, phase) execution.
CREATE TABLE IF NOT EXISTS iterations (
    spec_id TEXT NOT NULL,
    iteration INTEGER NOT NULL,
    phase TEXT NOT NULL DEFAULT 'execute',
    worker_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT NOT NULL,
    duration_seconds INTEGER NOT NULL,
    tasks_completed INTEGER DEFAULT 0,
    tasks_added INTEGER DEFAULT 0,
    tasks_skipped INTEGER DEFAULT 0,
    exit_code INTEGER,
    pre_pending INTEGER,
    post_pending INTEGER,
    quality_score REAL,
    quality_breakdown TEXT,
    PRIMARY KEY (spec_id, iteration, phase)
);

-- Append-only event log.
CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    spec_id TEXT,
    event_type TEXT NOT NULL,
    message TEXT,
    data TEXT,
    level TEXT DEFAULT 'info'
);

-- Performance indexes.
CREATE INDEX IF NOT EXISTS idx_specs_last_worker ON specs(last_worker);
CREATE INDEX IF NOT EXISTS idx_events_spec_id ON events(spec_id);
CREATE INDEX IF NOT EXISTS idx_iterations_spec_id ON iterations(spec_id);
