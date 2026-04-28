use chrono::Utc;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub struct Telemetry {
    pub path: PathBuf,
}

impl Telemetry {
    pub fn new(path: PathBuf) -> Self {
        Telemetry { path }
    }

    /// Append-only JSONL entry: {ts, event, detail}
    pub fn record(&self, event: &str, detail: &Value) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let entry = json!({
            "ts": Utc::now().to_rfc3339(),
            "event": event,
            "detail": detail,
        });
        let mut line = serde_json::to_string(&entry).unwrap_or_default();
        line.push('\n');

        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }

    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home)
            .join(".boi")
            .join("telemetry")
            .join("boi.jsonl")
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Telemetry::new(Self::default_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn test_new() {
        let t = Telemetry::new(PathBuf::from("/tmp/test.jsonl"));
        assert_eq!(t.path, PathBuf::from("/tmp/test.jsonl"));
    }

    #[test]
    fn test_record_appends_jsonl() {
        let path = PathBuf::from(format!("/tmp/boi-test-telemetry-{}.jsonl", std::process::id()));
        let _ = fs::remove_file(&path);

        let t = Telemetry::new(path.clone());
        t.record("boi.spec.dispatched", &json!({"spec_id": "q-001"}));
        t.record("boi.task.completed", &json!({"spec_id": "q-001", "task_id": "t-1"}));

        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        let entry: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry["event"], "boi.spec.dispatched");
        assert!(entry["ts"].is_string());
        assert_eq!(entry["detail"]["spec_id"], "q-001");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_record_creates_parent_dirs() {
        let path = PathBuf::from(format!(
            "/tmp/boi-test-{}/nested/boi.jsonl",
            std::process::id()
        ));
        let t = Telemetry::new(path.clone());
        t.record("boi.worker.started", &json!({}));
        assert!(path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }
}
