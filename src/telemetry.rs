use chrono::Utc;
use rusqlite::{params, Connection, Result};
use serde_json::Value;
use std::path::PathBuf;

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

#[derive(Clone)]
pub struct Telemetry {
    pub db_path: PathBuf,
    stderr_level: LogLevel,
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
        Telemetry { db_path, stderr_level }
    }

    fn open_conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
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
        )?;
        Ok(conn)
    }

    pub fn emit(&self, event_type: &str, level: LogLevel, detail: &Value) {
        let conn = match self.open_conn() {
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
        let conn = match self.open_conn() {
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
        let conn = match self.open_conn() {
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
        let conn = match self.open_conn() {
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
        let conn = match self.open_conn() {
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
        t.emit("boi.spec.dispatched", LogLevel::Info, &json!({"spec_id": "q-001"}));
        t.emit(
            "boi.task.completed",
            LogLevel::Info,
            &json!({"spec_id": "q-001", "task_id": "t-1"}),
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
        t.emit("boi.spec.dispatched", LogLevel::Info, &json!({"spec_id": "q-001"}));
        t.emit("boi.spec.dispatched", LogLevel::Info, &json!({"spec_id": "q-002"}));
        t.emit("boi.task.completed", LogLevel::Info, &json!({"spec_id": "q-001"}));

        let events = t.by_spec("q-001");
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.spec_id.as_deref() == Some("q-001")));

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn test_by_type() {
        let db = temp_db("type");
        let _ = std::fs::remove_file(&db);

        let t = Telemetry::new(db.clone());
        t.emit("boi.spec.dispatched", LogLevel::Info, &json!({"spec_id": "q-001"}));
        t.emit("boi.worker.started", LogLevel::Info, &json!({}));
        t.emit("boi.spec.dispatched", LogLevel::Info, &json!({"spec_id": "q-002"}));

        let events = t.by_type("boi.spec.dispatched");
        assert_eq!(events.len(), 2);

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn test_by_level() {
        let db = temp_db("level");
        let _ = std::fs::remove_file(&db);

        let t = Telemetry::new(db.clone());
        t.emit("boi.phase.start", LogLevel::Debug, &json!({"spec_id": "q-001"}));
        t.emit("boi.task.done", LogLevel::Info, &json!({"spec_id": "q-001"}));
        t.emit("boi.verify.fail", LogLevel::Error, &json!({"spec_id": "q-001"}));

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
        t.emit("boi.debug.event", LogLevel::Debug, &json!({"spec_id": "q-001"}));
        t.emit("boi.error.event", LogLevel::Error, &json!({"spec_id": "q-001"}));

        let events = t.recent(10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].level, "error");
        assert_eq!(events[1].level, "debug");

        let _ = std::fs::remove_file(&db);
    }
}
