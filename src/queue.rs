use chrono::Utc;
use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension, Result};

pub struct Queue {
    conn: Connection,
}

#[derive(Debug)]
pub struct SpecRecord {
    pub id: String,
    pub title: String,
    pub mode: String,
    pub status: String,
    pub spec_path: Option<String>,
    pub total_tasks: Option<i64>,
    pub completed_tasks: i64,
    pub priority: i64,
    pub depends_on: Option<String>,
    pub queued_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub worker_id: Option<String>,
    pub error: Option<String>,
    pub max_iterations: i64,
    pub iteration: i64,
    pub project: Option<String>,
    pub phase: String,
    pub worker_timeout_seconds: Option<i64>,
    pub context: Option<String>,
    pub workspace: Option<String>,
    /// Number of completed critique↔improve loop cycles for this spec.
    pub phase_loop_count: i64,
    pub project_context: Option<String>,
    /// JSON-serialized Vec<String> of task-level phase names (from spec YAML).
    pub task_phases: Option<String>,
    /// JSON-serialized Vec<String> of spec-level phase names (from spec YAML).
    pub spec_phases: Option<String>,
    /// JSON-serialized HashMap<phase_name, PhaseOverride> (from spec YAML).
    pub phase_overrides: Option<String>,
    /// Named worker pool requested by this spec (None → use registry default).
    pub worker_pool: Option<String>,
}

#[derive(Debug)]
pub struct FullTaskRecord {
    pub id: String,
    pub spec_id: String,
    pub title: String,
    pub status: String,
    pub depends: String,
    pub spec_content: Option<String>,
    pub verify_content: Option<String>,
}

#[derive(Debug)]
pub struct IterationRecord {
    pub spec_id: String,
    pub iteration: i64,
    pub phase: Option<String>,
    pub worker_id: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub duration_seconds: Option<f64>,
    pub tasks_completed: i64,
    pub tasks_added: i64,
    pub exit_code: Option<i64>,
}

#[derive(Debug)]
pub struct WorkerRecord {
    pub id: String,
    pub worktree_path: Option<String>,
    pub current_spec_id: Option<String>,
    pub current_pid: Option<i64>,
    pub start_time: Option<String>,
    pub current_phase: Option<String>,
    pub current_task_id: Option<String>,
}

#[derive(Debug)]
pub struct PhaseRunRecord {
    pub spec_id: String,
    pub task_id: Option<String>,
    pub phase: String,
    pub level: String,
    pub outcome: String,
    pub duration_ms: Option<i64>,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub model: Option<String>,
    pub runtime: Option<String>,
    pub pipeline_id: Option<String>,
    pub attempt: i64,
    pub failure_mode: Option<String>,
    pub cold_start_ms: Option<i64>,
    pub inference_ms: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub tool_call_count: Option<i64>,
    pub tool_calls_by_type: Option<String>,
    pub ttft_ms: Option<i64>,
    pub loop_iteration: Option<i64>,
    pub verify_exit_code: Option<i64>,
    pub context_json: Option<String>,
}

#[derive(Debug)]
pub struct PhaseCostSummary {
    pub phase: String,
    pub total_cost: f64,
    pub total_duration_ms: i64,
    pub count: i64,
}

#[derive(Debug)]
pub struct TaskRecord {
    pub id: String,
    pub spec_id: String,
    pub title: String,
    pub status: String,
    pub depends: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub error: Option<String>,
}

pub struct SpecStatus {
    pub spec: SpecRecord,
    pub tasks: Vec<TaskRecord>,
}

#[derive(Debug)]
pub struct BenchResultRecord {
    pub run_id: String,
    pub pipeline: String,
    pub spec_file: String,
    pub run_number: i64,
    pub status: String,
    pub total_ms: i64,
    pub tasks_total: i64,
    pub tasks_done: i64,
    pub tasks_failed: i64,
    pub total_cost_usd: Option<f64>,
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub tasks_skipped: i64,
}

fn gen_id(prefix: char, conn: &Connection) -> String {
    let mut rng = rand::thread_rng();
    let table = if prefix == 'S' { "specs" } else { "tasks" };
    for _ in 0..100 {
        let bytes: [u8; 2] = rng.gen();
        let candidate = format!("{}{:02X}{:02X}", prefix, bytes[0], bytes[1]);
        let exists: bool = conn
            .query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {} WHERE id = ?1)", table),
                rusqlite::params![candidate],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !exists {
            return candidate;
        }
    }
    let bytes: [u8; 4] = rng.gen();
    format!("{}{:02X}{:02X}{:02X}{:02X}", prefix, bytes[0], bytes[1], bytes[2], bytes[3])
}

impl Queue {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;
            PRAGMA busy_timeout=5000;

