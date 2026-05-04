pub mod builtins;
pub mod cli;
pub mod config;
pub mod fmt;
pub mod hooks;
pub mod phases;
pub mod prompt;
pub mod queue;
pub mod runner;
pub mod runtime;
pub mod spawn;
pub mod spec;
pub mod telemetry;
#[cfg(test)]
pub mod test_utils;
pub mod worker;
pub mod worktree;

/// Loads `~/.boi/.env` (or `$BOI_ENV_FILE`) into process env at startup.
/// Uses `dotenvy::from_path` so existing process env wins; .env only fills missing keys.
pub fn load_boi_env() {
    let env_path = std::env::var("BOI_ENV_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(".boi")
                .join(".env")
        });
    if env_path.exists() {
        let _ = dotenvy::from_path(&env_path);
    }
}
