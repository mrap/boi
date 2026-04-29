use crate::hooks::HookEntry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Paths {
    pub db: Option<String>,
    pub worktrees: Option<String>,
    pub logs: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    pub max_workers: Option<u32>,
    pub spawns_per_tick: Option<u32>,
    pub task_timeout_minutes: Option<u32>,
    pub retry_count: Option<u32>,
    pub cleanup_on_failure: Option<bool>,
    pub hooks: Option<HashMap<String, HookEntry>>,
    pub paths: Option<Paths>,
    pub claude_bin: Option<String>,
    pub brain: Option<PathBuf>,
}

/// Resolve brain directory: spec-level overrides config-level, falls back to None.
pub fn resolve_brain(
    spec_brain: Option<&PathBuf>,
    config_brain: Option<&PathBuf>,
) -> Option<PathBuf> {
    spec_brain.or(config_brain).cloned()
}

/// Validate that the brain path exists and contains CLAUDE.md.
pub fn validate_brain(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("brain directory not found: {}", path.display()));
    }
    let claude_md = path.join("CLAUDE.md");
    if !claude_md.exists() {
        return Err(format!(
            "brain directory missing CLAUDE.md: {}",
            path.display()
        ));
    }
    Ok(())
}

pub fn load() -> Config {
    let config_path = default_config_path();
    Config::load_from(&config_path)
}

/// Fallible load — returns Err on parse failure rather than silently defaulting.
/// Used by SIGHUP hot-reload so a bad config file is a no-op instead of a reset.
pub fn try_load() -> Result<Config, String> {
    try_load_from(&default_config_path())
}

pub fn try_load_from(path: &std::path::Path) -> Result<Config, String> {
    if path.exists() {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read config {}: {}", path.display(), e))?;
        serde_yml::from_str::<Config>(&content)
            .map_err(|e| format!("config parse error in {}: {}", path.display(), e))
    } else {
        Ok(Config::default())
    }
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

    pub fn spawns_per_tick(&self) -> u32 {
        self.spawns_per_tick.unwrap_or(4)
    }

    pub fn task_timeout_secs(&self) -> u64 {
        self.task_timeout_minutes.unwrap_or(30) as u64 * 60
    }

    pub fn retry_count(&self) -> u32 {
        self.retry_count.unwrap_or(3)
    }

    pub fn cleanup_on_failure(&self) -> bool {
        self.cleanup_on_failure.unwrap_or(false)
    }

    pub fn claude_bin(&self) -> String {
        if let Some(ref bin) = self.claude_bin {
            return bin.clone();
        }
        std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
    }

    pub fn db_path(&self) -> PathBuf {
        if let Some(p) = self.paths.as_ref().and_then(|p| p.db.as_ref()) {
            PathBuf::from(p)
        } else {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".boi").join("boi-rust.db")
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
    use crate::test_utils;
    use std::fs;
    use std::io::Write;

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.max_workers(), 5);
        assert_eq!(cfg.task_timeout_secs(), 30 * 60);
        assert_eq!(cfg.retry_count(), 3);
        assert_eq!(cfg.spawns_per_tick(), 4);
    }

    #[test]
    fn test_spawns_per_tick_default() {
        let cfg = Config::default();
        assert_eq!(cfg.spawns_per_tick(), 4);
    }

    #[test]
    fn test_spawns_per_tick_explicit() {
        let path = test_utils::test_file("config-spawns", "yaml");
        let yaml = "spawns_per_tick: 8\n";
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let cfg = Config::load_from(&path);
        assert_eq!(cfg.spawns_per_tick(), 8);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_load_from_missing_file() {
        let path = test_utils::test_file("nonexistent", "yaml");
        let _ = fs::remove_file(&path);
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.max_workers(), 5);
    }

    #[test]
    fn test_load_from_yaml() {
        let path = test_utils::test_file("config", "yaml");
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
        assert!(cfg.db_path().to_str().unwrap().contains("boi-rust.db"));
        assert!(cfg.worktrees_dir().to_str().unwrap().ends_with("worktrees"));
        assert!(cfg.logs_dir().to_str().unwrap().ends_with("logs"));
    }

    #[test]
    fn test_custom_paths_via_yaml() {
        let path = test_utils::test_file("config-paths", "yaml");
        let yaml = "paths:\n  db: /custom/boi.db\n";
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let cfg = Config::load_from(&path);
        assert_eq!(cfg.db_path(), PathBuf::from("/custom/boi.db"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_brain_field_deserializes() {
        let path = test_utils::test_file("config-brain", "yaml");
        let yaml = "brain: /some/brain/dir\n";
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let cfg = Config::load_from(&path);
        assert_eq!(cfg.brain, Some(PathBuf::from("/some/brain/dir")));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_brain_defaults_to_none() {
        let cfg = Config::default();
        assert!(cfg.brain.is_none());
    }

    #[test]
    fn test_brain_validate_path_missing() {
        let err = validate_brain(Path::new("/nonexistent/brain/path")).unwrap_err();
        assert!(err.contains("not found"), "err={}", err);
    }

    #[test]
    fn test_brain_validate_missing_claude_md() {
        let dir = test_utils::test_dir("brain-no-claude-md");
        let err = validate_brain(&dir).unwrap_err();
        assert!(err.contains("CLAUDE.md"), "err={}", err);
    }

    #[test]
    fn test_brain_validate_ok() {
        let dir = test_utils::test_dir("brain-valid");
        fs::write(dir.join("CLAUDE.md"), "# context").unwrap();
        validate_brain(&dir).expect("valid brain should pass validation");
    }

    #[test]
    fn test_brain_resolve_spec_overrides_config() {
        let spec_brain = PathBuf::from("/spec/brain");
        let config_brain = PathBuf::from("/config/brain");
        let resolved = resolve_brain(Some(&spec_brain), Some(&config_brain));
        assert_eq!(resolved, Some(PathBuf::from("/spec/brain")));
    }

    #[test]
    fn test_brain_resolve_config_fallback() {
        let config_brain = PathBuf::from("/config/brain");
        let resolved = resolve_brain(None, Some(&config_brain));
        assert_eq!(resolved, Some(PathBuf::from("/config/brain")));
    }

    #[test]
    fn test_brain_resolve_none_when_unset() {
        let resolved = resolve_brain(None, None);
        assert!(resolved.is_none());
    }
}
