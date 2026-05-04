use serial_test::serial;

fn make_temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "boi-dotenv-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// a. .env present → key loaded
#[test]
#[serial]
fn test_env_file_present_key_loaded() {
    let home = make_temp_dir();
    let boi_dir = home.join(".boi");
    std::fs::create_dir_all(&boi_dir).unwrap();
    std::fs::write(boi_dir.join(".env"), "BOI_TEST_A_KEY=value_from_file\n").unwrap();

    let prev_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", &home);
    std::env::remove_var("BOI_ENV_FILE");
    std::env::remove_var("BOI_TEST_A_KEY");

    boi::load_boi_env();

    let val = std::env::var("BOI_TEST_A_KEY").ok();

    // restore
    std::env::remove_var("BOI_TEST_A_KEY");
    match prev_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
    std::fs::remove_dir_all(&home).ok();

    assert_eq!(val.as_deref(), Some("value_from_file"));
}

// b. .env missing → silent OK (no panic, no crash)
#[test]
#[serial]
fn test_env_file_missing_silent() {
    std::env::set_var("BOI_ENV_FILE", "/tmp/boi-test-nonexistent-file-xyz123.env");

    boi::load_boi_env(); // must not panic

    std::env::remove_var("BOI_ENV_FILE");
}

// c. Existing env value not clobbered
#[test]
#[serial]
fn test_existing_env_not_clobbered() {
    let dir = make_temp_dir();
    let env_file = dir.join(".env");
    std::fs::write(&env_file, "BOI_TEST_C_KEY=from_file\n").unwrap();

    std::env::set_var("BOI_ENV_FILE", &env_file);
    std::env::set_var("BOI_TEST_C_KEY", "from_process_env");

    boi::load_boi_env();

    let val = std::env::var("BOI_TEST_C_KEY").ok();

    // restore
    std::env::remove_var("BOI_TEST_C_KEY");
    std::env::remove_var("BOI_ENV_FILE");
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(val.as_deref(), Some("from_process_env"), "existing env var must not be overwritten by .env file");
}

// d. BOI_ENV_FILE override path works
#[test]
#[serial]
fn test_boi_env_file_override_path() {
    let dir = make_temp_dir();
    let custom_env = dir.join("custom.env");
    std::fs::write(&custom_env, "BOI_TEST_D_KEY=override_path_val\n").unwrap();

    std::env::set_var("BOI_ENV_FILE", &custom_env);
    std::env::remove_var("BOI_TEST_D_KEY");

    boi::load_boi_env();

    let val = std::env::var("BOI_TEST_D_KEY").ok();

    // restore
    std::env::remove_var("BOI_TEST_D_KEY");
    std::env::remove_var("BOI_ENV_FILE");
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(val.as_deref(), Some("override_path_val"), "BOI_ENV_FILE should point to custom path and load it");
}
