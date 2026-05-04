use boi::config::{Config, WorkerPoolConfig};
use serial_test::serial;
use std::io::Write;

#[test]
fn test_config_fly_integration_parses_pool_type() {
    let path = std::env::temp_dir().join("boi-cfg-fly-integ.yaml");
    let yaml = "worker_pool:\n  type: fly\n  fly:\n    app: boi-test\n    memory_mb: 512\n";
    std::fs::File::create(&path).unwrap().write_all(yaml.as_bytes()).unwrap();

    let cfg = Config::load_from(&path);
    let wp = cfg.worker_pool.expect("worker_pool should be set");
    assert_eq!(wp.pool_type_str(), "fly");
    assert_eq!(
        wp.fly.as_ref().and_then(|f| f.app.as_deref()),
        Some("boi-test")
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
#[serial]
fn test_config_fly_integration_validate_requires_token() {
    let wp = WorkerPoolConfig {
        pool_type: Some("fly".to_string()),
        fly: None,
    };
    let saved = std::env::var("FLY_API_TOKEN").ok();
    std::env::remove_var("FLY_API_TOKEN");

    let result = wp.validate();
    assert!(result.is_err());
    assert!(
        format!("{}", result.unwrap_err()).contains("FLY_API_TOKEN"),
        "error must mention FLY_API_TOKEN"
    );

    if let Some(v) = saved {
        std::env::set_var("FLY_API_TOKEN", v);
    }
}

#[test]
#[serial]
fn test_config_fly_integration_local_pool_no_token_needed() {
    let wp = WorkerPoolConfig {
        pool_type: Some("local".to_string()),
        fly: None,
    };
    let saved = std::env::var("FLY_API_TOKEN").ok();
    std::env::remove_var("FLY_API_TOKEN");

    assert!(wp.validate().is_ok());

    if let Some(v) = saved {
        std::env::set_var("FLY_API_TOKEN", v);
    }
}
