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
pub struct ContextConfig {
    pub always_include: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct FlyPoolConfig {
    pub app: Option<String>,
    pub region: Option<String>,
    pub image: Option<String>,
    pub cpu_kind: Option<String>,
    pub cpu_count: Option<u32>,
    pub memory_mb: Option<u32>,
    pub max_cost_usd: Option<f64>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct WorkerPoolConfig {
    #[serde(rename = "type")]
    pub pool_type: Option<String>,
    pub fly: Option<FlyPoolConfig>,
}

impl WorkerPoolConfig {
    pub fn pool_type_str(&self) -> &str {
        self.pool_type.as_deref().unwrap_or("local")
    }

    /// Error loudly if type=fly but FLY_API_TOKEN is not set.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.pool_type_str() == "fly" && std::env::var("FLY_API_TOKEN").is_err() {
            anyhow::bail!(
                "worker_pool.type=fly requires FLY_API_TOKEN to be set in environment"
            );
        }
        Ok(())
    }

    /// Create a WorkerPool from this config.
    /// For "fly": reads FLY_API_TOKEN from environment (call validate() first).
    /// For "local" (default): uses the provided queue_path, hook_config, max_workers.
    pub fn create_pool(
        &self,
        queue_path: &str,
        hook_config: crate::hooks::HookConfig,
        max_workers: u32,
    ) -> anyhow::Result<Box<dyn crate::pool::WorkerPool>> {
        match self.pool_type_str() {
            "fly" => {
                let dispatcher = crate::remote::FlyDispatcher::new()
                    .map_err(|e| anyhow::anyhow!("fly dispatcher init: {e}"))?;
                let dispatcher = if let Some(fly_cfg) = &self.fly {
                    dispatcher.with_fly_config(fly_cfg)
                } else {
                    dispatcher
                };
                Ok(Box::new(dispatcher))
            }
            _ => Ok(Box::new(crate::pool::LocalThreadPool::new(
                queue_path,
                hook_config,
                max_workers,
            ))),
        }
    }
}

// ── Named-pool config (flat format used in worker_pools block) ───────────────

/// Per-pool config entry inside a `worker_pools:` block.
/// Fly fields are inlined (not nested) matching the design doc format.
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct NamedPoolConfig {
    #[serde(rename = "type")]
    pub pool_type: Option<String>,
    pub max_workers: Option<u32>,
    // fly-specific fields (flat in named-pool format)
    pub app: Option<String>,
    pub region: Option<String>,
    pub image: Option<String>,
    pub cpu_kind: Option<String>,
    pub cpu_count: Option<u32>,
    pub memory_mb: Option<u32>,
    pub max_cost_usd: Option<f64>,
}

impl NamedPoolConfig {
    pub fn pool_type_str(&self) -> &str {
        self.pool_type.as_deref().unwrap_or("local")
    }

    pub fn create_pool(
        &self,
        queue_path: &str,
        hook_config: crate::hooks::HookConfig,
    ) -> anyhow::Result<Box<dyn crate::pool::WorkerPool>> {
        let max_workers = self.max_workers.unwrap_or(5);
        match self.pool_type_str() {
            "fly" => {
                let dispatcher = crate::remote::FlyDispatcher::new()
                    .map_err(|e| anyhow::anyhow!("fly dispatcher init: {e}"))?;
                let fly_cfg = FlyPoolConfig {
                    app: self.app.clone(),
                    region: self.region.clone(),
                    image: self.image.clone(),
                    cpu_kind: self.cpu_kind.clone(),
                    cpu_count: self.cpu_count,
                    memory_mb: self.memory_mb,
                    max_cost_usd: self.max_cost_usd,
                };
                Ok(Box::new(dispatcher.with_fly_config(&fly_cfg)))
            }
            _ => Ok(Box::new(crate::pool::LocalThreadPool::new(
                queue_path,
                hook_config,
                max_workers,
            ))),
        }
    }
}

// ── WorkerPoolsConfig ─────────────────────────────────────────────────────────