            CREATE TABLE IF NOT EXISTS specs (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                mode TEXT NOT NULL DEFAULT 'execute',
                status TEXT NOT NULL DEFAULT 'queued',
                spec_path TEXT,
                total_tasks INTEGER,
                completed_tasks INTEGER DEFAULT 0,
                priority INTEGER DEFAULT 100,
                depends_on TEXT,
                queued_at TEXT NOT NULL,
                started_at TEXT,
                completed_at TEXT,
                worker_id TEXT,
                error TEXT,
                max_iterations INTEGER DEFAULT 30,
                iteration INTEGER DEFAULT 0,
                project TEXT,
                phase TEXT DEFAULT 'execute',
                project_context TEXT
            );

            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT NOT NULL,
                spec_id TEXT NOT NULL REFERENCES specs(id),
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'PENDING',
                depends TEXT DEFAULT '[]',
                started_at TEXT,
                completed_at TEXT,
                error TEXT,
                spec_content TEXT,
                verify_content TEXT,
                PRIMARY KEY (spec_id, id)
            );

            CREATE TABLE IF NOT EXISTS iterations (
                spec_id TEXT NOT NULL,
                iteration INTEGER NOT NULL,
                phase TEXT,
                worker_id TEXT,
                started_at TEXT,
                ended_at TEXT,
                duration_seconds REAL,
                tasks_completed INTEGER DEFAULT 0,
                tasks_added INTEGER DEFAULT 0,
                exit_code INTEGER,
                PRIMARY KEY (spec_id, iteration)
            );

            CREATE TABLE IF NOT EXISTS events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                spec_id TEXT,
                event_type TEXT NOT NULL,
                message TEXT,
                data TEXT,
                level TEXT DEFAULT 'info'
            );

            CREATE TABLE IF NOT EXISTS workers (
                id TEXT PRIMARY KEY,
                worktree_path TEXT,
                current_spec_id TEXT,
                current_pid INTEGER,
                start_time TEXT,
                current_phase TEXT,
                current_task_id TEXT
            );

            CREATE TABLE IF NOT EXISTS processes (
                pid INTEGER,
                spec_id TEXT NOT NULL,
                worker_id TEXT,
                iteration INTEGER,
                phase TEXT,
                started_at TEXT,
                ended_at TEXT,
                exit_code INTEGER
            );

            CREATE TABLE IF NOT EXISTS phase_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                spec_id TEXT NOT NULL,
                task_id TEXT,
                phase TEXT NOT NULL,
                level TEXT NOT NULL,
                outcome TEXT NOT NULL,
                duration_ms INTEGER,
                cost_usd REAL,
                input_tokens INTEGER,
                output_tokens INTEGER,
                started_at TEXT NOT NULL,
                completed_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_phase_runs_spec ON phase_runs(spec_id);
            CREATE INDEX IF NOT EXISTS idx_phase_runs_phase ON phase_runs(phase);

            CREATE TABLE IF NOT EXISTS bench_results (
                run_id TEXT,
                pipeline TEXT,
                spec_file TEXT,
                run_number INTEGER,
                status TEXT,
                total_ms INTEGER,
                tasks_total INTEGER,
                tasks_done INTEGER,
                tasks_failed INTEGER
            );

            CREATE TABLE IF NOT EXISTS runners (
                runner_id TEXT PRIMARY KEY,
                secret_key_hash TEXT NOT NULL,
                tags TEXT DEFAULT '[]',
                registered_at TEXT NOT NULL,
                last_seen TEXT
            );",
        )?;

        // Migrate existing specs tables that lack new columns
        Self::ensure_column(&conn, "specs", "max_iterations", "INTEGER DEFAULT 30");
        Self::ensure_column(&conn, "specs", "iteration", "INTEGER DEFAULT 0");
        Self::ensure_column(&conn, "specs", "project", "TEXT");
        Self::ensure_column(&conn, "specs", "phase", "TEXT DEFAULT 'execute'");
        Self::ensure_column(&conn, "specs", "worker_timeout_seconds", "INTEGER");
        Self::ensure_column(&conn, "specs", "context", "TEXT");
        Self::ensure_column(&conn, "specs", "workspace", "TEXT");
        Self::ensure_column(&conn, "specs", "phase_loop_count", "INTEGER DEFAULT 0");
        Self::ensure_column(&conn, "specs", "project_context", "TEXT");
        Self::ensure_column(&conn, "specs", "task_phases", "TEXT");
        Self::ensure_column(&conn, "specs", "spec_phases", "TEXT");
        Self::ensure_column(&conn, "specs", "phase_overrides", "TEXT");
        Self::ensure_column(&conn, "specs", "worker_pool", "TEXT");
        Self::ensure_column(&conn, "specs", "runner_id", "TEXT");
        Self::ensure_column(&conn, "specs", "remote_cost_usd", "REAL");
        Self::ensure_column(&conn, "specs", "remote_duration_secs", "REAL");
        Self::ensure_column(&conn, "tasks", "spec_content", "TEXT");
        Self::ensure_column(&conn, "tasks", "verify_content", "TEXT");

        // Telemetry columns for experiment instrumentation (Gap 1-16, 19)
        Self::ensure_column(&conn, "phase_runs", "model", "TEXT");
        Self::ensure_column(&conn, "phase_runs", "runtime", "TEXT");
        Self::ensure_column(&conn, "phase_runs", "pipeline_id", "TEXT");
        Self::ensure_column(&conn, "phase_runs", "attempt", "INTEGER DEFAULT 1");
        Self::ensure_column(&conn, "phase_runs", "failure_mode", "TEXT");
        Self::ensure_column(&conn, "phase_runs", "cold_start_ms", "INTEGER");
        Self::ensure_column(&conn, "phase_runs", "inference_ms", "INTEGER");
        Self::ensure_column(&conn, "phase_runs", "cache_read_tokens", "INTEGER");
        Self::ensure_column(&conn, "phase_runs", "cache_creation_tokens", "INTEGER");
        Self::ensure_column(&conn, "phase_runs", "tool_call_count", "INTEGER");
        Self::ensure_column(&conn, "phase_runs", "tool_calls_by_type", "TEXT");
        Self::ensure_column(&conn, "phase_runs", "ttft_ms", "INTEGER");
        Self::ensure_column(&conn, "phase_runs", "loop_iteration", "INTEGER");
        Self::ensure_column(&conn, "phase_runs", "verify_exit_code", "INTEGER");
        Self::ensure_column(&conn, "phase_runs", "context_json", "TEXT");

        Self::ensure_column(&conn, "bench_results", "total_cost_usd", "REAL");
        Self::ensure_column(&conn, "bench_results", "total_input_tokens", "INTEGER");
        Self::ensure_column(&conn, "bench_results", "total_output_tokens", "INTEGER");
        Self::ensure_column(&conn, "bench_results", "tasks_skipped", "INTEGER DEFAULT 0");

        // Phase 3: load-aware dispatch hints
        Self::ensure_column(&conn, "runners", "slots_free", "INTEGER");
        Self::ensure_column(&conn, "runners", "ram_free_mb", "INTEGER");
        // Phase 3: tag matching
        Self::ensure_column(&conn, "specs", "required_tags", "TEXT DEFAULT '[]'");

        Ok(Queue { conn })
    }

    fn ensure_column(conn: &Connection, table: &str, column: &str, col_type: &str) {
        // Check if column exists by querying table_info
        let has_col: bool = conn
            .prepare(&format!("PRAGMA table_info({})", table))
            .and_then(|mut stmt| {
                let rows = stmt.query_map([], |row| {
                    let name: String = row.get(1)?;
                    Ok(name)
                })?;
                Ok(rows.filter_map(|r| r.ok()).any(|n| n == column))
            })
            .unwrap_or(false);

        if !has_col {
            let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, col_type);
            if let Err(e) = conn.execute(&sql, []) {
                eprintln!("[boi] ERROR: failed to add column {}.{}: {}", table, column, e);
            }
        }
    }

    pub fn enqueue(
        &self,
        spec: &crate::spec::BoiSpec,
        spec_path: Option<&str>,
    ) -> Result<String> {
        self.enqueue_with_context(spec, spec_path, None)
    }

    /// Enqueue a spec with optional pre-computed project context content.
    /// The project_context is stored in the DB and injected into worker prompts
    /// via the {{PROJECT_CONTEXT}} template variable.
    pub fn enqueue_with_context(
        &self,
        spec: &crate::spec::BoiSpec,
        spec_path: Option<&str>,
        project_context: Option<&str>,
    ) -> Result<String> {
        // Intake validation: reject specs with pre-DONE or non-PENDING tasks before
        // acquiring a write lock or touching the DB (eliminates spec_validation failure class).
        if let Err(reason) = crate::spec::validate_intake(spec) {
            eprintln!(
                "[boi] ERROR: intake-reject: {} — {}",
                spec_path.unwrap_or("<unknown>"),
                reason
            );
            return Err(rusqlite::Error::InvalidParameterName(reason));
        }

        // BEGIN IMMEDIATE acquires a write lock upfront so the dedup SELECT and the
        // INSERT are atomic — no TOCTOU window even with concurrent boi dispatch calls.
        self.conn.execute("BEGIN IMMEDIATE", [])?;

        // Pre-dispatch dedup: skip if the same spec_path is already queued/running.
        // This prevents the double-dispatch defect (S8552+S05C5 incident: same spec
        // dispatched 26s apart, both ran 64+ minutes concurrently at 2x LLM cost).
        if let Some(path) = spec_path {
            let existing: Option<String> = self.conn.query_row(
                "SELECT id FROM specs WHERE spec_path = ?1 AND status IN ('queued', 'running') LIMIT 1",
                [path],
                |row| row.get::<_, String>(0),
            ).optional()?;
            if let Some(existing_id) = existing {
                self.conn.execute("ROLLBACK", [])?;
                eprintln!("[boi] INFO: dedup-skip: spec_path={} already queued/running as {}", path, existing_id);
                return Ok(existing_id);
            }
        }

        let id = gen_id('S', &self.conn);

        let now = Utc::now().to_rfc3339();
        let mode = spec.mode.as_deref().unwrap_or("execute");
        let total = spec.tasks.len() as i64;

        let task_phases_json: Option<String> = spec.task_phases.as_ref()
            .filter(|v| !v.is_empty())
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let spec_phases_json: Option<String> = spec.spec_phases.as_ref()
            .filter(|v| !v.is_empty())
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let phase_overrides_json: Option<String> = if spec.phase_overrides.is_empty() {
            None
        } else {
            serde_json::to_string(&spec.phase_overrides).ok()
        };

        if let Err(e) = self.conn.execute(
            "INSERT INTO specs (id, title, mode, status, spec_path, total_tasks, queued_at, context, workspace, project_context, task_phases, spec_phases, phase_overrides, worker_pool)
             VALUES (?1, ?2, ?3, 'queued', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![id, spec.title, mode, spec_path, total, now, spec.context, spec.workspace, project_context, task_phases_json, spec_phases_json, phase_overrides_json, spec.worker_pool],
        ) {
            let _ = self.conn.execute("ROLLBACK", []);
            return Err(e);
        }

        let mut yaml_to_canonical: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for task in spec.tasks.iter() {
            let canonical_task_id = gen_id('T', &self.conn);
            yaml_to_canonical.insert(task.id.clone(), canonical_task_id.clone());
        }

        for task in spec.tasks.iter() {
            let canonical_task_id = &yaml_to_canonical[&task.id];
            let canonical_deps: Vec<String> = task.depends.as_deref().unwrap_or(&[])
                .iter()
                .map(|d| yaml_to_canonical.get(d).cloned().unwrap_or_else(|| d.clone()))
                .collect();
            let depends_json = serde_json::to_string(&canonical_deps)
                .unwrap_or_else(|_| "[]".to_string());
            if let Err(e) = self.conn.execute(
                "INSERT INTO tasks (id, spec_id, title, status, depends, spec_content, verify_content)
                 VALUES (?1, ?2, ?3, 'PENDING', ?4, ?5, ?6)",
                params![canonical_task_id, id, task.title, depends_json, task.spec, task.verify],
            ) {
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(e);
            }
        }

        self.conn.execute("COMMIT", [])?;
        Ok(id)
    }

    /// Returns the highest-priority queued spec whose depends_on (if any) is completed.
    /// Atomically sets the spec status to 'assigning' to prevent double-dispatch.
    pub fn dequeue(&self) -> Result<Option<SpecRecord>> {
        let tx = self.conn.unchecked_transaction()?;

        let maybe_id: Option<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM specs
                 WHERE status = 'queued'
                   AND (depends_on IS NULL OR depends_on = ''
                        OR EXISTS (SELECT 1 FROM specs s2
                                   WHERE s2.id = specs.depends_on AND s2.status = 'completed'))
                 ORDER BY priority ASC, queued_at ASC
                 LIMIT 1",
            )?;
            match stmt.query_row([], |row| row.get::<_, String>(0)) {
                Ok(id) => Some(id),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => return Err(e),
            }
        };

        let id = match maybe_id {
            Some(id) => id,
            None => return Ok(None),
        };

        tx.execute(
            "UPDATE specs SET status = 'assigning' WHERE id = ?1",
            params![id],
        )?;

        let rec = {
            let mut stmt = tx.prepare(
                "SELECT id, title, mode, status, spec_path,
                        (SELECT COUNT(*) FROM tasks WHERE tasks.spec_id = specs.id) as total_tasks,
                        completed_tasks,
                        priority, depends_on, queued_at, started_at, completed_at, worker_id, error,
                        max_iterations, iteration, project, phase, worker_timeout_seconds,
                        context, workspace, phase_loop_count, project_context,
                        task_phases, spec_phases, phase_overrides, worker_pool
                 FROM specs WHERE id = ?1",
            )?;
            stmt.query_row(params![id], row_to_spec)?
        };

        tx.commit()?;
        Ok(Some(rec))
    }

    /// Returns true if every tag in `required_tags_json` (a JSON array) is present
    /// in `runner_tags_json` (also a JSON array).  An empty required list always matches.
    pub fn tags_match(runner_tags_json: &str, required_tags_json: &str) -> bool {
        let runner: Vec<String> = serde_json::from_str(runner_tags_json).unwrap_or_default();
        let required: Vec<String> = serde_json::from_str(required_tags_json).unwrap_or_default();
        required.iter().all(|tag| runner.contains(tag))
    }

    /// Like `dequeue()` but skips specs whose `required_tags` are not a subset of
    /// `runner_tags_json`.  Returns the highest-priority eligible spec, or None.
    pub fn dequeue_filtered(&self, runner_tags_json: &str) -> Result<Option<SpecRecord>> {
        let tx = self.conn.unchecked_transaction()?;

        let candidates: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, COALESCE(required_tags, '[]') FROM specs
                 WHERE status = 'queued'
                   AND (depends_on IS NULL OR depends_on = ''
                        OR EXISTS (SELECT 1 FROM specs s2
                                   WHERE s2.id = specs.depends_on AND s2.status = 'completed'))
                 ORDER BY priority ASC, queued_at ASC",
            )?;
            let mapped = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            });
            match mapped {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(e) => return Err(e),
            }
        };

        let id = match candidates
            .into_iter()
            .find(|(_, req_tags)| Self::tags_match(runner_tags_json, req_tags))
        {
            Some((id, _)) => id,
            None => return Ok(None),
        };

        tx.execute(
            "UPDATE specs SET status = 'assigning' WHERE id = ?1",
            params![id],
        )?;

        let rec = {
            let mut stmt = tx.prepare(
                "SELECT id, title, mode, status, spec_path,
                        (SELECT COUNT(*) FROM tasks WHERE tasks.spec_id = specs.id) as total_tasks,
                        completed_tasks,
                        priority, depends_on, queued_at, started_at, completed_at, worker_id, error,
                        max_iterations, iteration, project, phase, worker_timeout_seconds,
                        context, workspace, phase_loop_count, project_context,
                        task_phases, spec_phases, phase_overrides, worker_pool
                 FROM specs WHERE id = ?1",
            )?;
            stmt.query_row(params![id], row_to_spec)?
        };

        tx.commit()?;
        Ok(Some(rec))
    }

    /// Revert a spec from 'assigning' back to 'queued'.
    /// Called when a named pool is at capacity — the spec waits its turn without
    /// silently falling back to a different pool.
    pub fn requeue(&self, spec_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE specs SET status = 'queued' WHERE id = ?1 AND status = 'assigning'",
            params![spec_id],
        )?;
        Ok(())
    }

    /// Like `dequeue()` but only returns a spec whose `worker_pool` is in
    /// `available_pools`, or whose `worker_pool` is NULL and the default pool
    /// is in `available_pools`.  Atomically sets status to 'assigning'.
    ///
    /// Pass the names of every pool that currently has free slots.
    pub fn dequeue_for_pools(&self, available_pools: &[&str], default_pool: &str) -> Result<Option<SpecRecord>> {
        if available_pools.is_empty() {
            return Ok(None);
        }

        let tx = self.conn.unchecked_transaction()?;

        // Build a parameterized IN clause.  SQLite doesn't support binding a
        // variable-length list as a single parameter, so we construct the
        // placeholders dynamically.
        let placeholders: String = available_pools.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let default_in_available = available_pools.contains(&default_pool);

        let sql = format!(
            "SELECT id FROM specs
             WHERE status = 'queued'
               AND (depends_on IS NULL OR depends_on = ''
                    OR EXISTS (SELECT 1 FROM specs s2
                               WHERE s2.id = specs.depends_on AND s2.status = 'completed'))
               AND (
                 (worker_pool IN ({placeholders}))
                 OR (worker_pool IS NULL AND {default_available})
               )
             ORDER BY priority ASC, queued_at ASC
             LIMIT 1",
            placeholders = placeholders,
            default_available = if default_in_available { "1" } else { "0" },
        );

        let maybe_id: Option<String> = {
            let mut stmt = tx.prepare(&sql)?;
            let pool_params: Vec<&dyn rusqlite::ToSql> = available_pools
                .iter()
                .map(|p| p as &dyn rusqlite::ToSql)
                .collect();
            match stmt.query_row(pool_params.as_slice(), |row| row.get::<_, String>(0)) {
                Ok(id) => Some(id),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => return Err(e),
            }
        };

        let id = match maybe_id {
            Some(id) => id,
            None => return Ok(None),
        };

        tx.execute(
            "UPDATE specs SET status = 'assigning' WHERE id = ?1",
            params![id],
        )?;

        let rec = {
            let mut stmt = tx.prepare(
                "SELECT id, title, mode, status, spec_path,
                        (SELECT COUNT(*) FROM tasks WHERE tasks.spec_id = specs.id) as total_tasks,
                        completed_tasks,
                        priority, depends_on, queued_at, started_at, completed_at, worker_id, error,
                        max_iterations, iteration, project, phase, worker_timeout_seconds,
                        context, workspace, phase_loop_count, project_context,
                        task_phases, spec_phases, phase_overrides, worker_pool
                 FROM specs WHERE id = ?1",
            )?;
            stmt.query_row(params![id], row_to_spec)?
        };

        tx.commit()?;
        Ok(Some(rec))
    }

    pub fn update_task(&self, spec_id: &str, task_id: &str, status: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        match status {
            "DONE" | "SKIPPED" => {
                self.conn.execute(
                    "UPDATE tasks SET status = ?1, completed_at = ?2
                     WHERE spec_id = ?3 AND id = ?4",
                    params![status, now, spec_id, task_id],
                )?;
                self.conn.execute(
                    "UPDATE specs SET completed_tasks = completed_tasks + 1 WHERE id = ?1",
                    params![spec_id],
                )?;
            }
            "RUNNING" => {
                self.conn.execute(
                    "UPDATE tasks SET status = ?1, started_at = ?2
                     WHERE spec_id = ?3 AND id = ?4",
                    params![status, now, spec_id, task_id],
                )?;
            }
            _ => {
                self.conn.execute(
                    "UPDATE tasks SET status = ?1 WHERE spec_id = ?2 AND id = ?3",
                    params![status, spec_id, task_id],
                )?;
            }
        }
        Ok(())
    }

    /// Atomically check that a task is still PENDING and claim it as RUNNING.
    /// Returns true if claim succeeded, false if another worker already claimed it.
    /// Uses BEGIN IMMEDIATE to acquire a write lock before the read, preventing the
    /// TOCTOU race where two workers both see PENDING and both mark it RUNNING.
    pub fn try_claim_task(&self, spec_id: &str, task_id: &str) -> Result<bool> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let current_status: Option<String> = self.conn.query_row(
            "SELECT status FROM tasks WHERE spec_id = ?1 AND id = ?2",
            params![spec_id, task_id],
            |row| row.get(0),
        ).optional()?;
        match current_status.as_deref() {
            Some("PENDING") => {
                self.conn.execute(
                    "UPDATE tasks SET status = 'RUNNING', started_at = datetime('now') WHERE spec_id = ?1 AND id = ?2",
                    params![spec_id, task_id],
                )?;
                self.conn.execute("COMMIT", [])?;
                Ok(true)
            }
            _ => {
                self.conn.execute("ROLLBACK", [])?;
                Ok(false)
            }
        }
    }

    /// Mark a spec as running and record the worker identity that owns it.
    /// Use this instead of `update_spec(id, "running")` so ghost-worker detection
    /// can cross-check worker_id against live processes.
    pub fn update_spec_running(&self, spec_id: &str, worker_id: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE specs SET status = 'running', started_at = ?1, worker_id = ?2 WHERE id = ?3",
            params![now, worker_id, spec_id],
        )?;
        Ok(())
    }

    /// Claim a spec for a remote runner: set status=running + runner_id.
    /// Called by the HTTP API after dequeue() atomically marks it 'assigning'.
    pub fn claim_for_runner(&self, spec_id: &str, runner_id: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE specs SET status = 'running', started_at = ?1, runner_id = ?2 WHERE id = ?3",
            params![now, runner_id, spec_id],
        )?;
        Ok(())
    }

    /// Record completion/failure of a spec from a remote runner.
    pub fn complete_from_runner(
        &self,
        spec_id: &str,
        status: &str,
        cost_usd: Option<f64>,
        duration_secs: Option<f64>,
        error: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE specs SET status = ?1, completed_at = ?2, remote_cost_usd = ?3,
             remote_duration_secs = ?4, error = ?5 WHERE id = ?6",
            params![status, now, cost_usd, duration_secs, error, spec_id],
        )?;
        Ok(())
    }

    /// Insert a spec received from a remote central into the local runner DB,
    /// using the central's spec_id verbatim so worker telemetry correlates correctly.
    pub fn enqueue_with_id(
        &self,
        spec_id: &str,
        spec: &crate::spec::BoiSpec,
        spec_path: Option<&str>,
        workspace_override: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let now = Utc::now().to_rfc3339();
        let mode = spec.mode.as_deref().unwrap_or("execute");
        let total = spec.tasks.len() as i64;
        let workspace = workspace_override.or(spec.workspace.as_deref());
        tx.execute(
            "INSERT OR REPLACE INTO specs (id, title, mode, status, spec_path, total_tasks, queued_at, context, workspace)
             VALUES (?1, ?2, ?3, 'queued', ?4, ?5, ?6, ?7, ?8)",
            params![spec_id, spec.title, mode, spec_path, total, now, spec.context, workspace],
        )?;
        // Guard: clean stale tasks and phase_runs from any prior dispatch of this spec_id.
        // INSERT OR REPLACE on tasks alone leaves orphaned phase_runs that corrupt task
        // history on re-dispatch, causing 0-iteration failures (TPHA3-class bug).
        let stale_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM tasks WHERE spec_id = ?1")?;
            let ids: Vec<String> = stmt
                .query_map(params![spec_id], |r| r.get(0))?
                .filter_map(|r| r.ok())
                .collect();
            ids
        };
        if !stale_ids.is_empty() {
            eprintln!(
                "[boi] WARN: [enqueue_with_id] discarding {} stale task(s) for spec {} before re-dispatch: {:?}",
                stale_ids.len(),
                spec_id,
                stale_ids
            );
            tx.execute("DELETE FROM phase_runs WHERE spec_id = ?1", params![spec_id])?;
            tx.execute("DELETE FROM tasks WHERE spec_id = ?1", params![spec_id])?;
        }
        for task in &spec.tasks {
            let depends_json = task
                .depends
                .as_deref()
                .map(|d| serde_json::to_string(d).unwrap_or_else(|_| "[]".to_string()))
                .unwrap_or_else(|| "[]".to_string());
            tx.execute(
                "INSERT INTO tasks (id, spec_id, title, status, depends, spec_content, verify_content)
                 VALUES (?1, ?2, ?3, 'PENDING', ?4, ?5, ?6)",
                params![task.id, spec_id, task.title, depends_json, task.spec, task.verify],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Mark a spec as merge_failed with an error message.
    pub fn set_merge_failed(&self, spec_id: &str, reason: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE specs SET status = 'merge_failed', error = ?1 WHERE id = ?2",
            params![reason, spec_id],
        )?;
        Ok(())
    }

    /// Get workspace path for a spec (used by API handler for remote merge).
    pub fn get_spec_status(&self, spec_id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT status FROM specs WHERE id = ?1",
                params![spec_id],
                |row| row.get(0),
            )
            .optional()
            .map(|o| o.flatten())
    }

    pub fn get_spec_workspace(&self, spec_id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT workspace FROM specs WHERE id = ?1",
                params![spec_id],
                |row| row.get(0),
            )
            .optional()
            .map(|o| o.flatten())
    }

    /// Get the runner_id that claimed a spec (for fleet event emission).
    pub fn get_spec_runner_id(&self, spec_id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT runner_id FROM specs WHERE id = ?1",
                params![spec_id],
                |row| row.get(0),
            )
            .optional()
            .map(|o| o.flatten())
    }

    /// Get runners whose last_seen is older than threshold_secs seconds.
    /// Returns (runner_id, last_seen) pairs; only includes runners with a known last_seen.
    pub fn get_timed_out_runners(&self, threshold_secs: i64) -> Result<Vec<(String, String)>> {
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(threshold_secs);
        let cutoff_str = cutoff.to_rfc3339();
        let mut stmt = self.conn.prepare(
            "SELECT runner_id, last_seen FROM runners \
             WHERE last_seen IS NOT NULL AND last_seen < ?1",
        )?;
        let rows = stmt.query_map(params![cutoff_str], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }

    /// Get spec IDs currently running on a given runner (used for host_down events).
    pub fn get_running_specs_for_runner(&self, runner_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM specs WHERE status = 'running' AND runner_id = ?1",
        )?;
        let rows = stmt.query_map(params![runner_id], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }

    /// Fleet status: running specs per runner + last 10 completions.
    pub fn fleet_status(&self) -> Result<(Vec<(String, Vec<(String, String)>)>, Vec<(String, String, String, String)>)> {
        // Running specs: group by runner_id
        let mut run_stmt = self.conn.prepare(
            "SELECT COALESCE(runner_id, 'local'), id, COALESCE(title, '')
             FROM specs WHERE status = 'running'
             ORDER BY runner_id, started_at",
        )?;
        let rows = run_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        let mut runner_map: std::collections::HashMap<String, Vec<(String, String)>> =
            std::collections::HashMap::new();
        for row in rows {
            let (rid, sid, title) = row?;
            runner_map.entry(rid).or_default().push((sid, title));
        }
        let mut runners: Vec<(String, Vec<(String, String)>)> =
            runner_map.into_iter().collect();
        runners.sort_by(|a, b| a.0.cmp(&b.0));

        // Last 10 completions
        let mut comp_stmt = self.conn.prepare(
            "SELECT COALESCE(runner_id, 'local'), id, COALESCE(title, ''), status
             FROM specs WHERE status IN ('completed', 'failed', 'merge_failed')
             ORDER BY completed_at DESC LIMIT 10",
        )?;
        let completions: Vec<(String, String, String, String)> = comp_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>>>()?;

        Ok((runners, completions))
    }

    /// Return all registered runners with their last-reported capacity.
    pub fn list_runners(&self) -> Result<Vec<(String, Option<i64>, Option<i64>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT runner_id, slots_free, ram_free_mb FROM runners ORDER BY runner_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>>>()
    }

    /// Register a runner with its HMAC key (stored as hex). Upserts on conflict.
    pub fn register_runner(&self, runner_id: &str, key_hex: &str, tags: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO runners (runner_id, secret_key_hash, tags, registered_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(runner_id) DO UPDATE SET secret_key_hash = ?2, tags = ?3, registered_at = ?4",
            params![runner_id, key_hex, tags, now],
        )?;
        Ok(())
    }

    /// Look up the stored HMAC key for a runner. Returns None if runner is not registered.
    pub fn lookup_runner_key(&self, runner_id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT secret_key_hash FROM runners WHERE runner_id = ?1",
                params![runner_id],
                |row| row.get(0),
            )
            .optional()
    }

    /// Update the last_seen timestamp for a runner.
    pub fn touch_runner(&self, runner_id: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE runners SET last_seen = ?1 WHERE runner_id = ?2",
            params![now, runner_id],
        )?;
        Ok(())
    }

    /// Store the runner's current load hints (slots_free, ram_free_mb) and update last_seen.
    /// These are advisory — used by the central to avoid dispatching to saturated runners.
    pub fn update_runner_capacity(
        &self,
        runner_id: &str,
        slots_free: Option<i64>,
        ram_free_mb: Option<i64>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE runners SET last_seen = ?1, slots_free = ?2, ram_free_mb = ?3 WHERE runner_id = ?4",
            params![now, slots_free, ram_free_mb, runner_id],
        )?;
        Ok(())
    }

    pub fn update_spec(&self, spec_id: &str, status: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        match status {
            "running" => {
                self.conn.execute(
                    "UPDATE specs SET status = ?1, started_at = ?2 WHERE id = ?3",
                    params![status, now, spec_id],
                )?;
            }
            "completed" | "failed" | "cancelled" | "paused" => {
                self.conn.execute(
                    "UPDATE specs SET status = ?1, completed_at = ?2 WHERE id = ?3",
                    params![status, now, spec_id],
                )?;
            }
            _ => {
                self.conn.execute(
                    "UPDATE specs SET status = ?1 WHERE id = ?2",
                    params![status, spec_id],
                )?;
            }
        }
        Ok(())
    }

    pub fn status(&self, spec_id: &str) -> Result<Option<SpecStatus>> {
        let spec = match self.conn.query_row(
            "SELECT id, title, mode, status, spec_path,
                    (SELECT COUNT(*) FROM tasks WHERE tasks.spec_id = specs.id) as total_tasks,
                    completed_tasks,
                    priority, depends_on, queued_at, started_at, completed_at, worker_id, error,
                    max_iterations, iteration, project, phase, worker_timeout_seconds,
                    context, workspace, phase_loop_count, project_context,
                    task_phases, spec_phases, phase_overrides, worker_pool
             FROM specs WHERE id = ?1",
            params![spec_id],
            row_to_spec,
        ) {
            Ok(s) => s,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e),
        };

        let mut task_stmt = self.conn.prepare(
            "SELECT id, spec_id, title, status, depends, started_at, completed_at, error
             FROM tasks WHERE spec_id = ?1 ORDER BY id",
        )?;

        let tasks = task_stmt
            .query_map(params![spec_id], row_to_task)?
            .collect::<Result<Vec<_>>>()?;

        Ok(Some(SpecStatus { spec, tasks }))
    }

    pub fn status_all(&self) -> Result<Vec<SpecRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, mode, status, spec_path,
                    (SELECT COUNT(*) FROM tasks WHERE tasks.spec_id = specs.id) as total_tasks,
                    completed_tasks,
                    priority, depends_on, queued_at, started_at, completed_at, worker_id, error,
                    max_iterations, iteration, project, phase, worker_timeout_seconds,
                    context, workspace, phase_loop_count, project_context,
                    task_phases, spec_phases, phase_overrides, worker_pool
             FROM specs
             ORDER BY
               CASE status WHEN 'running' THEN 0 WHEN 'queued' THEN 1 ELSE 2 END,
               priority ASC,
               queued_at DESC",
        )?;

        let records = stmt.query_map([], row_to_spec)?
            .collect::<Result<Vec<_>>>()?;
        Ok(records)
    }

    pub fn cancel(&self, spec_id: &str) -> Result<()> {
        self.update_spec(spec_id, "cancelled")
    }

    pub fn list_running_specs(&self) -> anyhow::Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, COALESCE(s.title, ''), COALESCE(
                 (SELECT t.id FROM tasks t WHERE t.spec_id = s.id AND t.status = 'RUNNING' LIMIT 1),
                 ''
             )
             FROM specs s
             WHERE s.status IN ('running', 'assigning')
             ORDER BY s.queued_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn set_spec_fields(
        &self,
        spec_id: &str,
        mode: Option<&str>,
        max_iterations: Option<i64>,
        project: Option<&str>,
        worker_timeout_seconds: Option<i64>,
    ) -> Result<()> {
        if let Some(m) = mode {
            self.conn.execute(
                "UPDATE specs SET mode = ?1 WHERE id = ?2",
                params![m, spec_id],
            )?;
        }
        if let Some(mi) = max_iterations {
            self.conn.execute(
                "UPDATE specs SET max_iterations = ?1 WHERE id = ?2",
                params![mi, spec_id],
            )?;
        }
        if let Some(p) = project {
            self.conn.execute(
                "UPDATE specs SET project = ?1 WHERE id = ?2",
                params![p, spec_id],
            )?;
        }
        if let Some(t) = worker_timeout_seconds {
            self.conn.execute(
                "UPDATE specs SET worker_timeout_seconds = ?1 WHERE id = ?2",
                params![t, spec_id],
            )?;
        }
        Ok(())
    }

    pub fn set_priority(&self, spec_id: &str, priority: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE specs SET priority = ?1 WHERE id = ?2",
            params![priority, spec_id],
        )?;
        Ok(())
    }

    pub fn set_depends_on(&self, spec_id: &str, depends_on: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE specs SET depends_on = ?1 WHERE id = ?2",
            params![depends_on, spec_id],
        )?;
        Ok(())
    }

    pub fn get_iterations(&self, spec_id: &str) -> Result<Vec<IterationRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT spec_id, iteration, phase, worker_id, started_at, ended_at,
                    duration_seconds, tasks_completed, tasks_added, exit_code
             FROM iterations WHERE spec_id = ?1 ORDER BY iteration",
        )?;
        let rows = stmt
            .query_map(params![spec_id], |row| {
                Ok(IterationRecord {
                    spec_id: row.get(0)?,
                    iteration: row.get(1)?,
                    phase: row.get(2)?,
                    worker_id: row.get(3)?,
                    started_at: row.get(4)?,
                    ended_at: row.get(5)?,
                    duration_seconds: row.get(6)?,
                    tasks_completed: row.get(7)?,
                    tasks_added: row.get(8)?,
                    exit_code: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    // --- Event records ---

    pub fn insert_event(
        &self,
        spec_id: Option<&str>,
        event_type: &str,
        message: Option<&str>,
        data: Option<&str>,
        level: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO events (timestamp, spec_id, event_type, message, data, level)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now, spec_id, event_type, message, data, level],
        )?;
        Ok(())
    }

    pub fn get_workers(&self) -> Result<Vec<WorkerRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, worktree_path, current_spec_id, current_pid, start_time,
                    current_phase, current_task_id
             FROM workers ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(WorkerRecord {
                    id: row.get(0)?,
                    worktree_path: row.get(1)?,
                    current_spec_id: row.get(2)?,
                    current_pid: row.get(3)?,
                    start_time: row.get(4)?,
                    current_phase: row.get(5)?,
                    current_task_id: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    // --- Phase run records ---

    pub fn insert_phase_run(&self, rec: &PhaseRunRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO phase_runs (spec_id, task_id, phase, level, outcome,
             duration_ms, cost_usd, input_tokens, output_tokens, started_at, completed_at,
             model, runtime, pipeline_id, attempt, failure_mode,
             cold_start_ms, inference_ms, cache_read_tokens, cache_creation_tokens,
             tool_call_count, tool_calls_by_type, ttft_ms, loop_iteration, verify_exit_code,
             context_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25,
                     ?26)",
            params![
                rec.spec_id,
                rec.task_id,
                rec.phase,
                rec.level,
                rec.outcome,
                rec.duration_ms,
                rec.cost_usd,
                rec.input_tokens,
                rec.output_tokens,
                rec.started_at,
                rec.completed_at,
                rec.model,
                rec.runtime,
                rec.pipeline_id,
                rec.attempt,
                rec.failure_mode,
                rec.cold_start_ms,
                rec.inference_ms,
                rec.cache_read_tokens,
                rec.cache_creation_tokens,
                rec.tool_call_count,
                rec.tool_calls_by_type,
                rec.ttft_ms,
                rec.loop_iteration,
                rec.verify_exit_code,
                rec.context_json,
            ],
        )?;
        Ok(())
    }

    pub fn phase_cost_summary(&self, spec_id: &str) -> Result<Vec<PhaseCostSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT phase,
                    COALESCE(SUM(cost_usd), 0.0),
                    COALESCE(SUM(duration_ms), 0),
                    COUNT(*)
             FROM phase_runs WHERE spec_id = ?1
             GROUP BY phase ORDER BY phase",
        )?;
        let rows = stmt
            .query_map(params![spec_id], |row| {
                Ok(PhaseCostSummary {
                    phase: row.get(0)?,
                    total_cost: row.get(1)?,
                    total_duration_ms: row.get(2)?,
                    count: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn phase_cost_total(&self, spec_id: &str) -> Result<f64> {
        let total: f64 = self.conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM phase_runs WHERE spec_id = ?1",
            params![spec_id],
            |row| row.get(0),
        )?;
        Ok(total)
    }

    pub fn insert_bench_result(&self, r: &BenchResultRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO bench_results (run_id, pipeline, spec_file, run_number, status, total_ms,
             tasks_total, tasks_done, tasks_failed, total_cost_usd, total_input_tokens,
             total_output_tokens, tasks_skipped)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![r.run_id, r.pipeline, r.spec_file, r.run_number, r.status, r.total_ms,
                    r.tasks_total, r.tasks_done, r.tasks_failed, r.total_cost_usd,
                    r.total_input_tokens, r.total_output_tokens, r.tasks_skipped],
        )?;
        Ok(())
    }

    /// Aggregate cost and token totals from phase_runs for a given spec.
    pub fn aggregate_spec_cost(&self, spec_id: &str) -> Result<(Option<f64>, Option<i64>, Option<i64>)> {
        let result: (Option<f64>, Option<i64>, Option<i64>) = self.conn.query_row(
            "SELECT SUM(cost_usd), SUM(input_tokens), SUM(output_tokens)
             FROM phase_runs WHERE spec_id = ?1",
            params![spec_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(result)
    }

    // --- Task management ---

    pub fn add_task(
        &self,
        spec_id: &str,
        _authoring_id: &str,
        title: &str,
        spec_text: Option<&str>,
        verify: Option<&str>,
        depends: &[String],
    ) -> Result<String> {
        let task_id = gen_id('T', &self.conn);
        let depends_json = serde_json::to_string(depends).unwrap_or_else(|_| "[]".to_string());
        self.conn.execute(
            "INSERT INTO tasks (id, spec_id, title, status, depends, spec_content, verify_content)
             VALUES (?1, ?2, ?3, 'PENDING', ?4, ?5, ?6)",
            params![task_id, spec_id, title, depends_json, spec_text, verify],
        )?;
        self.conn.execute(
            "UPDATE specs SET total_tasks = (SELECT COUNT(*) FROM tasks WHERE spec_id = ?1) WHERE id = ?1",
            params![spec_id],
        )?;
        if let Err(e) = self.insert_event(
            Some(spec_id),
            "task.added",
            Some(&format!("Added task {} to {}", task_id, spec_id)),
            None,
            "info",
        ) {
            eprintln!("[boi] ERROR: failed to insert task.added event for {} in {}: {}", task_id, spec_id, e);
        }
        Ok(task_id)
    }

    pub fn skip_task(&self, spec_id: &str, task_id: &str) -> Result<()> {
        self.update_task(spec_id, task_id, "SKIPPED")
    }

    pub fn update_task_spec_content(&self, spec_id: &str, task_id: &str, content: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET spec_content = ?1 WHERE spec_id = ?2 AND id = ?3",
            params![content, spec_id, task_id],
        )?;
        Ok(())
    }

    pub fn update_task_verify_content(&self, spec_id: &str, task_id: &str, content: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET verify_content = ?1 WHERE spec_id = ?2 AND id = ?3",
            params![content, spec_id, task_id],
        )?;
        Ok(())
    }

    pub fn block_task(&self, spec_id: &str, task_id: &str, dep_id: &str) -> Result<()> {
        // Read current depends, add the new dep
        let current: String = self.conn.query_row(
            "SELECT depends FROM tasks WHERE spec_id = ?1 AND id = ?2",
            params![spec_id, task_id],
            |row| row.get(0),
        )?;
        let mut deps: Vec<String> = serde_json::from_str(&current).unwrap_or_default();
        if !deps.contains(&dep_id.to_string()) {
            deps.push(dep_id.to_string());
        }
        let new_deps = serde_json::to_string(&deps).unwrap_or_else(|_| "[]".to_string());
        self.conn.execute(
            "UPDATE tasks SET depends = ?1 WHERE spec_id = ?2 AND id = ?3",
            params![new_deps, spec_id, task_id],
        )?;
        Ok(())
    }

    pub fn get_tasks(&self, spec_id: &str) -> Result<Vec<TaskRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec_id, title, status, depends, started_at, completed_at, error FROM tasks WHERE spec_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![spec_id], row_to_task)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    }

    pub fn get_tasks_full(&self, spec_id: &str) -> Result<Vec<FullTaskRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec_id, title, status, depends, spec_content, verify_content
             FROM tasks WHERE spec_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map(params![spec_id], |row| {
                Ok(FullTaskRecord {
                    id: row.get(0)?,
                    spec_id: row.get(1)?,
                    title: row.get(2)?,
                    status: row.get(3)?,
                    depends: row.get(4)?,
                    spec_content: row.get(5)?,
                    verify_content: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Reset any specs stuck in 'running' or 'assigning' back to 'queued'.
    /// Called on daemon startup to recover from crashes.
    pub fn recover_stuck_specs(&self) -> Result<usize> {
        // Reset RUNNING tasks back to PENDING before resetting specs,
        // so the subquery still matches specs in ('running', 'assigning').
        self.conn.execute(
            "UPDATE tasks SET status = 'PENDING', started_at = NULL
             WHERE spec_id IN (SELECT id FROM specs WHERE status IN ('running', 'assigning'))
               AND status = 'RUNNING'",
            [],
        )?;
        self.conn.execute(
            "UPDATE specs SET status = 'queued', worker_id = NULL, started_at = NULL, error = NULL
             WHERE status IN ('running', 'assigning')",
            [],
        )
    }

    pub fn prune_events(&self, days: u32) -> Result<usize> {
        let cutoff = Utc::now() - chrono::Duration::days(days as i64);
        self.conn.execute(
            "DELETE FROM events WHERE timestamp < ?1",
            params![cutoff.to_rfc3339()],
        )
    }

    pub fn prune_phase_runs(&self, days: u32) -> Result<usize> {
        let cutoff = Utc::now() - chrono::Duration::days(days as i64);
        self.conn.execute(
            "DELETE FROM phase_runs WHERE started_at < ?1",
            params![cutoff.to_rfc3339()],
        )
    }

    /// Get lifetime counts of failed and completed specs (across entire DB history)
    pub fn lifetime_counts(&self) -> Result<(i64, i64)> {
        let failed: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM specs WHERE status = 'failed'",
            [],
            |r| r.get(0),
        )?;
        let completed: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM specs WHERE status = 'completed'",
            [],
            |r| r.get(0),
        )?;
        Ok((failed, completed))
    }

    /// Get outcome count for a spec by reading the YAML spec file
    pub fn outcome_count(&self, spec_id: &str) -> i64 {
        let path: Option<String> = self
            .conn
            .query_row(
                "SELECT spec_path FROM specs WHERE id = ?1",
                rusqlite::params![spec_id],
                |r| r.get(0),
            )
            .unwrap_or(None);
        if let Some(p) = path {
            if let Ok(content) = std::fs::read_to_string(&p) {
                // Count "- description:" lines under outcomes
                let mut in_outcomes = false;
                let mut count: i64 = 0;
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed == "outcomes:" {
                        in_outcomes = true;
                        continue;
                    }
                    if in_outcomes {
                        if trimmed.starts_with("- description:") {
                            count += 1;
                        } else if !trimmed.is_empty()
                            && !trimmed.starts_with("- ")
                            && !trimmed.starts_with("verify:")
                        {
                            // Left the outcomes section
                            break;
                        }
                    }
                }
                count
            } else {
                0
            }
        } else {
            0
        }
    }

    /// Increment the critique↔improve loop counter for a spec.
    /// Returns the new count after incrementing.
    pub fn increment_phase_loop_count(&self, spec_id: &str) -> Result<i64> {
        self.conn.execute(
            "UPDATE specs SET phase_loop_count = phase_loop_count + 1 WHERE id = ?1",
            params![spec_id],
        )?;
        let count: i64 = self.conn.query_row(
            "SELECT phase_loop_count FROM specs WHERE id = ?1",
            params![spec_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Get the current critique↔improve loop counter for a spec.
    pub fn get_phase_loop_count(&self, spec_id: &str) -> Result<i64> {
        let count: i64 = self.conn.query_row(
            "SELECT COALESCE(phase_loop_count, 0) FROM specs WHERE id = ?1",
            params![spec_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Reset the critique↔improve loop counter to zero.
    pub fn reset_phase_loop_count(&self, spec_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE specs SET phase_loop_count = 0 WHERE id = ?1",
            params![spec_id],
        )?;
        Ok(())
    }

    /// Returns true if the critique↔improve loop has reached or exceeded the cap.
    /// Logs a warning when the cap is hit so the pipeline can proceed to task execution.
    pub fn phase_loop_capped(&self, spec_id: &str, max_loops: i64) -> bool {
        let count = self.get_phase_loop_count(spec_id).unwrap_or(0);
        if count >= max_loops {
            eprintln!(
                "[boi] WARN: spec {} reached max critique/improve loops ({}/{}); proceeding to task execution",
                spec_id, count, max_loops
            );
            true
        } else {
            false
        }
    }

    /// Get the last updated timestamp across all specs (for heartbeat detection)
    pub fn last_spec_update(&self) -> Result<Option<String>> {
        let result: Option<String> = self
            .conn
            .query_row(
                "SELECT MAX(COALESCE(completed_at, started_at, queued_at))
                 FROM specs WHERE status = 'running'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(None);
        Ok(result)
    }

    /// Return all non-null current_pid values from the workers table.
    pub fn get_worker_pids(&self) -> Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT current_pid FROM workers WHERE current_pid IS NOT NULL",
        )?;
        let pids = stmt
            .query_map([], |row| row.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .map(|p| p as u32)
            .collect();
        Ok(pids)
    }

    /// Return (active_pids, ended_pids) from the processes table.
    /// active = ended_at IS NULL, ended = ended_at IS NOT NULL.
    pub fn get_process_pids(&self) -> Result<(Vec<u32>, Vec<u32>)> {
        let mut stmt = self.conn.prepare(
            "SELECT pid, ended_at FROM processes WHERE pid IS NOT NULL",
        )?;
        let mut active = Vec::new();
        let mut ended = Vec::new();
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows.filter_map(|r| r.ok()) {
            let (pid, ended_at) = row;
            if ended_at.is_some() {
                ended.push(pid as u32);
            } else {
                active.push(pid as u32);
            }
        }
        Ok((active, ended))
    }

    /// Set ended_at on all processes rows for the given pid that have no ended_at yet.
    pub fn mark_process_ended_by_pid(&self, pid: u32) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE processes SET ended_at = ?1 WHERE pid = ?2 AND ended_at IS NULL",
            params![now, pid as i64],
        )
    }

    // --- Daemon consistency health checks ---

    /// Count specs stuck in `running` >1 hour with no iteration activity in the last hour
    /// (ghost worker symptom).
    pub fn ghost_worker_count(&self) -> Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM specs
             WHERE status = 'running'
               AND started_at < datetime('now', '-1 hour')
               AND NOT EXISTS (
                 SELECT 1 FROM iterations i
                 WHERE i.spec_id = specs.id
                   AND i.started_at > datetime('now', '-1 hour')
               )",
            [],
            |r| r.get(0),
        )
    }

    /// Count specs in `running` with no worker_id set (duplicate-assignment symptom).
    pub fn running_without_worker_id_count(&self) -> Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM specs
             WHERE status = 'running'
               AND (worker_id IS NULL OR worker_id = '')",
            [],
            |r| r.get(0),
        )
    }

    /// Count specs in `running` or `assigning` for >1 hour with no recent iteration
    /// (stale status symptom).
    pub fn stale_status_count(&self) -> Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM specs
             WHERE status IN ('running', 'assigning')
               AND started_at < datetime('now', '-1 hour')
               AND (
                 NOT EXISTS (SELECT 1 FROM iterations WHERE spec_id = specs.id)
                 OR (SELECT MAX(started_at) FROM iterations WHERE spec_id = specs.id)
                      < datetime('now', '-1 hour')
               )",
            [],
            |r| r.get(0),
        )
    }
}

