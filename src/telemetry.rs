use chrono::Utc;
use rand::Rng;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "debug" => LogLevel::Debug,
            "warn" | "warning" => LogLevel::Warn,
            "error" => LogLevel::Error,
            _ => LogLevel::Info,
        }
    }

    fn prefix(&self) -> &'static str {
        match self {
            LogLevel::Debug => "[debug]",
            LogLevel::Info => "[info]",
            LogLevel::Warn => "[warn]",
            LogLevel::Error => "[ERROR]",
        }
    }
}

/// Full telemetry record for one phase invocation.
/// All fields are either real measured/observed values or explicitly null — never fabricated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseInvocation {
    pub invocation_id: String,
    pub spec_id: Option<String>,
    pub task_id: Option<String>,
    pub phase_name: String,
    pub phase_level: String,
    pub mode: Option<String>,
    pub runtime: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget_tokens: Option<i64>,
    pub extended_thinking: Option<bool>,
    pub prompt_template_path: Option<String>,
    pub prompt_length_chars: Option<i64>,
    pub prompt_length_tokens: Option<i64>,
    pub timeout_secs: i64,
    pub bare_flag: bool,
    pub brain_dir: Option<String>,
    pub api_key_env_used: Option<String>,
    pub cli_args: Option<Vec<String>>,
    pub http_endpoint: Option<String>,
    pub started_at: String,
    pub branch_sha: Option<String>,
    pub host_os: Option<String>,
    pub host_arch: Option<String>,
    pub daemon_version: Option<String>,
}

/// Completion-only fields emitted on phase exit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseCompletionFields {
    pub completed_at: String,
    pub duration_ms: i64,
    pub startup_ms: Option<i64>,
    pub inference_ms: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub exit_status: String,
    pub exit_reason: Option<String>,
}

/// A fully-populated row from the phase_runs table (invocation + completion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseRunRecord {
    pub invocation_id: String,
    pub spec_id: Option<String>,
    pub task_id: Option<String>,
    pub phase_name: String,
    pub phase_level: Option<String>,
    pub mode: Option<String>,
    pub runtime: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget_tokens: Option<i64>,
    pub extended_thinking: Option<bool>,
    pub prompt_template_path: Option<String>,
    pub prompt_length_chars: Option<i64>,
    pub prompt_length_tokens: Option<i64>,
    pub timeout_secs: Option<i64>,
    pub bare_flag: bool,
    pub brain_dir: Option<String>,
    pub api_key_env_used: Option<String>,
    pub cli_args: Option<Vec<String>>,
    pub http_endpoint: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub startup_ms: Option<i64>,
    pub inference_ms: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub exit_status: Option<String>,
    pub exit_reason: Option<String>,
    pub retry_index: Option<i64>,
    pub branch_sha: Option<String>,
    pub host_os: Option<String>,
    pub host_arch: Option<String>,
    pub daemon_version: Option<String>,
}

/// Generate a unique invocation ID: `{timestamp_ms}-{random_hex}`.
pub fn generate_invocation_id() -> String {
    let r: u64 = rand::thread_rng().gen();
    let ts = Utc::now().timestamp_millis();
    format!("{}-{:016x}", ts, r)
}

#[derive(Clone)]
pub struct Telemetry {
    pub db_path: PathBuf,
    pub audit_log_path: Option<PathBuf>,
    stderr_level: LogLevel,
    pub conn: Arc<Mutex<Connection>>,
}

#[derive(Debug)]
pub struct TelemetryEvent {
    pub seq: i64,
    pub timestamp: String,
    pub spec_id: Option<String>,
    pub event_type: String,
    pub message: Option<String>,
    pub data: Option<String>,
    pub level: String,
}