/// Named-pools block. Deserializes the mixed format where `default` is a
/// string referencing a named pool and all other keys are pool config objects:
///
/// ```yaml
/// worker_pools:
///   local:
///     type: local
///     max_workers: 5
///   fly-runners:
///     type: fly
///     app: boi-runners
///     region: iad
///     max_workers: 10
///   default: local
/// ```
#[derive(Debug)]
pub struct WorkerPoolsConfig {
    pub pools: HashMap<String, NamedPoolConfig>,
    pub default: String,
}

impl<'de> serde::Deserialize<'de> for WorkerPoolsConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = HashMap::<String, serde_yml::Value>::deserialize(deserializer)?;
        let default_name = match raw.get("default") {
            Some(v) => v
                .as_str()
                .ok_or_else(|| D::Error::custom("`default` must be a string pool name"))?
                .to_string(),
            None => return Err(D::Error::missing_field("default")),
        };
        let mut pools = HashMap::new();
        for (k, v) in &raw {
            if k == "default" {
                continue;
            }
            let cfg: NamedPoolConfig =
                serde_yml::from_value(v.clone()).map_err(D::Error::custom)?;
            pools.insert(k.clone(), cfg);
        }
        if !pools.contains_key(&default_name) {
            return Err(D::Error::custom(format!(
                "`default` pool \"{default_name}\" is not defined in worker_pools"
            )));
        }
        Ok(WorkerPoolsConfig { pools, default: default_name })
    }
}

impl serde::Serialize for WorkerPoolsConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.pools.len() + 1))?;
        for (k, v) in &self.pools {
            map.serialize_entry(k, v)?;
        }
        map.serialize_entry("default", &self.default)?;
        map.end()
    }
}

// ── PoolRegistry ──────────────────────────────────────────────────────────────

/// Resolves pool names → WorkerPool instances.
/// Built from WorkerPoolsConfig or injected with mock pools in tests.
pub struct PoolRegistry {
    pools: HashMap<String, Box<dyn crate::pool::WorkerPool>>,
    default_name: String,
}

impl PoolRegistry {
    /// Construct directly from a pre-built pools map (useful in tests).
    pub fn from_pools(
        pools: HashMap<String, Box<dyn crate::pool::WorkerPool>>,
        default_name: impl Into<String>,
    ) -> Self {
        PoolRegistry { pools, default_name: default_name.into() }
    }

    /// Look up a pool by name. Returns None if unknown.
    pub fn get(&self, name: &str) -> Option<&dyn crate::pool::WorkerPool> {
        self.pools.get(name).map(|p| p.as_ref())
    }

    /// Returns true if a pool with this name is registered.
    pub fn has_pool(&self, name: &str) -> bool {
        self.pools.contains_key(name)
    }

    /// Return the default pool name.
    pub fn default_name(&self) -> &str {
        &self.default_name
    }

    /// Return all registered pool names.
    pub fn pool_names(&self) -> Vec<&str> {
        self.pools.keys().map(|s| s.as_str()).collect()
    }