/// Read and concatenate content from a list of file paths.
/// Expands `~` to $HOME. Missing files are skipped silently.
/// Truncates at 50 000 chars to prevent prompt bloat.
pub fn read_context_files(paths: &[String]) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out = String::new();
    for raw in paths {
        let path = raw.replace('~', &home);
        if let Ok(content) = std::fs::read_to_string(&path) {
            out.push_str(&format!("\n<!-- context: {} -->\n", path));
            out.push_str(&content);
        }
        if out.len() >= 50_000 {
            out.truncate(50_000);
            break;
        }
    }
    out
}

fn row_to_spec(row: &rusqlite::Row<'_>) -> rusqlite::Result<SpecRecord> {
    Ok(SpecRecord {
        id: row.get(0)?,
        title: row.get(1)?,
        mode: row.get(2)?,
        status: row.get(3)?,
        spec_path: row.get(4)?,
        total_tasks: row.get(5)?,
        completed_tasks: row.get(6)?,
        priority: row.get(7)?,
        depends_on: row.get(8)?,
        queued_at: row.get(9)?,
        started_at: row.get(10)?,
        completed_at: row.get(11)?,
        worker_id: row.get(12)?,
        error: row.get(13)?,
        max_iterations: row.get::<_, Option<i64>>(14)?.unwrap_or(30),
        iteration: row.get::<_, Option<i64>>(15)?.unwrap_or(0),
        project: row.get(16)?,
        phase: row.get::<_, Option<String>>(17)?.unwrap_or_else(|| "execute".to_string()),
        worker_timeout_seconds: row.get(18)?,
        context: row.get(19)?,
        workspace: row.get(20)?,
        phase_loop_count: row.get::<_, Option<i64>>(21)?.unwrap_or(0),
        project_context: row.get(22)?,
        task_phases: row.get(23)?,
        spec_phases: row.get(24)?,
        phase_overrides: row.get(25)?,
        worker_pool: row.get(26)?,
    })
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    Ok(TaskRecord {
        id: row.get(0)?,
        spec_id: row.get(1)?,
        title: row.get(2)?,
        status: row.get(3)?,
        depends: row.get(4)?,
        started_at: row.get(5)?,
        completed_at: row.get(6)?,
        error: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{BoiSpec, BoiTask, TaskStatus};

    fn make_spec(title: &str, tasks: Vec<BoiTask>) -> BoiSpec {
        BoiSpec {
            title: title.to_string(),
            mode: Some("execute".to_string()),
            workspace: None,
            workspace_rationale: Some("test fixture — no repo target".into()),
            initiative: None,
            context: None,
            outcomes: None,
            spec_phases: None,
            task_phases: None,
            context_files: None,
            phase_overrides: std::collections::HashMap::new(),
            worker_pool: None,
            max_cost_usd: None,
            key_artifacts: None,
            tasks,
        }
    }

    fn make_task(id: &str, title: &str) -> BoiTask {
        BoiTask {
            id: id.to_string(),
            title: title.to_string(),
            status: TaskStatus::Pending,
            depends: None,
            spec: None,
            verify: None,
            verify_prompt: None,
            phases: None,
        }
    }

    fn open_mem() -> Queue {
        Queue::open(":memory:").unwrap()
    }

    fn is_valid_spec_id(id: &str) -> bool {
        id.len() == 5
            && id.starts_with('S')
            && id[1..].chars().all(|c| c.is_ascii_hexdigit())
    }

    fn is_valid_task_id(id: &str) -> bool {
        id.len() == 5
            && id.starts_with('T')
            && id[1..].chars().all(|c| c.is_ascii_hexdigit())
    }

    #[test]
    fn test_enqueue_auto_id() {
        let q = open_mem();
        let spec = make_spec("My Spec", vec![make_task("t-1", "Setup"), make_task("t-2", "Run")]);
        let id = q.enqueue(&spec, None).unwrap();
        assert!(is_valid_spec_id(&id), "spec id={}", id);
        let st = q.status(&id).unwrap().unwrap();
        for task in &st.tasks {
            assert!(is_valid_task_id(&task.id), "task id={}", task.id);
        }
    }

    #[test]
    fn test_unique_ids() {
        let q = open_mem();
        let spec = make_spec("S1", vec![make_task("t-1", "A")]);
        let id1 = q.enqueue(&spec, None).unwrap();
        let id2 = q.enqueue(&spec, None).unwrap();
        assert!(is_valid_spec_id(&id1), "id1={}", id1);
        assert!(is_valid_spec_id(&id2), "id2={}", id2);
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_dequeue_returns_queued_spec() {
        let q = open_mem();
        let spec = make_spec("Dequeue Test", vec![make_task("t-1", "Task")]);
        let id = q.enqueue(&spec, None).unwrap();
        let dequeued = q.dequeue().unwrap().expect("should find a spec");
        assert_eq!(dequeued.id, id);
        assert_eq!(dequeued.status, "assigning");
    }

    #[test]
    fn test_dequeue_empty() {
        let q = open_mem();
        assert!(q.dequeue().unwrap().is_none());
    }

    #[test]
    fn test_dequeue_skips_running() {
        let q = open_mem();
        let spec = make_spec("Running", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, None).unwrap();
        q.update_spec(&id, "running").unwrap();
        assert!(q.dequeue().unwrap().is_none());
    }

    #[test]
    fn test_dequeue_priority_order() {
        let q = open_mem();
        let spec = make_spec("Low", vec![make_task("t-1", "T")]);
        let id_low = q.enqueue(&spec, None).unwrap();
        q.conn
            .execute("UPDATE specs SET priority = 200 WHERE id = ?1", params![id_low])
            .unwrap();

        let spec2 = make_spec("High", vec![make_task("t-1", "T")]);
        let id_high = q.enqueue(&spec2, None).unwrap();
        q.conn
            .execute("UPDATE specs SET priority = 50 WHERE id = ?1", params![id_high])
            .unwrap();

        let dequeued = q.dequeue().unwrap().unwrap();
        assert_eq!(dequeued.id, id_high);
    }

    #[test]
    fn test_update_task_done_increments_completed() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T"), make_task("t-2", "U")]);
        let id = q.enqueue(&spec, None).unwrap();
        let st = q.status(&id).unwrap().unwrap();
        let task_id = st.tasks[0].id.clone();
        q.update_task(&id, &task_id, "DONE").unwrap();
        let st = q.status(&id).unwrap().unwrap();
        assert_eq!(st.spec.completed_tasks, 1);
    }

    #[test]
    fn test_update_spec_status() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, None).unwrap();
        q.update_spec(&id, "completed").unwrap();
        let st = q.status(&id).unwrap().unwrap();
        assert_eq!(st.spec.status, "completed");
        assert!(st.spec.completed_at.is_some());
    }

    #[test]
    fn test_cancel() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, None).unwrap();
        q.cancel(&id).unwrap();
        let st = q.status(&id).unwrap().unwrap();
        assert_eq!(st.spec.status, "cancelled");
    }

    #[test]
    fn test_status_not_found() {
        let q = open_mem();
        assert!(q.status("sffff").unwrap().is_none());
    }

    #[test]
    fn test_status_all_ordering() {
        let q = open_mem();
        let spec = make_spec("A", vec![make_task("t-1", "T")]);
        let id1 = q.enqueue(&spec, None).unwrap();
        let id2 = q.enqueue(&spec, None).unwrap();
        q.update_spec(&id1, "running").unwrap();
        let all = q.status_all().unwrap();
        assert_eq!(all[0].id, id1, "running spec should come first");
        assert_eq!(all[1].id, id2);
    }

    #[test]
    fn test_enqueue_stores_spec_path() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, Some("/path/to/spec.yaml")).unwrap();
        let st = q.status(&id).unwrap().unwrap();
        assert_eq!(st.spec.spec_path.as_deref(), Some("/path/to/spec.yaml"));
    }

    #[test]
    fn test_enqueue_with_context_stores_project_context() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue_with_context(&spec, None, Some("my context content")).unwrap();
        let st = q.status(&id).unwrap().unwrap();
        assert_eq!(st.spec.project_context.as_deref(), Some("my context content"));
    }

    #[test]
    fn test_enqueue_project_context_defaults_to_none() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, None).unwrap();
        let st = q.status(&id).unwrap().unwrap();
        assert!(st.spec.project_context.is_none());
    }

    #[test]
    fn test_read_context_files_missing_file() {
        let result = read_context_files(&["/tmp/nonexistent-boi-test-file.md".to_string()]);
        assert!(result.is_empty(), "missing files should produce empty output");
    }

    #[test]
    fn test_read_context_files_reads_content() {
        use std::io::Write;
        let path = crate::test_utils::test_file("context-test", "md");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"hello context").unwrap();
        let result = read_context_files(&[path.to_str().unwrap().to_string()]);
        assert!(result.contains("hello context"), "result={}", result);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_tasks_stored_on_enqueue() {
        let q = open_mem();
        let spec = make_spec(
            "S",
            vec![make_task("t-1", "First"), make_task("t-2", "Second")],
        );
        let id = q.enqueue(&spec, None).unwrap();
        let st = q.status(&id).unwrap().unwrap();
        assert_eq!(st.tasks.len(), 2);
        assert!(is_valid_task_id(&st.tasks[0].id), "task[0].id={}", st.tasks[0].id);
        assert!(is_valid_task_id(&st.tasks[1].id), "task[1].id={}", st.tasks[1].id);
        assert_ne!(st.tasks[0].id, st.tasks[1].id);
    }

    #[test]
    fn test_recover_stuck_specs() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);

        // Create a 'running' spec with a RUNNING task
        let id1 = q.enqueue(&spec, None).unwrap();
        q.update_spec(&id1, "running").unwrap();
        let st = q.status(&id1).unwrap().unwrap();
        let t1 = st.tasks[0].id.clone();
        q.update_task(&id1, &t1, "RUNNING").unwrap();

        // Create an 'assigning' spec (no running tasks)
        let id2 = q.enqueue(&spec, None).unwrap();
        q.conn
            .execute(
                "UPDATE specs SET status = 'assigning' WHERE id = ?1",
                params![id2],
            )
            .unwrap();

        // A completed spec — must NOT be reset
        let id3 = q.enqueue(&spec, None).unwrap();
        q.update_spec(&id3, "completed").unwrap();

        let count = q.recover_stuck_specs().unwrap();
        assert_eq!(count, 2, "should have reset 2 stuck specs");

        let st1 = q.status(&id1).unwrap().unwrap();
        assert_eq!(st1.spec.status, "queued", "running spec must be reset to queued");
        assert_eq!(st1.tasks[0].status, "PENDING", "RUNNING task must be reset to PENDING");

        let st2 = q.status(&id2).unwrap().unwrap();
        assert_eq!(st2.spec.status, "queued", "assigning spec must be reset to queued");

        let st3 = q.status(&id3).unwrap().unwrap();
        assert_eq!(st3.spec.status, "completed", "completed spec must not be touched");
    }

    #[test]
    fn recover_stuck_specs_preserves_done() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T1"), make_task("t-2", "T2"), make_task("t-3", "T3")]);
        let id = q.enqueue(&spec, None).unwrap();
        q.update_spec(&id, "running").unwrap();

        // Use get_tasks (ORDER BY rowid = insertion order) consistently
        let initial_tasks = q.get_tasks(&id).unwrap();
        let tid1 = initial_tasks[0].id.clone();
        let tid2 = initial_tasks[1].id.clone();

        q.update_task(&id, &tid1, "DONE").unwrap();
        q.update_task(&id, &tid2, "RUNNING").unwrap();

        let count = q.recover_stuck_specs().unwrap();
        assert_eq!(count, 1, "should reset exactly 1 task (t-2)");

        let tasks = q.get_tasks(&id).unwrap();
        let t1 = tasks.iter().find(|t| t.id == tid1).expect("t-1 missing");
        let t2 = tasks.iter().find(|t| t.id == tid2).expect("t-2 missing");
        let t3 = tasks.iter().find(|t| t.id != tid1 && t.id != tid2).expect("t-3 missing");
        assert_eq!(t1.status, "DONE", "T1 must stay DONE");
        assert_eq!(t2.status, "PENDING", "T2 must be reset to PENDING");
        assert_eq!(t3.status, "PENDING", "T3 must stay PENDING");
    }

    #[test]
    fn restart_resumes_at_pending_task() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T1"), make_task("t-2", "T2"), make_task("t-3", "T3")]);
        let id = q.enqueue(&spec, None).unwrap();

        // Use get_tasks (ORDER BY rowid = insertion order) consistently
        let initial_tasks = q.get_tasks(&id).unwrap();
        let tid1 = initial_tasks[0].id.clone();
        let tid2 = initial_tasks[1].id.clone();

        q.update_task(&id, &tid1, "DONE").unwrap();

        let count = q.recover_stuck_specs().unwrap();
        assert_eq!(count, 0, "no RUNNING tasks to reset on clean restart");

        let tasks = q.get_tasks(&id).unwrap();
        let t1 = tasks.iter().find(|t| t.id == tid1).unwrap();
        let t2 = tasks.iter().find(|t| t.id == tid2).unwrap();
        assert_eq!(t1.status, "DONE");
        assert_eq!(t2.status, "PENDING");

        let done_ids: std::collections::HashSet<String> = tasks.iter()
            .filter(|t| t.status == "DONE")
            .map(|t| t.id.clone())
            .collect();
        assert!(done_ids.contains(&tid1), "done_ids must contain t-1");
        assert!(!done_ids.contains(&tid2), "done_ids must not contain t-2");

        let first_pending = tasks.iter()
            .find(|t| !done_ids.contains(&t.id))
            .expect("must have a pending task");
        assert_eq!(first_pending.id, tid2, "first pending task must be t-2");
    }

    #[test]
    fn test_gen_id_collision() {
        let q = open_mem();
        // Pre-insert a known spec ID to create a collision target.
        q.conn
            .execute(
                "INSERT INTO specs (id, title, mode, status, queued_at)
                 VALUES ('S0000', 'blocker', 'execute', 'queued', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        // gen_id must return a valid ID that differs from the pre-existing one.
        let id = gen_id('S', &q.conn);
        assert_ne!(id, "S0000", "gen_id must not return an already-existing ID");
        assert!(
            id.starts_with('S') && id[1..].chars().all(|c| c.is_ascii_hexdigit()),
            "gen_id returned invalid format: {}",
            id
        );
        // Insert the new ID and try again to ensure repeated calls also avoid collisions.
        q.conn
            .execute(
                "INSERT INTO specs (id, title, mode, status, queued_at)
                 VALUES (?1, 'X', 'execute', 'queued', '2026-01-01T00:00:00Z')",
                params![id],
            )
            .unwrap();
        let id2 = gen_id('S', &q.conn);
        assert_ne!(id2, "S0000", "gen_id returned S0000 after it was inserted");
        assert_ne!(id2, id, "gen_id returned duplicate on second call");
    }

    #[test]
    fn test_depends_on_blocks_dequeue() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let blocker_id = q.enqueue(&spec, None).unwrap();

        // Enqueue a second spec that depends on the first
        let id2 = q.enqueue(&spec, None).unwrap();
        q.conn
            .execute(
                "UPDATE specs SET depends_on = ?1 WHERE id = ?2",
                params![blocker_id, id2],
            )
            .unwrap();

        // Only the first (no dependency) should dequeue
        let dequeued = q.dequeue().unwrap().unwrap();
        assert_eq!(dequeued.id, blocker_id);

        // Complete the blocker; now the second should dequeue
        q.update_spec(&blocker_id, "completed").unwrap();
        let dequeued2 = q.dequeue().unwrap().unwrap();
        assert_eq!(dequeued2.id, id2);
    }

    // --- spec_improve: loop cap enforcement ---

    #[test]
    fn test_spec_improve_loop_count_starts_at_zero() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, None).unwrap();
        assert_eq!(q.get_phase_loop_count(&id).unwrap(), 0);
    }

    #[test]
    fn test_spec_improve_loop_count_increments() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, None).unwrap();

        let after_first = q.increment_phase_loop_count(&id).unwrap();
        assert_eq!(after_first, 1);

        let after_second = q.increment_phase_loop_count(&id).unwrap();
        assert_eq!(after_second, 2);
    }

    #[test]
    fn test_spec_improve_loop_count_resets() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, None).unwrap();

        q.increment_phase_loop_count(&id).unwrap();
        q.increment_phase_loop_count(&id).unwrap();
        q.reset_phase_loop_count(&id).unwrap();

        assert_eq!(q.get_phase_loop_count(&id).unwrap(), 0);
    }

    #[test]
    fn test_spec_improve_loop_not_capped_below_max() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, None).unwrap();

        q.increment_phase_loop_count(&id).unwrap();
        q.increment_phase_loop_count(&id).unwrap();

        assert!(!q.phase_loop_capped(&id, 3), "count=2 should not be capped at max=3");
    }

    #[test]
    fn test_spec_improve_loop_capped_at_max() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, None).unwrap();

        for _ in 0..3 {
            q.increment_phase_loop_count(&id).unwrap();
        }

        assert!(q.phase_loop_capped(&id, 3), "count=3 must be capped at max=3");
    }

    #[test]
    fn test_spec_improve_loop_cap_configurable() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, None).unwrap();

        q.increment_phase_loop_count(&id).unwrap();

        // max_loops=1: capped after 1 iteration
        assert!(q.phase_loop_capped(&id, 1));
        // max_loops=5: not capped yet
        assert!(!q.phase_loop_capped(&id, 5));
    }

    #[test]
    fn test_spec_improve_loop_count_in_spec_record() {
        let q = open_mem();
        let spec = make_spec("S", vec![make_task("t-1", "T")]);
        let id = q.enqueue(&spec, None).unwrap();

        q.increment_phase_loop_count(&id).unwrap();
        q.increment_phase_loop_count(&id).unwrap();

        let st = q.status(&id).unwrap().unwrap();
        assert_eq!(st.spec.phase_loop_count, 2, "phase_loop_count must be readable from SpecRecord");
    }

    // --- Telemetry column tests ---

    #[test]
    fn test_phase_run_new_columns_stored() {
        let q = open_mem();
        let rec = PhaseRunRecord {
            spec_id: "S0001".to_string(),
            task_id: Some("T0001".to_string()),
            phase: "execute".to_string(),
            level: "task".to_string(),
            outcome: "proceed".to_string(),
            duration_ms: Some(5000),
            cost_usd: Some(0.045),
            input_tokens: Some(1500),
            output_tokens: Some(800),
            started_at: "2026-04-29T00:00:00Z".to_string(),
            completed_at: Some("2026-04-29T00:00:05Z".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            runtime: Some("claude".to_string()),
            pipeline_id: Some("v1-abc123".to_string()),
            attempt: 2,
            failure_mode: None,
            cold_start_ms: Some(200),
            inference_ms: Some(4800),
            cache_read_tokens: Some(500),
            cache_creation_tokens: Some(300),
            tool_call_count: Some(12),
            tool_calls_by_type: Some(r#"{"Read":5,"Edit":4,"Bash":3}"#.to_string()),
            ttft_ms: Some(200),
            loop_iteration: Some(2),
            verify_exit_code: Some(0),
            context_json: None,
        };
        q.insert_phase_run(&rec).unwrap();

        let row: (
            Option<String>, Option<String>, Option<String>, i64,
            Option<String>, Option<i64>, Option<i64>, Option<i64>,
            Option<i64>, Option<i64>, Option<String>, Option<i64>,
            Option<f64>, Option<i64>, Option<i64>,
        ) = q.conn.query_row(
            "SELECT model, runtime, pipeline_id, attempt,
                    failure_mode, cold_start_ms, inference_ms,
                    cache_read_tokens, cache_creation_tokens,
                    tool_call_count, tool_calls_by_type, ttft_ms,
                    cost_usd, input_tokens, output_tokens
             FROM phase_runs WHERE spec_id = 'S0001'",
            [],
            |r| Ok((
                r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?,
                r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?,
                r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?,
                r.get(12)?, r.get(13)?, r.get(14)?,
            )),
        ).unwrap();

        assert_eq!(row.0, Some("claude-sonnet-4-6".to_string()));
        assert_eq!(row.1, Some("claude".to_string()));
        assert_eq!(row.2, Some("v1-abc123".to_string()));
        assert_eq!(row.3, 2);
        assert_eq!(row.4, None); // no failure_mode on success
        assert_eq!(row.5, Some(200));
        assert_eq!(row.6, Some(4800));
        assert_eq!(row.7, Some(500));
        assert_eq!(row.8, Some(300));
        assert_eq!(row.9, Some(12));
        assert!(row.10.as_ref().unwrap().contains("Read"));
        assert_eq!(row.11, Some(200));
        assert!((row.12.unwrap() - 0.045).abs() < 1e-6);
        assert_eq!(row.13, Some(1500));
        assert_eq!(row.14, Some(800));
    }

    #[test]
    fn test_phase_run_failure_mode_stored() {
        let q = open_mem();
        let rec = PhaseRunRecord {
            spec_id: "S0002".to_string(),
            task_id: None,
            phase: "critic".to_string(),
            level: "spec".to_string(),
            outcome: "failed".to_string(),
            duration_ms: Some(30000),
            cost_usd: None,
            input_tokens: None,
            output_tokens: None,
            started_at: "2026-04-29T00:00:00Z".to_string(),
            completed_at: Some("2026-04-29T00:00:30Z".to_string()),
            model: Some("claude-opus-4-7".to_string()),
            runtime: Some("openrouter".to_string()),
            pipeline_id: None,
            attempt: 1,
            failure_mode: Some("timeout".to_string()),
            cold_start_ms: None,
            inference_ms: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            tool_call_count: Some(0),
            tool_calls_by_type: None,
            ttft_ms: None,
            loop_iteration: None,
            verify_exit_code: None,
            context_json: None,
        };
        q.insert_phase_run(&rec).unwrap();

        let failure_mode: Option<String> = q.conn.query_row(
            "SELECT failure_mode FROM phase_runs WHERE spec_id = 'S0002'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(failure_mode, Some("timeout".to_string()));
    }

    #[test]
    fn test_bench_result_cost_columns() {
        let q = open_mem();
        let rec = BenchResultRecord {
            run_id: "bench-001".to_string(),
            pipeline: "v1".to_string(),
            spec_file: "test.yaml".to_string(),
            run_number: 1,
            status: "completed".to_string(),
            total_ms: 60000,
            tasks_total: 5,
            tasks_done: 4,
            tasks_failed: 0,
            total_cost_usd: Some(0.23),
            total_input_tokens: Some(15000),
            total_output_tokens: Some(8000),
            tasks_skipped: 1,
        };
        q.insert_bench_result(&rec).unwrap();

        let row: (Option<f64>, Option<i64>, Option<i64>, i64) = q.conn.query_row(
            "SELECT total_cost_usd, total_input_tokens, total_output_tokens, tasks_skipped
             FROM bench_results WHERE run_id = 'bench-001'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).unwrap();

        assert!((row.0.unwrap() - 0.23).abs() < 1e-6);
        assert_eq!(row.1, Some(15000));
        assert_eq!(row.2, Some(8000));
        assert_eq!(row.3, 1);
    }

    #[test]
    fn test_aggregate_spec_cost() {
        let q = open_mem();
        for i in 0..3 {
            let rec = PhaseRunRecord {
                spec_id: "S0010".to_string(),
                task_id: Some(format!("T{:04}", i)),
                phase: "execute".to_string(),
                level: "task".to_string(),
                outcome: "proceed".to_string(),
                duration_ms: Some(1000),
                cost_usd: Some(0.01 * (i + 1) as f64),
                input_tokens: Some(100 * (i + 1) as i64),
                output_tokens: Some(50 * (i + 1) as i64),
                started_at: "2026-04-29T00:00:00Z".to_string(),
                completed_at: Some("2026-04-29T00:00:01Z".to_string()),
                model: None,
                runtime: None,
                pipeline_id: None,
                attempt: 1,
                failure_mode: None,
                cold_start_ms: None,
                inference_ms: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                tool_call_count: None,
                tool_calls_by_type: None,
                ttft_ms: None,
                loop_iteration: None,
                verify_exit_code: None,
                context_json: None,
            };
            q.insert_phase_run(&rec).unwrap();
        }

        let (cost, input, output) = q.aggregate_spec_cost("S0010").unwrap();
        assert!((cost.unwrap() - 0.06).abs() < 1e-6, "cost={:?}", cost);
        assert_eq!(input, Some(600));
        assert_eq!(output, Some(300));
    }

    #[test]
    fn test_phase_run_columns_migrated() {
        let q = open_mem();
        let has_col = |table: &str, col: &str| -> bool {
            q.conn.prepare(&format!("PRAGMA table_info({})", table))
                .and_then(|mut stmt| {
                    let rows = stmt.query_map([], |row| {
                        let name: String = row.get(1)?;
                        Ok(name)
                    })?;
                    Ok(rows.filter_map(|r| r.ok()).any(|n| n == col))
                })
                .unwrap_or(false)
        };

        assert!(has_col("phase_runs", "model"), "model column should exist");
        assert!(has_col("phase_runs", "runtime"), "runtime column should exist");
        assert!(has_col("phase_runs", "pipeline_id"), "pipeline_id column should exist");
        assert!(has_col("phase_runs", "attempt"), "attempt column should exist");
        assert!(has_col("phase_runs", "failure_mode"), "failure_mode column should exist");
        assert!(has_col("phase_runs", "cold_start_ms"), "cold_start_ms column should exist");
        assert!(has_col("phase_runs", "inference_ms"), "inference_ms column should exist");
        assert!(has_col("phase_runs", "cache_read_tokens"), "cache_read_tokens column should exist");
        assert!(has_col("phase_runs", "cache_creation_tokens"), "cache_creation_tokens column should exist");
        assert!(has_col("phase_runs", "tool_call_count"), "tool_call_count column should exist");
        assert!(has_col("phase_runs", "tool_calls_by_type"), "tool_calls_by_type column should exist");
        assert!(has_col("phase_runs", "ttft_ms"), "ttft_ms column should exist");
        assert!(has_col("phase_runs", "loop_iteration"), "loop_iteration column should exist");
        assert!(has_col("phase_runs", "verify_exit_code"), "verify_exit_code column should exist");
        assert!(has_col("phase_runs", "context_json"), "context_json column should exist");

        assert!(has_col("bench_results", "total_cost_usd"), "total_cost_usd column should exist");
        assert!(has_col("bench_results", "total_input_tokens"), "total_input_tokens column should exist");
        assert!(has_col("bench_results", "total_output_tokens"), "total_output_tokens column should exist");
        assert!(has_col("bench_results", "tasks_skipped"), "tasks_skipped column should exist");
    }

    #[test]
    fn test_phase_run_loop_iteration_stored() {
        let q = open_mem();
        let rec = PhaseRunRecord {
            spec_id: "S0020".to_string(),
            task_id: None,
            phase: "spec-critique".to_string(),
            level: "spec".to_string(),
            outcome: "redo".to_string(),
            duration_ms: Some(2000),
            cost_usd: None,
            input_tokens: None,
            output_tokens: None,
            started_at: "2026-04-29T00:00:00Z".to_string(),
            completed_at: Some("2026-04-29T00:00:02Z".to_string()),
            model: Some("gemini-flash".to_string()),
            runtime: Some("openrouter".to_string()),
            pipeline_id: Some("exp5-cond-v1".to_string()),
            attempt: 1,
            failure_mode: None,
            cold_start_ms: None,
            inference_ms: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            tool_call_count: None,
            tool_calls_by_type: None,
            ttft_ms: None,
            loop_iteration: Some(2),
            verify_exit_code: None,
            context_json: None,
        };
        q.insert_phase_run(&rec).unwrap();

        let loop_iter: Option<i64> = q.conn.query_row(
            "SELECT loop_iteration FROM phase_runs WHERE spec_id = 'S0020'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(loop_iter, Some(2), "loop_iteration=2 should be stored");
    }

    #[test]
    fn test_phase_run_verify_exit_code_stored() {
        let q = open_mem();
        let rec = PhaseRunRecord {
            spec_id: "S0021".to_string(),
            task_id: Some("T0001".to_string()),
            phase: "task-verify".to_string(),
            level: "task".to_string(),
            outcome: "redo".to_string(),
            duration_ms: Some(500),
            cost_usd: None,
            input_tokens: None,
            output_tokens: None,
            started_at: "2026-04-29T00:00:00Z".to_string(),
            completed_at: Some("2026-04-29T00:00:00Z".to_string()),
            model: None,
            runtime: Some("verify".to_string()),
            pipeline_id: None,
            attempt: 1,
            failure_mode: None,
            cold_start_ms: None,
            inference_ms: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            tool_call_count: None,
            tool_calls_by_type: None,
            ttft_ms: None,
            loop_iteration: Some(1),
            verify_exit_code: Some(1),
            context_json: None,
        };
        q.insert_phase_run(&rec).unwrap();

        let code: Option<i64> = q.conn.query_row(
            "SELECT verify_exit_code FROM phase_runs WHERE spec_id = 'S0021'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(code, Some(1), "verify_exit_code=1 (failure) should be stored");

        // Success case
        let rec2 = PhaseRunRecord {
            spec_id: "S0022".to_string(),
            task_id: Some("T0001".to_string()),
            phase: "task-verify".to_string(),
            level: "task".to_string(),
            outcome: "proceed".to_string(),
            duration_ms: Some(500),
            cost_usd: None,
            input_tokens: None,
            output_tokens: None,
            started_at: "2026-04-29T00:00:00Z".to_string(),
            completed_at: Some("2026-04-29T00:00:00Z".to_string()),
            model: None,
            runtime: Some("verify".to_string()),
            pipeline_id: None,
            attempt: 1,
            failure_mode: None,
            cold_start_ms: None,
            inference_ms: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            tool_call_count: None,
            tool_calls_by_type: None,
            ttft_ms: None,
            loop_iteration: Some(1),
            verify_exit_code: Some(0),
            context_json: None,
        };
        q.insert_phase_run(&rec2).unwrap();

        let code2: Option<i64> = q.conn.query_row(
            "SELECT verify_exit_code FROM phase_runs WHERE spec_id = 'S0022'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(code2, Some(0), "verify_exit_code=0 (success) should be stored");
    }

    // --- per_spec_pool tests ---

    fn make_spec_with_pool(title: &str, pool: Option<&str>) -> BoiSpec {
        let mut s = make_spec(title, vec![make_task("t-1", "Task")]);
        s.worker_pool = pool.map(|p| p.to_string());
        s
    }

    #[test]
    fn per_spec_pool_stored_on_enqueue() {
        let q = open_mem();
        let spec = make_spec_with_pool("Pool Spec", Some("fly-runners"));
        let id = q.enqueue(&spec, None).unwrap();
        let st = q.status(&id).unwrap().unwrap();
        assert_eq!(st.spec.worker_pool.as_deref(), Some("fly-runners"));
    }

    #[test]
    fn per_spec_pool_none_for_unset_pool() {
        let q = open_mem();
        let spec = make_spec_with_pool("Default Spec", None);
        let id = q.enqueue(&spec, None).unwrap();
        let st = q.status(&id).unwrap().unwrap();
        assert!(st.spec.worker_pool.is_none());
    }

    #[test]
    fn per_spec_pool_returned_by_dequeue() {
        let q = open_mem();
        let spec = make_spec_with_pool("Dequeue Pool Spec", Some("local"));
        let _id = q.enqueue(&spec, None).unwrap();
        let rec = q.dequeue().unwrap().expect("should dequeue");
        assert_eq!(rec.worker_pool.as_deref(), Some("local"));
    }

    #[test]
    fn per_spec_pool_requeue_reverts_assigning_to_queued() {
        let q = open_mem();
        let spec = make_spec_with_pool("Requeue Spec", Some("fly-runners"));
        let id = q.enqueue(&spec, None).unwrap();

        let rec = q.dequeue().unwrap().expect("should dequeue");
        assert_eq!(rec.status, "assigning");

        q.requeue(&id).unwrap();

        let st = q.status(&id).unwrap().unwrap();
        assert_eq!(st.spec.status, "queued", "requeue must restore queued status");
    }

    #[test]
    fn per_spec_pool_dequeue_for_pools_returns_matching_spec() {
        let q = open_mem();
        let spec = make_spec_with_pool("Fly Spec", Some("fly-runners"));
        let id = q.enqueue(&spec, None).unwrap();

        let rec = q.dequeue_for_pools(&["fly-runners"], "local").unwrap();
        assert!(rec.is_some(), "should dequeue spec requesting available pool");
        assert_eq!(rec.unwrap().id, id);
    }

    #[test]
    fn per_spec_pool_dequeue_for_pools_skips_unavailable_pool() {
        let q = open_mem();
        // Spec wants fly-runners, but only local is available
        let spec = make_spec_with_pool("Fly Spec", Some("fly-runners"));
        q.enqueue(&spec, None).unwrap();

        let rec = q.dequeue_for_pools(&["local"], "local").unwrap();
        assert!(rec.is_none(), "should NOT dequeue spec when its pool is not available");
    }

    #[test]
    fn per_spec_pool_dequeue_for_pools_null_pool_uses_default() {
        let q = open_mem();
        // Spec has no worker_pool → uses default pool ("local")
        let spec = make_spec_with_pool("Default Spec", None);
        let id = q.enqueue(&spec, None).unwrap();

        let rec = q.dequeue_for_pools(&["local"], "local").unwrap();
        assert!(rec.is_some(), "null worker_pool spec should dequeue when default pool is available");
        assert_eq!(rec.unwrap().id, id);
    }

    #[test]
    fn per_spec_pool_dequeue_for_pools_null_pool_blocked_when_default_unavailable() {
        let q = open_mem();
        // Spec has no worker_pool; default is "local" but local is NOT available
        let spec = make_spec_with_pool("Default Spec", None);
        q.enqueue(&spec, None).unwrap();

        let rec = q.dequeue_for_pools(&["fly-runners"], "local").unwrap();
        assert!(rec.is_none(), "null worker_pool spec blocked when default pool unavailable");
    }

    #[test]
    fn per_spec_pool_dequeue_for_pools_empty_available_returns_none() {
        let q = open_mem();
        let spec = make_spec_with_pool("Any Spec", Some("local"));
        q.enqueue(&spec, None).unwrap();

        let rec = q.dequeue_for_pools(&[], "local").unwrap();
        assert!(rec.is_none(), "empty available_pools must return None immediately");
    }

    // Regression tests for S8552+S05C5 double-dispatch incident:
    // second dispatch of the same spec_path while first is queued/running must be a no-op.

    #[test]
    fn test_dedup_skips_duplicate_spec_path_when_queued() {
        let q = open_mem();
        let spec = make_spec("Dedup Spec", vec![make_task("t-1", "T")]);
        let spec_path = "/tmp/boi-test-dedup-spec.yaml";

        // First dispatch: should insert normally.
        let id1 = q.enqueue(&spec, Some(spec_path)).unwrap();
        assert!(is_valid_spec_id(&id1), "first dispatch id={}", id1);

        let count: i64 = q.conn.query_row(
            "SELECT COUNT(*) FROM specs WHERE spec_path = ?1",
            [spec_path],
            |r| r.get::<_, i64>(0),
        ).unwrap();
        assert_eq!(count, 1, "should be exactly 1 spec after first dispatch");

        // Second dispatch of the same spec_path while first is still 'queued' → dedup no-op.
        let id2 = q.enqueue(&spec, Some(spec_path)).unwrap();

        // Dedup returns the existing spec id.
        assert_eq!(id1, id2, "dedup should return the existing spec id, not insert a new one");

        let count2: i64 = q.conn.query_row(
            "SELECT COUNT(*) FROM specs WHERE spec_path = ?1",
            [spec_path],
            |r| r.get::<_, i64>(0),
        ).unwrap();
        assert_eq!(count2, 1, "dedup must not insert a second row; count={}", count2);
    }

    #[test]
    fn test_dedup_skips_when_running() {
        let q = open_mem();
        let spec = make_spec("Dedup Running", vec![make_task("t-1", "T")]);
        let spec_path = "/tmp/boi-test-dedup-running-spec.yaml";

        let id1 = q.enqueue(&spec, Some(spec_path)).unwrap();
        q.update_spec(&id1, "running").unwrap();

        let id2 = q.enqueue(&spec, Some(spec_path)).unwrap();
        assert_eq!(id1, id2, "dedup should return existing id when already running");

        let count: i64 = q.conn.query_row(
            "SELECT COUNT(*) FROM specs WHERE spec_path = ?1",
            [spec_path],
            |r| r.get::<_, i64>(0),
        ).unwrap();
        assert_eq!(count, 1, "no second row when first is running; count={}", count);
    }

    #[test]
    fn test_dedup_allows_dispatch_after_completion() {
        let q = open_mem();
        let spec = make_spec("Dedup Completed", vec![make_task("t-1", "T")]);
        let spec_path = "/tmp/boi-test-dedup-completed-spec.yaml";

        let id1 = q.enqueue(&spec, Some(spec_path)).unwrap();
        q.update_spec(&id1, "completed").unwrap();

        // Completed spec must NOT block a new dispatch of the same spec_path.
        let id2 = q.enqueue(&spec, Some(spec_path)).unwrap();
        assert_ne!(id1, id2, "new dispatch allowed after completion; id2={}", id2);

        let count: i64 = q.conn.query_row(
            "SELECT COUNT(*) FROM specs WHERE spec_path = ?1",
            [spec_path],
            |r| r.get::<_, i64>(0),
        ).unwrap();
        assert_eq!(count, 2, "should have 2 rows (one completed, one queued); count={}", count);
    }

    #[test]
    fn test_stale_task_no_reattachment() {
        let q = open_mem();
        // First dispatch of SPEC-A with two tasks (simulates a prior failed run)
        let spec_a = make_spec(
            "Spec A",
            vec![make_task("TA1", "Old Task A1"), make_task("TA2", "Old Task A2")],
        );
        q.enqueue_with_id("SPEC-A", &spec_a, None, None).unwrap();

        // Simulate a phase_run left behind by the failed first run
        q.conn
            .execute(
                "INSERT INTO phase_runs (spec_id, task_id, phase, level, outcome, started_at, attempt) \
                 VALUES ('SPEC-A', 'TA1', 'execute', 'task', 'failed', '2026-01-01T00:00:00Z', 1)",
                [],
            )
            .unwrap();

        // Verify stale state is present before re-dispatch
        let old_task_count: i64 = q
            .conn
            .query_row("SELECT COUNT(*) FROM tasks WHERE spec_id = 'SPEC-A'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(old_task_count, 2, "setup: expected 2 stale tasks before re-dispatch");

        let old_phase_count: i64 = q
            .conn
            .query_row("SELECT COUNT(*) FROM phase_runs WHERE spec_id = 'SPEC-A'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(old_phase_count, 1, "setup: expected 1 stale phase_run before re-dispatch");

        // Re-dispatch SPEC-A with new spec content (simulating a retry / new iteration)
        let spec_b = make_spec("Spec A v2", vec![make_task("TB1", "New Task B1")]);
        q.enqueue_with_id("SPEC-A", &spec_b, None, None).unwrap();

        // Stale phase_runs must be cleared — orphaned phase history causes 0-iteration failures
        let phase_count_after: i64 = q
            .conn
            .query_row("SELECT COUNT(*) FROM phase_runs WHERE spec_id = 'SPEC-A'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(phase_count_after, 0, "stale phase_runs must be cleared on re-dispatch");

        // Old task TA1 must not persist after re-dispatch
        let stale_ta1: Option<String> = q
            .conn
            .query_row(
                "SELECT id FROM tasks WHERE spec_id = 'SPEC-A' AND id = 'TA1'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert!(stale_ta1.is_none(), "stale task TA1 must not survive re-dispatch");

        // New task TB1 must be present and PENDING
        let new_status: String = q
            .conn
            .query_row(
                "SELECT status FROM tasks WHERE spec_id = 'SPEC-A' AND id = 'TB1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(new_status, "PENDING", "re-dispatched task must start as PENDING");

        // Exactly 1 task for SPEC-A — no stale extras from prior run
        let total_tasks: i64 = q
            .conn
            .query_row("SELECT COUNT(*) FROM tasks WHERE spec_id = 'SPEC-A'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total_tasks, 1, "re-dispatch must produce exactly 1 fresh task, no stale extras");
    }
}

// Regression tests for daemon consistency invariants (H1/H2/H3 from
// docs/diagnostics/2026-05-daemon-consistency.md).
// These tests document BROKEN behaviour and must FAIL until TE1AC lands the fix.
#[cfg(test)]
mod daemon_consistency {
    use super::*;
    use crate::spec::{BoiSpec, BoiTask, TaskStatus};
    use std::collections::HashMap;

    fn minimal_spec(title: &str) -> BoiSpec {
        BoiSpec {
            title: title.to_string(),
            mode: Some("execute".to_string()),
            workspace: None,
            workspace_rationale: Some("test fixture — no repo target".into()),
            initiative: None,
            context: None,
            outcomes: None,
            spec_phases: None,
            task_phases: None,
            context_files: None,
            phase_overrides: HashMap::new(),
            worker_pool: None,
            max_cost_usd: None,
            key_artifacts: None,
            tasks: vec![BoiTask {
                id: "t-1".to_string(),
                title: "Task".to_string(),
                status: TaskStatus::Pending,
                depends: None,
                spec: None,
                verify: None,
                verify_prompt: None,
                phases: None,
            }],
        }
    }

    // H1 — running specs must have worker_id set.
    // Invariant: every spec in status='running' must carry a non-NULL worker_id
    // so that the daemon can detect ghost workers and avoid re-dispatching live work.
    #[test]
    fn running_spec_must_have_worker_id() {
        let q = Queue::open(":memory:").unwrap();
        let spec = minimal_spec("H1-ghost-worker-invariant");
        let spec_id = q.enqueue(&spec, None).unwrap();

        // Reproduce the exact call sequence from worker.rs:
        //   dequeue() → assigning, then update_spec_running(worker_id).
        let assigned = q.dequeue().unwrap().expect("spec should be dequeued");
        assert_eq!(assigned.status, "assigning", "dequeue should set assigning");

        let worker_id = format!("W-{}-test", spec_id);
        q.update_spec_running(&spec_id, &worker_id).unwrap();

        let st = q.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "running", "spec should be running");
        assert_eq!(
            st.spec.worker_id.as_deref(),
            Some(worker_id.as_str()),
            "worker_id must be set on a running spec so ghost-worker detection works",
        );
    }
}

// Integration tests for per-spec pool routing end-to-end (Queue + PoolRegistry).
#[cfg(test)]
mod integration_pool {
    use super::*;
    use crate::config::PoolRegistry;
    use crate::pool::{JobId, JobOutput, JobStatus, WorkerPool};
    use crate::spec::{BoiSpec, BoiTask, TaskStatus};
    use crate::worker::WorkerConfig;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct MockPool {
        name: String,
        spawned: Arc<Mutex<Vec<String>>>,
    }

    impl WorkerPool for MockPool {
        fn spawn(
            &self,
            spec_id: &str,
            _: &str,
            _: &str,
            _: &WorkerConfig,
        ) -> anyhow::Result<JobId> {
            self.spawned.lock().unwrap().push(spec_id.to_string());
            Ok(JobId::new(format!("{}-{}", self.name, spec_id)))
        }
        fn status(&self, _: &JobId) -> anyhow::Result<JobStatus> {
            Ok(JobStatus::Completed)
        }
        fn collect(&self, _: &JobId) -> anyhow::Result<JobOutput> {
            Ok(JobOutput { exit_code: 0, stdout: String::new(), stderr: String::new() })
        }
        fn cancel(&self, _: &JobId) -> anyhow::Result<()> {
            Ok(())
        }
        fn max_workers(&self) -> u32 {
            5
        }
    }

    fn make_registry(
        local_spawned: Arc<Mutex<Vec<String>>>,
        remote_spawned: Arc<Mutex<Vec<String>>>,
    ) -> PoolRegistry {
        let mut pools: HashMap<String, Box<dyn WorkerPool>> = HashMap::new();
        pools.insert(
            "mock-local".to_string(),
            Box::new(MockPool { name: "mock-local".to_string(), spawned: local_spawned }),
        );
        pools.insert(
            "mock-remote".to_string(),
            Box::new(MockPool { name: "mock-remote".to_string(), spawned: remote_spawned }),
        );
        PoolRegistry::from_pools(pools, "mock-local")
    }

    fn make_spec(title: &str, worker_pool: Option<&str>) -> BoiSpec {
        BoiSpec {
            title: title.to_string(),
            mode: Some("execute".to_string()),
            workspace: None,
            workspace_rationale: Some("test fixture — no repo target".into()),
            initiative: None,
            context: None,
            outcomes: None,
            spec_phases: None,
            task_phases: None,
            context_files: None,
            phase_overrides: HashMap::new(),
            worker_pool: worker_pool.map(|p| p.to_string()),
            max_cost_usd: None,
            key_artifacts: None,
            tasks: vec![BoiTask {
                id: "t-1".to_string(),
                title: "Task".to_string(),
                status: TaskStatus::Pending,
                depends: None,
                spec: None,
                verify: None,
                verify_prompt: None,
                phases: None,
            }],
        }
    }

    /// End-to-end: spec with explicit pool → routed to that pool;
    /// spec without pool → routed to registry default.
    #[test]
    fn integration_per_spec_pool() {
        let local_spawned: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let remote_spawned: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let registry = make_registry(local_spawned.clone(), remote_spawned.clone());
        let q = Queue::open(":memory:").unwrap();
        let config = WorkerConfig::default();

        // — Spec 1: explicitly requests mock-remote —
        let remote_spec = make_spec("Remote Spec", Some("mock-remote"));
        let remote_id = q.enqueue(&remote_spec, None).unwrap();

        let rec = q
            .dequeue_for_pools(&["mock-local", "mock-remote"], "mock-local")
            .unwrap()
            .expect("remote spec should dequeue");
        assert_eq!(rec.id, remote_id, "dequeued the remote spec");

        let pool = registry
            .resolve(rec.worker_pool.as_deref())
            .expect("mock-remote must resolve");
        pool.spawn(&rec.id, "", "", &config).unwrap();

        assert_eq!(
            remote_spawned.lock().unwrap().as_slice(),
            [remote_id.as_str()],
            "spec with worker_pool=mock-remote must run on mock-remote",
        );
        assert!(
            local_spawned.lock().unwrap().is_empty(),
            "mock-local must not be used for the remote spec",
        );

        // — Spec 2: no worker_pool → routed to default (mock-local) —
        let default_spec = make_spec("Default Spec", None);
        let default_id = q.enqueue(&default_spec, None).unwrap();

        let rec2 = q
            .dequeue_for_pools(&["mock-local", "mock-remote"], "mock-local")
            .unwrap()
            .expect("default spec should dequeue");
        assert_eq!(rec2.id, default_id, "dequeued the default spec");
        assert!(rec2.worker_pool.is_none(), "no explicit pool set on default spec");

        let pool2 = registry
            .resolve(rec2.worker_pool.as_deref())
            .expect("default pool must resolve");
        pool2.spawn(&rec2.id, "", "", &config).unwrap();

        assert_eq!(
            local_spawned.lock().unwrap().as_slice(),
            [default_id.as_str()],
            "spec without worker_pool must run on mock-local (the default)",
        );
        // remote_spawned still only has the one entry from spec 1
        assert_eq!(
            remote_spawned.lock().unwrap().len(),
            1,
            "mock-remote must not receive the default spec",
        );
    }
}

// Phase 1 distributed-fleet integration tests: local fallback behaviour.
// These verify that when no remote runner claims a spec via the HTTP API,
// specs are processed locally and fleet_status() reports runner_id='local'.
#[cfg(test)]
mod fleet_local_fallback {
    use super::*;
    use crate::spec::{BoiSpec, BoiTask, TaskStatus};
    use std::collections::HashMap;

    fn minimal_spec(title: &str) -> BoiSpec {
        BoiSpec {
            title: title.to_string(),
            mode: Some("execute".to_string()),
            workspace: None,
            workspace_rationale: Some("test fixture — no repo target".into()),
            initiative: None,
            context: None,
            outcomes: None,
            spec_phases: None,
            task_phases: None,
            context_files: None,
            phase_overrides: HashMap::new(),
            worker_pool: None,
            max_cost_usd: None,
            key_artifacts: None,
            tasks: vec![BoiTask {
                id: "t-1".to_string(),
                title: "Task".to_string(),
                status: TaskStatus::Pending,
                depends: None,
                spec: None,
                verify: None,
                verify_prompt: None,
                phases: None,
            }],
        }
    }

    // Specs dequeued locally (no runner claim) complete with runner_id='local'
    // in fleet_status(). This is the single-host fallback path.
    #[test]
    fn local_completion_shows_runner_id_local_in_fleet_status() {
        let q = Queue::open(":memory:").unwrap();
        let spec = minimal_spec("local-fallback-spec");
        let spec_id = q.enqueue(&spec, None).unwrap();

        // Simulate local daemon: dequeue (→ assigning) then mark running, then complete.
        // runner_id is never set, so it stays NULL in the DB.
        let rec = q.dequeue().unwrap().expect("spec should be dequeued");
        assert_eq!(rec.status, "assigning");

        q.update_spec_running(&spec_id, "W-local-test").unwrap();
        q.update_spec(&spec_id, "completed").unwrap();

        let (running, completions) = q.fleet_status().unwrap();
        assert!(running.is_empty(), "no specs should be running after completion");

        assert_eq!(completions.len(), 1, "one completion expected");
        let (runner_id, sid, _title, status) = &completions[0];
        assert_eq!(runner_id, "local", "local execution must show runner_id='local'");
        assert_eq!(sid, &spec_id);
        assert_eq!(status, "completed");
    }

    // A remote runner claiming via claim_for_runner() shows its own runner_id, not 'local'.
    // Confirms the two paths are correctly distinguished.
    #[test]
    fn remote_runner_shows_own_runner_id_in_fleet_status() {
        let q = Queue::open(":memory:").unwrap();
        let spec = minimal_spec("remote-runner-spec");
        let spec_id = q.enqueue(&spec, None).unwrap();

        let _ = q.dequeue().unwrap().expect("spec should be dequeued");
        q.claim_for_runner(&spec_id, "mac-mini-1").unwrap();
        q.complete_from_runner(&spec_id, "completed", Some(0.01), Some(42.0), None)
            .unwrap();

        let (_running, completions) = q.fleet_status().unwrap();
        assert_eq!(completions.len(), 1);
        let (runner_id, sid, _title, status) = &completions[0];
        assert_eq!(runner_id, "mac-mini-1");
        assert_eq!(sid, &spec_id);
        assert_eq!(status, "completed");
    }

    // With multiple specs: some local, some remote — both appear correctly in fleet_status.
    #[test]
    fn mixed_local_and_remote_completions() {
        let q = Queue::open(":memory:").unwrap();

        // Spec 1: local
        let local_id = q.enqueue(&minimal_spec("local-spec"), None).unwrap();
        let _ = q.dequeue().unwrap().expect("local spec dequeued");
        q.update_spec_running(&local_id, "W-local").unwrap();
        q.update_spec(&local_id, "completed").unwrap();

        // Spec 2: remote
        let remote_id = q.enqueue(&minimal_spec("remote-spec"), None).unwrap();
        let _ = q.dequeue().unwrap().expect("remote spec dequeued");
        q.claim_for_runner(&remote_id, "mac-mini-2").unwrap();
        q.complete_from_runner(&remote_id, "completed", None, None, None)
            .unwrap();

        let (_running, completions) = q.fleet_status().unwrap();
        assert_eq!(completions.len(), 2);

        // fleet_status orders by completed_at DESC, so remote (more recent) first
        let ids_and_runners: Vec<(&str, &str)> = completions
            .iter()
            .map(|(r, id, _, _)| (r.as_str(), id.as_str()))
            .collect();

        assert!(
            ids_and_runners.iter().any(|(r, id)| *r == "local" && *id == local_id),
            "local spec must appear with runner_id='local': {:?}",
            ids_and_runners
        );
        assert!(
            ids_and_runners.iter().any(|(r, id)| *r == "mac-mini-2" && *id == remote_id),
            "remote spec must appear with runner_id='mac-mini-2': {:?}",
            ids_and_runners
        );
    }

    // dequeue() still returns queued specs even when no remote runners exist.
    // This verifies the queue itself has no dependency on the HTTP API being present.
    #[test]
    fn dequeue_works_without_http_api() {
        let q = Queue::open(":memory:").unwrap();
        let spec = minimal_spec("no-api-spec");
        let spec_id = q.enqueue(&spec, None).unwrap();

        let rec = q.dequeue().unwrap();
        assert!(rec.is_some(), "dequeue must work without HTTP API running");
        assert_eq!(rec.unwrap().id, spec_id);
    }

    #[test]
    fn runner_register_and_lookup() {
        let q = Queue::open(":memory:").unwrap();
        q.register_runner("mac-mini", "deadbeef01020304", "[]").unwrap();
        let key = q.lookup_runner_key("mac-mini").unwrap();
        assert_eq!(key, Some("deadbeef01020304".to_string()));
    }

    #[test]
    fn runner_lookup_unknown_returns_none() {
        let q = Queue::open(":memory:").unwrap();
        let key = q.lookup_runner_key("nobody").unwrap();
        assert!(key.is_none());
    }

    #[test]
    fn runner_upsert_updates_key() {
        let q = Queue::open(":memory:").unwrap();
        q.register_runner("r1", "key-v1", "[]").unwrap();
        q.register_runner("r1", "key-v2", "[]").unwrap();
        let key = q.lookup_runner_key("r1").unwrap();
        assert_eq!(key, Some("key-v2".to_string()));
    }

    #[test]
    fn runner_touch_updates_last_seen() {
        let q = Queue::open(":memory:").unwrap();
        q.register_runner("heartbeat-runner", "abc", "[]").unwrap();
        q.touch_runner("heartbeat-runner").unwrap();
        // last_seen is set — verify no error and row exists
        let key = q.lookup_runner_key("heartbeat-runner").unwrap();
        assert!(key.is_some());
    }

}