impl Telemetry {
    pub fn new(db_path: PathBuf) -> Self {
        let stderr_level = match std::env::var("BOI_LOG_LEVEL") {
            Ok(val) => LogLevel::from_str(&val),
            Err(_) => LogLevel::Error,
        };
        let conn = Connection::open(&db_path).expect("failed to open telemetry DB");
        conn.execute_batch("PRAGMA journal_mode=WAL;").ok();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                spec_id TEXT,
                event_type TEXT NOT NULL,
                message TEXT,
                data TEXT,
                level TEXT DEFAULT 'info'
            );",
        )
        .ok();
        Self::init_phase_runs_table(&conn);
        Telemetry { db_path, audit_log_path: None, stderr_level, conn: Arc::new(Mutex::new(conn)) }
    }

    /// Override the audit log path (useful in tests to avoid writing to ~/.hex).
    pub fn with_audit_log(mut self, path: PathBuf) -> Self {
        self.audit_log_path = Some(path);
        self
    }

    fn init_phase_runs_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS phase_runs (
                invocation_id TEXT PRIMARY KEY,
                spec_id TEXT,
                task_id TEXT,
                phase_name TEXT NOT NULL,
                phase_level TEXT,
                mode TEXT,
                runtime TEXT,
                model TEXT,
                effort TEXT,
                thinking_enabled INTEGER,
                thinking_budget_tokens INTEGER,
                extended_thinking INTEGER,
                prompt_template_path TEXT,
                prompt_length_chars INTEGER,
                prompt_length_tokens INTEGER,
                timeout_secs INTEGER,
                bare_flag INTEGER NOT NULL DEFAULT 0,
                brain_dir TEXT,
                api_key_env_used TEXT,
                cli_args TEXT,
                http_endpoint TEXT,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                duration_ms INTEGER,
                startup_ms INTEGER,
                inference_ms INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_creation_tokens INTEGER,
                cost_usd REAL,
                exit_status TEXT,
                exit_reason TEXT,
                retry_index INTEGER,
                branch_sha TEXT,
                host_os TEXT,
                host_arch TEXT,
                daemon_version TEXT
            );",
        )
        .ok();
    }

    fn resolved_audit_log_path(&self) -> PathBuf {
        self.audit_log_path.clone().unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".hex").join("audit").join("boi-phase-runs.jsonl")
        })
    }

    fn append_to_audit_log(&self, data: &Value) {
        let path = self.resolved_audit_log_path();
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("[boi] ERROR: telemetry audit log dir create failed ({}): {}", parent.display(), e);
            }
        }
        match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut f) => {
                if let Ok(line) = serde_json::to_string(data) {
                    if let Err(e) = writeln!(f, "{}", line) {
                        eprintln!("[boi] ERROR: telemetry audit log write failed ({}): {}", path.display(), e);
                    }
                }
            }
            Err(e) => {
                eprintln!("[boi] ERROR: telemetry audit log open failed ({}): {}", path.display(), e);
            }
        }
    }

    /// Emit `boi.phase.invoked` — called immediately before branching to the runtime.
    /// Writes to: phase_runs table, audit log, stderr.
    pub fn emit_phase_invoked(&self, inv: &PhaseInvocation) {
        // 1. Insert into phase_runs
        let cli_args_json = inv.cli_args.as_ref().and_then(|a| serde_json::to_string(a).ok());
        {
            let conn = match self.conn.lock() {
                Ok(c) => c,
                Err(_) => {
                    eprintln!("[boi] ERROR: telemetry mutex poisoned in emit_phase_invoked ({})", inv.invocation_id);
                    return;
                }
            };
            if let Err(e) = conn.execute(
                "INSERT OR IGNORE INTO phase_runs (
                    invocation_id, spec_id, task_id, phase_name, phase_level,
                    mode, runtime, model, effort,
                    thinking_enabled, thinking_budget_tokens, extended_thinking,
                    prompt_template_path, prompt_length_chars, prompt_length_tokens,
                    timeout_secs, bare_flag, brain_dir, api_key_env_used, cli_args,
                    http_endpoint, started_at, branch_sha, host_os, host_arch, daemon_version
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26
                )",
                params![
                    inv.invocation_id, inv.spec_id, inv.task_id,
                    inv.phase_name, inv.phase_level,
                    inv.mode, inv.runtime, inv.model, inv.effort,
                    inv.thinking_enabled.map(|b| b as i32),
                    inv.thinking_budget_tokens,
                    inv.extended_thinking.map(|b| b as i32),
                    inv.prompt_template_path, inv.prompt_length_chars, inv.prompt_length_tokens,
                    inv.timeout_secs, inv.bare_flag as i32,
                    inv.brain_dir, inv.api_key_env_used, cli_args_json,
                    inv.http_endpoint, inv.started_at,
                    inv.branch_sha, inv.host_os, inv.host_arch, inv.daemon_version
                ],
            ) {
                eprintln!("[boi] ERROR: phase_runs INSERT failed for {}: {}", inv.invocation_id, e);
            }
        } // mutex released here

        // 2. Append to audit log
        let event_data = serde_json::json!({
            "event": "boi.phase.invoked",
            "invocation_id": inv.invocation_id,
            "spec_id": inv.spec_id,
            "task_id": inv.task_id,
            "phase_name": inv.phase_name,
            "phase_level": inv.phase_level,
            "mode": inv.mode,
            "runtime": inv.runtime,
            "model": inv.model,
            "effort": inv.effort,
            "thinking_enabled": inv.thinking_enabled,
            "thinking_budget_tokens": inv.thinking_budget_tokens,
            "extended_thinking": inv.extended_thinking,
            "prompt_template_path": inv.prompt_template_path,
            "prompt_length_chars": inv.prompt_length_chars,
            "prompt_length_tokens": inv.prompt_length_tokens,
            "timeout_secs": inv.timeout_secs,
            "bare_flag": inv.bare_flag,
            "brain_dir": inv.brain_dir,
            "api_key_env_used": inv.api_key_env_used,
            "cli_args": inv.cli_args,
            "http_endpoint": inv.http_endpoint,
            "started_at": inv.started_at,
            "branch_sha": inv.branch_sha,
            "host_os": inv.host_os,
            "host_arch": inv.host_arch,
            "daemon_version": inv.daemon_version,
        });
        self.append_to_audit_log(&event_data);

        // 3. Stderr — always loud for phase lifecycle events
        eprintln!(
            "[boi] [phase.invoked] phase={} spec={} runtime={} model={} timeout={}s inv={}",
            inv.phase_name,
            inv.spec_id.as_deref().unwrap_or("?"),
            inv.runtime.as_deref().unwrap_or("?"),
            inv.model.as_deref().unwrap_or("?"),
            inv.timeout_secs,
            inv.invocation_id,
        );
    }

    /// Emit `boi.phase.completed` — called on every exit path from a phase.
    /// Updates the phase_runs row inserted by emit_phase_invoked.
    pub fn emit_phase_completed(&self, invocation_id: &str, fields: &PhaseCompletionFields) {
        // 1. Update phase_runs row
        {
            let conn = match self.conn.lock() {
                Ok(c) => c,
                Err(_) => {
                    eprintln!("[boi] ERROR: telemetry mutex poisoned in emit_phase_completed ({})", invocation_id);
                    return;
                }
            };
            if let Err(e) = conn.execute(
                "UPDATE phase_runs SET
                    completed_at = ?2, duration_ms = ?3,
                    startup_ms = ?4, inference_ms = ?5,
                    input_tokens = ?6, output_tokens = ?7,
                    cache_read_tokens = ?8, cache_creation_tokens = ?9,
                    cost_usd = ?10, exit_status = ?11, exit_reason = ?12
                 WHERE invocation_id = ?1",
                params![
                    invocation_id,
                    fields.completed_at, fields.duration_ms,
                    fields.startup_ms, fields.inference_ms,
                    fields.input_tokens, fields.output_tokens,
                    fields.cache_read_tokens, fields.cache_creation_tokens,
                    fields.cost_usd, fields.exit_status, fields.exit_reason
                ],
            ) {
                eprintln!("[boi] ERROR: phase_runs UPDATE failed for {}: {}", invocation_id, e);
            }
        } // mutex released here

        // 2. Append to audit log
        let event_data = serde_json::json!({
            "event": "boi.phase.completed",
            "invocation_id": invocation_id,
            "completed_at": fields.completed_at,
            "duration_ms": fields.duration_ms,
            "startup_ms": fields.startup_ms,
            "inference_ms": fields.inference_ms,
            "input_tokens": fields.input_tokens,
            "output_tokens": fields.output_tokens,
            "cache_read_tokens": fields.cache_read_tokens,
            "cache_creation_tokens": fields.cache_creation_tokens,
            "cost_usd": fields.cost_usd,
            "exit_status": fields.exit_status,
            "exit_reason": fields.exit_reason,
        });
        self.append_to_audit_log(&event_data);

        // 3. Stderr — always loud
        eprintln!(
            "[boi] [phase.completed] inv={} exit={} duration={}ms{}",
            invocation_id,
            fields.exit_status,
            fields.duration_ms,
            fields.exit_reason
                .as_deref()
                .map(|r| format!(" reason={}", r))
                .unwrap_or_default(),
        );
    }

    pub fn emit(&self, event_type: &str, level: LogLevel, detail: &Value) {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let now = Utc::now().to_rfc3339();
        let spec_id = detail.get("spec_id").and_then(|v| v.as_str());
        let message = detail.get("message").and_then(|v| v.as_str());
        let data_str = serde_json::to_string(detail).ok();
        let level_str = level.as_str();

        if let Err(e) = conn.execute(
            "INSERT INTO events (timestamp, spec_id, event_type, message, data, level)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now, spec_id, event_type, message, data_str, level_str],
        ) {
            eprintln!("[boi] ERROR: telemetry insert failed for {}: {}", event_type, e);
        }

        if level >= self.stderr_level {
            let msg = message
                .or_else(|| detail.get("task_id").and_then(|v| v.as_str()))
                .unwrap_or(event_type);
            eprintln!("[boi] {} {}: {}", level.prefix(), event_type, msg);
        }
    }

    pub fn recent(&self, limit: usize) -> Vec<TelemetryEvent> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let mut stmt = match conn.prepare(
            "SELECT seq, timestamp, spec_id, event_type, message, data, level
             FROM events ORDER BY seq DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![limit as i64], row_to_event)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn by_spec(&self, spec_id: &str) -> Vec<TelemetryEvent> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let mut stmt = match conn.prepare(
            "SELECT seq, timestamp, spec_id, event_type, message, data, level
             FROM events WHERE spec_id = ?1 ORDER BY seq ASC",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![spec_id], row_to_event)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn by_level(&self, level: LogLevel) -> Vec<TelemetryEvent> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let mut stmt = match conn.prepare(
            "SELECT seq, timestamp, spec_id, event_type, message, data, level
             FROM events WHERE level = ?1 ORDER BY seq DESC",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![level.as_str()], row_to_event)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn by_type(&self, event_type: &str) -> Vec<TelemetryEvent> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let mut stmt = match conn.prepare(
            "SELECT seq, timestamp, spec_id, event_type, message, data, level
             FROM events WHERE event_type = ?1 ORDER BY seq DESC",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![event_type], row_to_event)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn default_db_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(".boi").join("boi-rust.db")
    }

    /// Query all phase_runs rows for a given spec_id, ordered by started_at ASC.
    pub fn phase_runs_by_spec(&self, spec_id: &str) -> Vec<PhaseRunRecord> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let mut stmt = match conn.prepare(
            "SELECT invocation_id, spec_id, task_id, phase_name, phase_level,
                    mode, runtime, model, effort,
                    thinking_enabled, thinking_budget_tokens, extended_thinking,
                    prompt_template_path, prompt_length_chars, prompt_length_tokens,
                    timeout_secs, bare_flag, brain_dir, api_key_env_used, cli_args,
                    http_endpoint, started_at, completed_at, duration_ms,
                    startup_ms, inference_ms, input_tokens, output_tokens,
                    cache_read_tokens, cache_creation_tokens, cost_usd,
                    exit_status, exit_reason, retry_index, branch_sha,
                    host_os, host_arch, daemon_version
             FROM phase_runs WHERE spec_id = ?1 ORDER BY started_at ASC",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![spec_id], |row| {
            Ok(PhaseRunRecord {
                invocation_id: row.get(0)?,
                spec_id: row.get(1)?,
                task_id: row.get(2)?,
                phase_name: row.get(3)?,
                phase_level: row.get(4)?,
                mode: row.get(5)?,
                runtime: row.get(6)?,
                model: row.get(7)?,
                effort: row.get(8)?,
                thinking_enabled: row.get::<_, Option<i32>>(9)?.map(|v| v != 0),
                thinking_budget_tokens: row.get(10)?,
                extended_thinking: row.get::<_, Option<i32>>(11)?.map(|v| v != 0),
                prompt_template_path: row.get(12)?,
                prompt_length_chars: row.get(13)?,
                prompt_length_tokens: row.get(14)?,
                timeout_secs: row.get(15)?,
                bare_flag: row.get::<_, i32>(16)? != 0,
                brain_dir: row.get(17)?,
                api_key_env_used: row.get(18)?,
                cli_args: row.get::<_, Option<String>>(19)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
                http_endpoint: row.get(20)?,
                started_at: row.get(21)?,
                completed_at: row.get(22)?,
                duration_ms: row.get(23)?,
                startup_ms: row.get(24)?,
                inference_ms: row.get(25)?,
                input_tokens: row.get(26)?,
                output_tokens: row.get(27)?,
                cache_read_tokens: row.get(28)?,
                cache_creation_tokens: row.get(29)?,
                cost_usd: row.get(30)?,
                exit_status: row.get(31)?,
                exit_reason: row.get(32)?,
                retry_index: row.get(33)?,
                branch_sha: row.get(34)?,
                host_os: row.get(35)?,
                host_arch: row.get(36)?,
                daemon_version: row.get(37)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Telemetry::new(Self::default_db_path())
    }
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<TelemetryEvent> {
    Ok(TelemetryEvent {
        seq: row.get(0)?,
        timestamp: row.get(1)?,
        spec_id: row.get(2)?,
        event_type: row.get(3)?,
        message: row.get(4)?,
        data: row.get(5)?,
        level: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils;
    use serde_json::json;

    fn temp_db(name: &str) -> PathBuf {
        test_utils::test_file(name, "db")
    }

    #[test]
    fn test_new() {
        let db = temp_db("new");
        let t = Telemetry::new(db.clone());
        assert_eq!(t.db_path, db);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn test_emit_and_recent() {
        let db = temp_db("emit");
        let _ = std::fs::remove_file(&db);

        let t = Telemetry::new(db.clone());
        t.emit("boi.spec.dispatched", LogLevel::Info, &json!({"spec_id": "s0001"}));
        t.emit(
            "boi.task.completed",
            LogLevel::Info,
            &json!({"spec_id": "s0001", "task_id": "t0001"}),
        );

        let events = t.recent(10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "boi.task.completed");
        assert_eq!(events[1].event_type, "boi.spec.dispatched");

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn test_by_spec() {
        let db = temp_db("spec");
        let _ = std::fs::remove_file(&db);

        let t = Telemetry::new(db.clone());
        t.emit("boi.spec.dispatched", LogLevel::Info, &json!({"spec_id": "s0001"}));
        t.emit("boi.spec.dispatched", LogLevel::Info, &json!({"spec_id": "s0002"}));
        t.emit("boi.task.completed", LogLevel::Info, &json!({"spec_id": "s0001"}));

        let events = t.by_spec("s0001");
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.spec_id.as_deref() == Some("s0001")));

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn test_by_type() {
        let db = temp_db("type");
        let _ = std::fs::remove_file(&db);

        let t = Telemetry::new(db.clone());
        t.emit("boi.spec.dispatched", LogLevel::Info, &json!({"spec_id": "s0001"}));
        t.emit("boi.worker.started", LogLevel::Info, &json!({}));
        t.emit("boi.spec.dispatched", LogLevel::Info, &json!({"spec_id": "s0002"}));

        let events = t.by_type("boi.spec.dispatched");
        assert_eq!(events.len(), 2);

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn test_by_level() {
        let db = temp_db("level");
        let _ = std::fs::remove_file(&db);

        let t = Telemetry::new(db.clone());
        t.emit("boi.phase.start", LogLevel::Debug, &json!({"spec_id": "s0001"}));
        t.emit("boi.task.done", LogLevel::Info, &json!({"spec_id": "s0001"}));
        t.emit("boi.verify.fail", LogLevel::Error, &json!({"spec_id": "s0001"}));

        let debug_events = t.by_level(LogLevel::Debug);
        assert_eq!(debug_events.len(), 1);
        assert_eq!(debug_events[0].level, "debug");

        let error_events = t.by_level(LogLevel::Error);
        assert_eq!(error_events.len(), 1);
        assert_eq!(error_events[0].level, "error");

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn test_log_level_from_str() {
        assert_eq!(LogLevel::from_str("debug"), LogLevel::Debug);
        assert_eq!(LogLevel::from_str("info"), LogLevel::Info);
        assert_eq!(LogLevel::from_str("warn"), LogLevel::Warn);
        assert_eq!(LogLevel::from_str("warning"), LogLevel::Warn);
        assert_eq!(LogLevel::from_str("error"), LogLevel::Error);
        assert_eq!(LogLevel::from_str("DEBUG"), LogLevel::Debug);
        assert_eq!(LogLevel::from_str("unknown"), LogLevel::Info);
    }

    #[test]
    fn test_log_level_ordering() {
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
    }

    #[test]
    fn test_emit_stores_level() {
        let db = temp_db("stores_level");
        let _ = std::fs::remove_file(&db);

        let t = Telemetry::new(db.clone());
        t.emit("boi.debug.event", LogLevel::Debug, &json!({"spec_id": "s0001"}));
        t.emit("boi.error.event", LogLevel::Error, &json!({"spec_id": "s0001"}));

        let events = t.recent(10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].level, "error");
        assert_eq!(events[1].level, "debug");

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn test_emit_100_events_under_1_second() {
        // RED: with per-emit connections this loop should exceed 200ms on any
        // real filesystem (each emit opens connection + WAL pragma + schema check)
        let db = temp_db("perf_100");
        let _ = std::fs::remove_file(&db);

        let t = Telemetry::new(db.clone());
        let start = std::time::Instant::now();
        for i in 0..1000 {
            t.emit("boi.perf.test", LogLevel::Info, &json!({"i": i}));
        }
        let elapsed = start.elapsed();

        let _ = std::fs::remove_file(&db);
        assert!(
            elapsed.as_millis() < 1000,
            "1000 emits took {}ms — expected < 1000ms; per-emit connections are the likely cause",
            elapsed.as_millis()
        );
    }

    /// Verify that emit_phase_invoked writes to the phase_runs table and audit log,
    /// and emit_phase_completed updates the row with completion fields.
    #[test]
    fn phase_logging_emit() {
        let db = test_utils::test_file("phase_logging_emit", "db");
        let audit_log = test_utils::test_file("phase_logging_emit_audit", "jsonl");
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(&audit_log);

        let t = Telemetry::new(db.clone()).with_audit_log(audit_log.clone());

        let inv = PhaseInvocation {
            invocation_id: "test-inv-001".to_string(),
            spec_id: Some("spec-001".to_string()),
            task_id: Some("task-001".to_string()),
            phase_name: "execute".to_string(),
            phase_level: "task".to_string(),
            mode: None,
            runtime: Some("claude".to_string()),
            model: Some("claude-opus-4-7".to_string()),
            effort: Some("high".to_string()),
            thinking_enabled: None,
            thinking_budget_tokens: None,
            extended_thinking: None,
            prompt_template_path: None,
            prompt_length_chars: Some(1500),
            prompt_length_tokens: Some(375),
            timeout_secs: 300,
            bare_flag: false,
            brain_dir: None,
            api_key_env_used: Some("ANTHROPIC_API_KEY".to_string()),
            cli_args: Some(vec!["--dangerously-skip-permissions".to_string()]),
            http_endpoint: None,
            started_at: "2026-04-29T00:00:00Z".to_string(),
            branch_sha: Some("abc123def456".to_string()),
            host_os: Some("macos".to_string()),
            host_arch: Some("aarch64".to_string()),
            daemon_version: Some("1.1.0".to_string()),
        };

        t.emit_phase_invoked(&inv);

        // Verify row was inserted into phase_runs
        {
            let conn = t.conn.lock().unwrap();
            let row: (String, Option<String>, Option<String>, String, Option<String>, Option<String>) =
                conn.query_row(
                    "SELECT invocation_id, spec_id, task_id, phase_name, runtime, model
                     FROM phase_runs WHERE invocation_id = ?1",
                    rusqlite::params!["test-inv-001"],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .expect("row should exist after emit_phase_invoked");

            assert_eq!(row.0, "test-inv-001");
            assert_eq!(row.1, Some("spec-001".to_string()));
            assert_eq!(row.2, Some("task-001".to_string()));
            assert_eq!(row.3, "execute");
            assert_eq!(row.4, Some("claude".to_string()));
            assert_eq!(row.5, Some("claude-opus-4-7".to_string()));

            // Completion fields should be null at this point
            let exit_status: Option<String> = conn
                .query_row(
                    "SELECT exit_status FROM phase_runs WHERE invocation_id = ?1",
                    rusqlite::params!["test-inv-001"],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exit_status.is_none(), "exit_status should be null before completion");
        }

        // Emit completion
        let completion = PhaseCompletionFields {
            completed_at: "2026-04-29T00:05:00Z".to_string(),
            duration_ms: 300_000,
            startup_ms: Some(2000),
            inference_ms: Some(298_000),
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            cost_usd: None,
            exit_status: "success".to_string(),
            exit_reason: None,
        };
        t.emit_phase_completed("test-inv-001", &completion);

        // Verify completion fields were written
        {
            let conn = t.conn.lock().unwrap();
            let (exit_status, duration_ms, completed_at, startup_ms): (
                Option<String>,
                Option<i64>,
                Option<String>,
                Option<i64>,
            ) = conn
                .query_row(
                    "SELECT exit_status, duration_ms, completed_at, startup_ms
                     FROM phase_runs WHERE invocation_id = ?1",
                    rusqlite::params!["test-inv-001"],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .expect("row should still exist after emit_phase_completed");

            assert_eq!(exit_status, Some("success".to_string()));
            assert_eq!(duration_ms, Some(300_000i64));
            assert_eq!(completed_at, Some("2026-04-29T00:05:00Z".to_string()));
            assert_eq!(startup_ms, Some(2000i64));
        }

        // Verify audit log has exactly 2 JSONL lines
        let audit_content =
            std::fs::read_to_string(&audit_log).expect("audit log should exist after emit");
        let lines: Vec<&str> = audit_content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "should have one invoked + one completed line in audit log");

        let event0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let event1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(event0["event"], "boi.phase.invoked");
        assert_eq!(event1["event"], "boi.phase.completed");
        assert_eq!(event0["invocation_id"], "test-inv-001");
        assert_eq!(event1["invocation_id"], "test-inv-001");
        assert_eq!(event0["phase_name"], "execute");
        assert_eq!(event1["exit_status"], "success");

        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(&audit_log);
    }
}
