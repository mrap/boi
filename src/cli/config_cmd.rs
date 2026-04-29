use crate::config;

pub fn cmd_config(key: Option<&str>, value: Option<&str>, cfg: &config::Config) {
    match (key, value) {
        (None, _) => {
            println!("max_workers:          {}", cfg.max_workers());
            println!("spawns_per_tick:      {}", cfg.spawns_per_tick());
            println!("task_timeout_minutes: {}", cfg.task_timeout_secs() / 60);
            println!("retry_count:          {}", cfg.retry_count());
            println!("db_path:              {}", cfg.db_path().display());
            println!("worktrees_dir:        {}", cfg.worktrees_dir().display());
            println!("logs_dir:             {}", cfg.logs_dir().display());
            let config_path = config::default_config_path();
            println!("config_file:          {}", config_path.display());
            if let Some(hooks) = &cfg.hooks {
                let hook_names: Vec<&str> = hooks.keys().map(|k| k.as_str()).collect();
                println!("hooks:                {}", hook_names.join(", "));
            } else {
                println!("hooks:                (none configured)");
            }
        }
        (Some(k), None) => {
            let val = match k {
                "max_workers" => cfg.max_workers().to_string(),
                "spawns_per_tick" => cfg.spawns_per_tick().to_string(),
                "task_timeout_minutes" => (cfg.task_timeout_secs() / 60).to_string(),
                "retry_count" => cfg.retry_count().to_string(),
                "db_path" => cfg.db_path().display().to_string(),
                "worktrees_dir" => cfg.worktrees_dir().display().to_string(),
                "logs_dir" => cfg.logs_dir().display().to_string(),
                _ => {
                    eprintln!("unknown config key: {}", k);
                    std::process::exit(1);
                }
            };
            println!("{}", val);
        }
        (Some(k), Some(v)) => {
            // Validate key
            match k {
                "max_workers" | "spawns_per_tick" | "task_timeout_minutes" | "retry_count" => {}
                _ => {
                    eprintln!("unknown config key: {} (supported: max_workers, spawns_per_tick, task_timeout_minutes, retry_count)", k);
                    std::process::exit(1);
                }
            }
            let config_path = config::default_config_path();
            let mut cfg_map: serde_yml::Value = if config_path.exists() {
                let content = std::fs::read_to_string(&config_path).unwrap_or_default();
                serde_yml::from_str(&content)
                    .unwrap_or(serde_yml::Value::Mapping(Default::default()))
            } else {
                serde_yml::Value::Mapping(Default::default())
            };
            if let serde_yml::Value::Mapping(ref mut map) = cfg_map {
                map.insert(
                    serde_yml::Value::String(k.to_string()),
                    serde_yml::Value::String(v.to_string()),
                );
            }
            if let Some(parent) = config_path.parent() {
                let _ = std::fs::create_dir_all(parent); // intentional: best-effort dir creation before config write
            }
            let yaml_str =
                serde_yml::to_string(&cfg_map).expect("cfg_map is always serializable to YAML");
            if let Err(e) = std::fs::write(&config_path, yaml_str) {
                eprintln!(
                    "error: failed to write config file {}: {}",
                    config_path.display(),
                    e
                );
                std::process::exit(1);
            }
            println!("set {} = {}", k, v);
        }
    }
}