    /// Resolve an optional pool name: Some(name) → named pool, None → default.
    /// Returns None only if the named pool doesn't exist.
    pub fn resolve(&self, name: Option<&str>) -> Option<&dyn crate::pool::WorkerPool> {
        self.get(name.unwrap_or(&self.default_name))
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    pub max_workers: Option<u32>,
    pub task_timeout_minutes: Option<u32>,
    pub retry_count: Option<u32>,
    /// Kill a task early if requeued >= this many times. None = disabled.
    pub convergence_threshold: Option<u32>,
    pub cleanup_on_failure: Option<bool>,
    pub hooks: Option<HashMap<String, HookEntry>>,
    pub paths: Option<Paths>,
    pub claude_bin: Option<String>,
    pub models: Option<HashMap<String, String>>,
    pub context: Option<ContextConfig>,
    pub worker_pool: Option<WorkerPoolConfig>,
    /// Named-pools registry (supersedes `worker_pool` when present).
    pub worker_pools: Option<WorkerPoolsConfig>,
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

    pub fn convergence_threshold(&self) -> Option<u32> {
        self.convergence_threshold
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

    /// Build a PoolRegistry from config.
    /// Uses `worker_pools` (named pools) if present; falls back to `worker_pool`
    /// (single pool) or a default local pool.
    pub fn build_pool_registry(
        &self,
        queue_path: &str,
        hook_config: crate::hooks::HookConfig,
    ) -> anyhow::Result<PoolRegistry> {
        if let Some(pools_cfg) = &self.worker_pools {
            let mut pools: HashMap<String, Box<dyn crate::pool::WorkerPool>> = HashMap::new();
            for (name, cfg) in &pools_cfg.pools {
                pools.insert(name.clone(), cfg.create_pool(queue_path, hook_config.clone())?);
            }
            return Ok(PoolRegistry::from_pools(pools, pools_cfg.default.clone()));
        }
        // Fall back to single worker_pool or default local pool
        let max_workers = self.max_workers();
        let pool: Box<dyn crate::pool::WorkerPool> = if let Some(wp) = &self.worker_pool {
            wp.create_pool(queue_path, hook_config.clone(), max_workers)?
        } else {
            Box::new(crate::pool::LocalThreadPool::new(queue_path, hook_config, max_workers))
        };
        let mut pools = HashMap::new();
        pools.insert("default".to_string(), pool);
        Ok(PoolRegistry::from_pools(pools, "default"))
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
    fn test_models_override_via_yaml() {
        let path = test_utils::test_file("config-models", "yaml");
        let yaml = "models:\n  spec-review: claude-opus-4-7\n  execute: claude-haiku-4-5-20251001\n";
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let cfg = Config::load_from(&path);
        let models = cfg.models.as_ref().expect("models should be present");
        assert_eq!(models.get("spec-review").map(|s| s.as_str()), Some("claude-opus-4-7"));
        assert_eq!(models.get("execute").map(|s| s.as_str()), Some("claude-haiku-4-5-20251001"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_models_none_when_absent() {
        let cfg = Config::default();
        assert!(cfg.models.is_none());
    }

    #[test]
    fn test_context_always_include_via_yaml() {
        let path = test_utils::test_file("config-context", "yaml");
        let yaml = "context:\n  always_include:\n    - ~/.claude/SHARED.md\n    - ~/notes.md\n";
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let cfg = Config::load_from(&path);
        let ctx = cfg.context.as_ref().expect("context should be present");
        let includes = ctx.always_include.as_ref().expect("always_include should be present");
        assert_eq!(includes.len(), 2);
        assert_eq!(includes[0], "~/.claude/SHARED.md");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_context_none_when_absent() {
        let cfg = Config::default();
        assert!(cfg.context.is_none());
    }

    // ── config_fly tests ─────────────────────────────────────────────────────

    #[test]
    fn test_config_fly_parses_worker_pool_type() {
        let path = test_utils::test_file("config-fly-type", "yaml");
        let yaml = "worker_pool:\n  type: fly\n  fly:\n    app: boi-workers\n    region: iad\n    image: registry.fly.io/boi-workers:latest\n    cpu_kind: shared\n    cpu_count: 1\n    memory_mb: 256\n    max_cost_usd: 5.0\n";
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let cfg = Config::load_from(&path);
        let wp = cfg.worker_pool.as_ref().expect("worker_pool should be present");
        assert_eq!(wp.pool_type_str(), "fly");
        let fly = wp.fly.as_ref().expect("fly config should be present");
        assert_eq!(fly.app.as_deref(), Some("boi-workers"));
        assert_eq!(fly.region.as_deref(), Some("iad"));
        assert_eq!(fly.image.as_deref(), Some("registry.fly.io/boi-workers:latest"));
        assert_eq!(fly.cpu_kind.as_deref(), Some("shared"));
        assert_eq!(fly.cpu_count, Some(1));
        assert_eq!(fly.memory_mb, Some(256));
        assert_eq!(fly.max_cost_usd, Some(5.0));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_config_fly_defaults_to_local_when_absent() {
        let cfg = Config::default();
        // No worker_pool configured → pool_type_str defaults to "local"
        let wp = cfg.worker_pool.unwrap_or_default();
        assert_eq!(wp.pool_type_str(), "local");
    }

    #[test]
    fn test_config_fly_validate_fails_without_token() {
        let wp = WorkerPoolConfig {
            pool_type: Some("fly".to_string()),
            fly: None,
        };
        // Remove FLY_API_TOKEN if present, then validate must fail.
        let saved = std::env::var("FLY_API_TOKEN").ok();
        std::env::remove_var("FLY_API_TOKEN");

        let result = wp.validate();
        assert!(result.is_err(), "validate() must fail when FLY_API_TOKEN is missing");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("FLY_API_TOKEN"), "error message must mention FLY_API_TOKEN");

        // Restore env
        if let Some(v) = saved {
            std::env::set_var("FLY_API_TOKEN", v);
        }
    }

    #[test]
    fn test_config_fly_validate_passes_with_token() {
        let wp = WorkerPoolConfig {
            pool_type: Some("fly".to_string()),
            fly: None,
        };
        std::env::set_var("FLY_API_TOKEN", "test-token-abc");
        let result = wp.validate();
        std::env::remove_var("FLY_API_TOKEN");
        assert!(result.is_ok(), "validate() must pass when FLY_API_TOKEN is set");
    }

    #[test]
    fn test_config_fly_validate_local_skips_token_check() {
        let wp = WorkerPoolConfig {
            pool_type: Some("local".to_string()),
            fly: None,
        };
        let saved = std::env::var("FLY_API_TOKEN").ok();
        std::env::remove_var("FLY_API_TOKEN");

        let result = wp.validate();
        assert!(result.is_ok(), "validate() must pass for local pool without FLY_API_TOKEN");

        if let Some(v) = saved {
            std::env::set_var("FLY_API_TOKEN", v);
        }
    }

    #[test]
    fn test_config_fly_create_pool_local_succeeds() {
        let wp = WorkerPoolConfig {
            pool_type: Some("local".to_string()),
            fly: None,
        };
        let hook_cfg = crate::hooks::HookConfig::default();
        let pool = wp.create_pool("/tmp/test-boi.db", hook_cfg, 2);
        assert!(pool.is_ok(), "create_pool('local') must succeed");
        assert_eq!(pool.unwrap().max_workers(), 2);
    }

    #[test]
    fn test_config_fly_create_pool_fly_fails_without_token() {
        let wp = WorkerPoolConfig {
            pool_type: Some("fly".to_string()),
            fly: None,
        };
        let saved = std::env::var("FLY_API_TOKEN").ok();
        std::env::remove_var("FLY_API_TOKEN");

        let hook_cfg = crate::hooks::HookConfig::default();
        let pool = wp.create_pool("/tmp/test-boi.db", hook_cfg, 2);
        assert!(pool.is_err(), "create_pool('fly') must fail without FLY_API_TOKEN");

        if let Some(v) = saved {
            std::env::set_var("FLY_API_TOKEN", v);
        }
    }

    // ── pool_registry tests ───────────────────────────────────────────────────

    struct MockPool {
        name: String,
        max: u32,
    }

    impl crate::pool::WorkerPool for MockPool {
        fn spawn(
            &self,
            id: &str,
            _: &str,
            _: &str,
            _: &crate::worker::WorkerConfig,
        ) -> anyhow::Result<crate::pool::JobId> {
            Ok(crate::pool::JobId::new(format!("{}-{}", self.name, id)))
        }
        fn status(&self, _: &crate::pool::JobId) -> anyhow::Result<crate::pool::JobStatus> {
            Ok(crate::pool::JobStatus::Completed)
        }
        fn collect(&self, _: &crate::pool::JobId) -> anyhow::Result<crate::pool::JobOutput> {
            Ok(crate::pool::JobOutput { exit_code: 0, stdout: String::new(), stderr: String::new() })
        }
        fn cancel(&self, _: &crate::pool::JobId) -> anyhow::Result<()> {
            Ok(())
        }
        fn max_workers(&self) -> u32 {
            self.max
        }
    }

    fn make_registry() -> PoolRegistry {
        let mut pools: HashMap<String, Box<dyn crate::pool::WorkerPool>> = HashMap::new();
        pools.insert("local".to_string(), Box::new(MockPool { name: "local".to_string(), max: 5 }));
        pools.insert("remote".to_string(), Box::new(MockPool { name: "remote".to_string(), max: 10 }));
        PoolRegistry::from_pools(pools, "local")
    }

    #[test]
    fn pool_registry_resolves_named_pool() {
        let reg = make_registry();
        let pool = reg.get("remote").expect("remote pool must exist");
        assert_eq!(pool.max_workers(), 10);
    }

    #[test]
    fn pool_registry_resolves_default() {
        let reg = make_registry();
        assert_eq!(reg.default_name(), "local");
        let pool = reg.resolve(None).expect("default pool must resolve");
        assert_eq!(pool.max_workers(), 5);
    }

    #[test]
    fn pool_registry_unknown_pool_returns_none() {
        let reg = make_registry();
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn pool_registry_has_pool_reflects_membership() {
        let reg = make_registry();
        assert!(reg.has_pool("local"));
        assert!(reg.has_pool("remote"));
        assert!(!reg.has_pool("ghost"));
    }

    #[test]
    fn pool_registry_resolve_named_picks_correct_pool() {
        let reg = make_registry();
        let pool = reg.resolve(Some("remote")).expect("remote must resolve");
        assert_eq!(pool.max_workers(), 10);
        let pool = reg.resolve(Some("local")).expect("local must resolve");
        assert_eq!(pool.max_workers(), 5);
    }

    #[test]
    fn pool_registry_yaml_parses_named_pools() {
        let yaml = "local:\n  type: local\n  max_workers: 3\nfly-runners:\n  type: fly\n  app: boi-runners\n  region: iad\n  max_workers: 10\ndefault: local\n";
        let cfg: WorkerPoolsConfig = serde_yml::from_str(yaml).expect("parse must succeed");
        assert_eq!(cfg.default, "local");
        assert!(cfg.pools.contains_key("local"));
        assert!(cfg.pools.contains_key("fly-runners"));
        assert_eq!(cfg.pools["local"].pool_type_str(), "local");
        assert_eq!(cfg.pools["local"].max_workers, Some(3));
        assert_eq!(cfg.pools["fly-runners"].app.as_deref(), Some("boi-runners"));
        assert_eq!(cfg.pools["fly-runners"].region.as_deref(), Some("iad"));
        assert_eq!(cfg.pools["fly-runners"].max_workers, Some(10));
    }

    #[test]
    fn pool_registry_yaml_rejects_missing_default() {
        let yaml = "local:\n  type: local\n  max_workers: 3\n";
        let result: Result<WorkerPoolsConfig, _> = serde_yml::from_str(yaml);
        assert!(result.is_err(), "missing `default` must fail");
    }

    #[test]
    fn pool_registry_yaml_rejects_undefined_default() {
        let yaml = "local:\n  type: local\n  max_workers: 3\ndefault: ghost\n";
        let result: Result<WorkerPoolsConfig, _> = serde_yml::from_str(yaml);
        assert!(result.is_err(), "default pointing to undefined pool must fail");
    }

    #[test]
    fn pool_registry_config_worker_pools_field_parses() {
        let path = test_utils::test_file("config-worker-pools", "yaml");
        let yaml = "worker_pools:\n  local:\n    type: local\n    max_workers: 4\n  default: local\n";
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let cfg = Config::load_from(&path);
        let wps = cfg.worker_pools.as_ref().expect("worker_pools must parse");
        assert_eq!(wps.default, "local");
        assert!(wps.pools.contains_key("local"));
        assert_eq!(wps.pools["local"].max_workers, Some(4));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn pool_registry_build_from_config_local_only() {
        let cfg = Config::default();
        let hook_cfg = crate::hooks::HookConfig::default();
        let reg = cfg.build_pool_registry("/tmp/test-boi.db", hook_cfg);
        assert!(reg.is_ok(), "build_pool_registry must succeed for default config");
        let reg = reg.unwrap();
        assert_eq!(reg.default_name(), "default");
        assert!(reg.has_pool("default"));
    }
}
