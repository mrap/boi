use crate::hooks::HookEntry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Paths {
    pub db: Option<String>,
    pub telemetry: Option<String>,
    pub worktrees: Option<String>,
    pub logs: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    pub max_workers: Option<u32>,
    pub task_timeout_minutes: Option<u32>,
    pub retry_count: Option<u32>,
    pub hooks: Option<HashMap<String, HookEntry>>,
    pub paths: Option<Paths>,
}

pub fn load() -> Config {
    let config_path = default_config_path();
    Config::load_from(&config_path)
}

pub fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".boi").join("config.yaml")
}

impl Config {
    pub fn load_from(path: &Path) -> Self {
        if path.exists() {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            match serde_yml::from_str::<Config>(&content) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!(
                        "ERROR: config parse failed at {}: {}",
                        path.display(),
                        e
                    );
                    eprintln!("Using defaults. Fix the config file or delete it.");
                    Config::default()
                }
            }
        } else {
            Config::default()
        }
    }

    pub fn max_workers(&self) -> u32 {
        self.max_workers.unwrap_or(5)
    }

    pub fn task_timeout_secs(&self) -> u64 {
        self.task_timeout_minutes.unwrap_or(30) as u64 * 60
    }

    pub fn retry_count(&self) -> u32 {
        self.retry_count.unwrap_or(3)
    }

    pub fn db_path(&self) -> PathBuf {
        if let Some(p) = self.paths.as_ref().and_then(|p| p.db.as_ref()) {
            PathBuf::from(p)
        } else {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".boi").join("boi.db")
        }
    }

    pub fn telemetry_path(&self) -> PathBuf {
        if let Some(p) = self.paths.as_ref().and_then(|p| p.telemetry.as_ref()) {
            PathBuf::from(p)
        } else {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home)
                .join(".boi")
                .join("telemetry")
                .join("boi.jsonl")
        }
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        if let Some(p) = self.paths.as_ref().and_then(|p| p.worktrees.as_ref()) {
            PathBuf::from(p)
        } else {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".boi").join("worktrees")
        }
    }

    pub fn logs_dir(&self) -> PathBuf {
        if let Some(p) = self.paths.as_ref().and_then(|p| p.logs.as_ref()) {
            PathBuf::from(p)
        } else {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".boi").join("logs")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.max_workers(), 5);
        assert_eq!(cfg.task_timeout_secs(), 30 * 60);
        assert_eq!(cfg.retry_count(), 3);
    }

    #[test]
    fn test_load_from_missing_file() {
        let cfg = Config::load_from(Path::new("/tmp/boi-nonexistent-config-xyz.yaml"));
        assert_eq!(cfg.max_workers(), 5);
    }

    #[test]
    fn test_load_from_yaml() {
        let path = PathBuf::from(format!("/tmp/boi-test-config-{}.yaml", std::process::id()));
        let yaml = "max_workers: 3\ntask_timeout_minutes: 10\nretry_count: 1\n";
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let cfg = Config::load_from(&path);
        assert_eq!(cfg.max_workers(), 3);
        assert_eq!(cfg.task_timeout_secs(), 600);
        assert_eq!(cfg.retry_count(), 1);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_default_paths() {
        let cfg = Config::default();
        assert!(cfg.db_path().to_str().unwrap().contains("boi.db"));
        assert!(cfg.telemetry_path().to_str().unwrap().ends_with("boi.jsonl"));
        assert!(cfg.worktrees_dir().to_str().unwrap().ends_with("worktrees"));
        assert!(cfg.logs_dir().to_str().unwrap().ends_with("logs"));
    }

    #[test]
    fn test_custom_paths_via_yaml() {
        let path = PathBuf::from(format!(
            "/tmp/boi-test-config-paths-{}.yaml",
            std::process::id()
        ));
        let yaml = "paths:\n  db: /custom/boi.db\n  telemetry: /custom/t.jsonl\n";
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let cfg = Config::load_from(&path);
        assert_eq!(cfg.db_path(), PathBuf::from("/custom/boi.db"));
        assert_eq!(cfg.telemetry_path(), PathBuf::from("/custom/t.jsonl"));

        let _ = fs::remove_file(&path);
    }
}
