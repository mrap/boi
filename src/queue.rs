use chrono::Utc;
use rand::Rng;
use rusqlite::{params, Connection, Result};

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
    /// Remote dispatch provider for this spec ("fly" or None for local).
    pub remote: Option<String>,
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
        Self::ensure_column(&conn, "specs", "remote", "TEXT");
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

        Self::ensure_column(&conn, "bench_results", "total_cost_usd", "REAL");
        Self::ensure_column(&conn, "bench_results", "total_input_tokens", "INTEGER");
        Self::ensure_column(&conn, "bench_results", "total_output_tokens", "INTEGER");
        Self::ensure_column(&conn, "bench_results", "tasks_skipped", "INTEGER DEFAULT 0");

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
        let tx = self.conn.unchecked_transaction()?;

        let id = gen_id('S', &tx);

        let now = Utc::now().to_rfc3339();
        let mode = spec.mode.as_deref().unwrap_or("execute");
        let total = spec.tasks.len() as i64;

        tx.execute(
            "INSERT INTO specs (id, title, mode, status, spec_path, total_tasks, queued_at, context, workspace, project_context)
             VALUES (?1, ?2, ?3, 'queued', ?4, ?5, ?6, ?7, ?8, ?9)",
            params![id, spec.title, mode, spec_path, total, now, spec.context, spec.workspace, project_context],
        )?;

        let mut yaml_to_canonical: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for task in spec.tasks.iter() {
            let canonical_task_id = gen_id('T', &tx);
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
            tx.execute(
                "INSERT INTO tasks (id, spec_id, title, status, depends, spec_content, verify_content)
                 VALUES (?1, ?2, ?3, 'PENDING', ?4, ?5, ?6)",
                params![canonical_task_id, id, task.title, depends_json, task.spec, task.verify],
            )?;
        }

        tx.commit()?;
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
                        context, workspace, phase_loop_count, project_context, remote
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
            "inconclusive" => {
                // When a diagnosis is available, callers use update_spec_with_error to also
                // set the error column. This arm handles the no-diagnosis fallback path.
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

    pub fn update_spec_with_error(&self, spec_id: &str, status: &str, error: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE specs SET status = ?1, completed_at = ?2, error = ?3 WHERE id = ?4",
            params![status, now, error, spec_id],
        )?;
        Ok(())
    }

    pub fn status(&self, spec_id: &str) -> Result<Option<SpecStatus>> {
        let spec = match self.conn.query_row(
            "SELECT id, title, mode, status, spec_path,
                    (SELECT COUNT(*) FROM tasks WHERE tasks.spec_id = specs.id) as total_tasks,
                    completed_tasks,
                    priority, depends_on, queued_at, started_at, completed_at, worker_id, error,
                    max_iterations, iteration, project, phase, worker_timeout_seconds,
                    context, workspace, phase_loop_count, project_context, remote
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
                    context, workspace, phase_loop_count, project_context, remote
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

    pub fn set_remote(&self, spec_id: &str, remote: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE specs SET remote = ?1 WHERE id = ?2",
            params![remote, spec_id],
        )?;
        Ok(())
    }

    pub fn get_spec_remote(&self, spec_id: &str) -> Result<Option<String>> {
        match self.conn.query_row(
            "SELECT remote FROM specs WHERE id = ?1",
            params![spec_id],
            |row| row.get(0),
        ) {
            Ok(r) => Ok(r),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
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
             tool_call_count, tool_calls_by_type, ttft_ms, loop_iteration, verify_exit_code)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)",
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
        remote: row.get(23)?,
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
            initiative: None,
            context: None,
            outcomes: None,
            spec_phases: None,
            task_phases: None,
            context_files: None,
            hypothesis: None,
            success_criteria: None,
            key_artifacts: None,
            preconditions: None,
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
            containerized: None,
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
        };
        q.insert_phase_run(&rec2).unwrap();

        let code2: Option<i64> = q.conn.query_row(
            "SELECT verify_exit_code FROM phase_runs WHERE spec_id = 'S0022'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(code2, Some(0), "verify_exit_code=0 (success) should be stored");
    }
}
