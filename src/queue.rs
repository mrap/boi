use chrono::Utc;
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
pub struct EventRecord {
    pub seq: i64,
    pub timestamp: String,
    pub spec_id: Option<String>,
    pub event_type: String,
    pub message: Option<String>,
    pub data: Option<String>,
    pub level: String,
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
pub struct ProcessRecord {
    pub pid: Option<i64>,
    pub spec_id: String,
    pub worker_id: Option<String>,
    pub iteration: Option<i64>,
    pub phase: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub exit_code: Option<i64>,
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

impl Queue {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;

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
                phase TEXT DEFAULT 'execute'
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
            CREATE INDEX IF NOT EXISTS idx_phase_runs_phase ON phase_runs(phase);",
        )?;

        // Migrate existing specs tables that lack new columns
        Self::ensure_column(&conn, "specs", "max_iterations", "INTEGER DEFAULT 30");
        Self::ensure_column(&conn, "specs", "iteration", "INTEGER DEFAULT 0");
        Self::ensure_column(&conn, "specs", "project", "TEXT");
        Self::ensure_column(&conn, "specs", "phase", "TEXT DEFAULT 'execute'");
        Self::ensure_column(&conn, "specs", "worker_timeout_seconds", "INTEGER");
        Self::ensure_column(&conn, "tasks", "spec_content", "TEXT");
        Self::ensure_column(&conn, "tasks", "verify_content", "TEXT");

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
            let _ = conn.execute(&sql, []);
        }
    }

    pub fn enqueue(
        &self,
        spec: &crate::spec::BoiSpec,
        spec_path: Option<&str>,
    ) -> Result<String> {
        let tx = self.conn.unchecked_transaction()?;

        let max_n: Option<i64> = tx
            .query_row(
                "SELECT MAX(CAST(SUBSTR(id, 3) AS INTEGER)) FROM specs WHERE id LIKE 'q-%'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(None);
        let n = max_n.map(|n| n + 1).unwrap_or(1);
        let id = format!("q-{}", n);

        let now = Utc::now().to_rfc3339();
        let mode = spec.mode.as_deref().unwrap_or("execute");
        let total = spec.tasks.len() as i64;

        tx.execute(
            "INSERT INTO specs (id, title, mode, status, spec_path, total_tasks, queued_at)
             VALUES (?1, ?2, ?3, 'queued', ?4, ?5, ?6)",
            params![id, spec.title, mode, spec_path, total, now],
        )?;

        for task in &spec.tasks {
            let depends_json = serde_json::to_string(task.depends.as_deref().unwrap_or(&[]))
                .unwrap_or_else(|_| "[]".to_string());
            tx.execute(
                "INSERT INTO tasks (id, spec_id, title, status, depends, spec_content, verify_content)
                 VALUES (?1, ?2, ?3, 'PENDING', ?4, ?5, ?6)",
                params![task.id, id, task.title, depends_json, task.spec, task.verify],
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
                "SELECT id, title, mode, status, spec_path, total_tasks, completed_tasks,
                        priority, depends_on, queued_at, started_at, completed_at, worker_id, error,
                        max_iterations, iteration, project, phase, worker_timeout_seconds
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
            "SELECT id, title, mode, status, spec_path, total_tasks, completed_tasks,
                    priority, depends_on, queued_at, started_at, completed_at, worker_id, error,
                    max_iterations, iteration, project, phase, worker_timeout_seconds
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
            "SELECT id, title, mode, status, spec_path, total_tasks, completed_tasks,
                    priority, depends_on, queued_at, started_at, completed_at, worker_id, error,
                    max_iterations, iteration, project, phase, worker_timeout_seconds
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

    /// Resume a paused spec by resetting its status to "queued".
    pub fn resume_spec(&self, spec_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE specs SET status = 'queued' WHERE id = ?1 AND status = 'paused'",
            params![spec_id],
        )?;
        Ok(())
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

    pub fn increment_iteration(&self, spec_id: &str) -> Result<i64> {
        self.conn.execute(
            "UPDATE specs SET iteration = iteration + 1 WHERE id = ?1",
            params![spec_id],
        )?;
        let iter: i64 = self.conn.query_row(
            "SELECT iteration FROM specs WHERE id = ?1",
            params![spec_id],
            |row| row.get(0),
        )?;
        Ok(iter)
    }

    // --- Iteration records ---

    pub fn insert_iteration(&self, rec: &IterationRecord) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO iterations
             (spec_id, iteration, phase, worker_id, started_at, ended_at,
              duration_seconds, tasks_completed, tasks_added, exit_code)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                rec.spec_id,
                rec.iteration,
                rec.phase,
                rec.worker_id,
                rec.started_at,
                rec.ended_at,
                rec.duration_seconds,
                rec.tasks_completed,
                rec.tasks_added,
                rec.exit_code,
            ],
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

    pub fn get_events(&self, spec_id: Option<&str>, limit: usize) -> Result<Vec<EventRecord>> {
        let (sql, p_spec_id);
        if let Some(sid) = spec_id {
            sql = "SELECT seq, timestamp, spec_id, event_type, message, data, level
                   FROM events WHERE spec_id = ?1 ORDER BY seq DESC LIMIT ?2";
            p_spec_id = Some(sid.to_string());
        } else {
            sql = "SELECT seq, timestamp, spec_id, event_type, message, data, level
                   FROM events ORDER BY seq DESC LIMIT ?1";
            p_spec_id = None;
        }

        let mut stmt = self.conn.prepare(sql)?;
        let rows = if let Some(ref sid) = p_spec_id {
            stmt.query_map(params![sid, limit as i64], row_to_event)?
                .collect::<Result<Vec<_>>>()?
        } else {
            stmt.query_map(params![limit as i64], row_to_event)?
                .collect::<Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    // --- Worker records ---

    pub fn upsert_worker(&self, rec: &WorkerRecord) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO workers
             (id, worktree_path, current_spec_id, current_pid, start_time, current_phase, current_task_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                rec.id,
                rec.worktree_path,
                rec.current_spec_id,
                rec.current_pid,
                rec.start_time,
                rec.current_phase,
                rec.current_task_id,
            ],
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

    pub fn clear_worker(&self, worker_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE workers SET current_spec_id = NULL, current_pid = NULL,
                    current_phase = NULL, current_task_id = NULL
             WHERE id = ?1",
            params![worker_id],
        )?;
        Ok(())
    }

    // --- Process records ---

    pub fn insert_process(&self, rec: &ProcessRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO processes (pid, spec_id, worker_id, iteration, phase, started_at, ended_at, exit_code)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                rec.pid,
                rec.spec_id,
                rec.worker_id,
                rec.iteration,
                rec.phase,
                rec.started_at,
                rec.ended_at,
                rec.exit_code,
            ],
        )?;
        Ok(())
    }

    // --- Phase run records ---

    pub fn insert_phase_run(&self, rec: &PhaseRunRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO phase_runs (spec_id, task_id, phase, level, outcome,
             duration_ms, cost_usd, input_tokens, output_tokens, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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

    // --- Task management ---

    pub fn add_task(
        &self,
        spec_id: &str,
        task_id: &str,
        title: &str,
        spec_text: Option<&str>,
        verify: Option<&str>,
        depends: &[String],
    ) -> Result<()> {
        let depends_json = serde_json::to_string(depends).unwrap_or_else(|_| "[]".to_string());
        self.conn.execute(
            "INSERT INTO tasks (id, spec_id, title, status, depends, spec_content, verify_content)
             VALUES (?1, ?2, ?3, 'PENDING', ?4, ?5, ?6)",
            params![task_id, spec_id, title, depends_json, spec_text, verify],
        )?;
        // Update total_tasks count
        self.conn.execute(
            "UPDATE specs SET total_tasks = (SELECT COUNT(*) FROM tasks WHERE spec_id = ?1) WHERE id = ?1",
            params![spec_id],
        )?;
        // Also log the addition as an event for audit trail
        let _ = self.insert_event(
            Some(spec_id),
            "task.added",
            Some(&format!("Added task {} to {}", task_id, spec_id)),
            None,
            "info",
        );
        Ok(())
    }

    pub fn skip_task(&self, spec_id: &str, task_id: &str) -> Result<()> {
        self.update_task(spec_id, task_id, "SKIPPED")
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
            "SELECT id, spec_id, title, status, depends, started_at, completed_at, error FROM tasks WHERE spec_id = ?1",
        )?;
        let rows = stmt.query_map(params![spec_id], row_to_task)?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    }

    /// Reset any specs stuck in 'running' or 'assigning' back to 'queued'.
    /// Called on daemon startup to recover from crashes.
    pub fn recover_stuck_specs(&self) -> Result<usize> {
        self.conn.execute(
            "UPDATE specs SET status = 'queued' WHERE status IN ('running', 'assigning')",
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

    /// Get lifetime totals for failed and completed specs across all history
    pub fn lifetime_stats(&self) -> Result<(i64, i64)> {
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
    })
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    Ok(EventRecord {
        seq: row.get(0)?,
        timestamp: row.get(1)?,
        spec_id: row.get(2)?,
        event_type: row.get(3)?,
        message: row.get(4)?,
        data: row.get(5)?,
        level: row.get(6)?,
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

    #[test]
    fn test_enqueue_returns_id() {
        let q = open_mem();
        let spec = make_spec("My Spec", vec![make_task("t-1", "Setup")]);
        let id = q.enqueue(&spec, None).unwrap();
        assert!(id.starts_with("q-"), "id={}", id);
    }

    #[test]
    fn test_sequential_ids() {
        let q = open_mem();
        let spec = make_spec("S1", vec![make_task("t-1", "A")]);
        let id1 = q.enqueue(&spec, None).unwrap();
        let id2 = q.enqueue(&spec, None).unwrap();
        assert_eq!(id1, "q-1");
        assert_eq!(id2, "q-2");
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
        q.update_task(&id, "t-1", "DONE").unwrap();
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
        assert!(q.status("q-999").unwrap().is_none());
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
    fn test_tasks_stored_on_enqueue() {
        let q = open_mem();
        let spec = make_spec(
            "S",
            vec![make_task("t-1", "First"), make_task("t-2", "Second")],
        );
        let id = q.enqueue(&spec, None).unwrap();
        let st = q.status(&id).unwrap().unwrap();
        assert_eq!(st.tasks.len(), 2);
        assert_eq!(st.tasks[0].id, "t-1");
        assert_eq!(st.tasks[1].id, "t-2");
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
}
