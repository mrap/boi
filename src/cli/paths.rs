//! `~/.boi/v2/` filesystem-path resolution shared across the CLI modules.
//!
//! Every `boi` process — the daemon and each short-lived command — needs the
//! same set of paths: the SQLite `boi.db`, the control socket, the OTel traces
//! directory, the phase / pipeline / recipe directories. Resolving them in one
//! place keeps the layout in a single file (review item: no scattered
//! `~/.boi/v2/...` string literals).
//!
//! `~` is expanded from `$HOME`; `$HOME` unset is a loud [`PathError`] — never
//! a silent CWD-relative fallback (the `revision_artifact_path` lesson,
//! review B-svc-S3).

use std::path::PathBuf;

/// The `~/.boi/v2/` root could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// `$HOME` is unset, so `~/.boi/v2/` cannot be located.
    #[error("cannot locate ~/.boi/v2: $HOME is unset")]
    HomeUnset,
    /// `~/.boi/v2/` (or a parent of it) could not be created.
    ///
    /// Surfaces the first-run case: a command that opens the database
    /// (`dispatch`, `spec show`, `log`, `mcp serve`, `clean`, `dashboard`)
    /// runs before the daemon has ever started. `sqlite://…?mode=rwc` creates
    /// the *file* but never its parent directory, so without this the driver
    /// fails with an opaque "unable to open database file" (review item:
    /// first-run crash).
    #[error("could not create directory {path}: {source}")]
    CreateDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// The `~/.boi/v2/` root directory.
///
/// `$HOME` unset is a loud failure — BOI v2 is single-user and home-rooted; a
/// missing `$HOME` is an environment fault the caller must see, never a
/// guessed CWD-relative path.
pub fn boi_root() -> Result<PathBuf, PathError> {
    let home = std::env::var("HOME").map_err(|_| PathError::HomeUnset)?;
    Ok(PathBuf::from(home).join(".boi").join("v2"))
}

/// The SQLite database path — `~/.boi/v2/boi.db`.
pub fn boi_db() -> Result<PathBuf, PathError> {
    Ok(boi_root()?.join("boi.db"))
}

/// The `sqlx` connection URL for the SQLite database.
///
/// `sqlite://<abs-path>?mode=rwc` — `rwc` creates the file on first daemon
/// boot. A read-only command opens the same URL; SQLite tolerates `rwc` on an
/// existing file.
///
/// `mode=rwc` creates the database *file* but never its parent directory —
/// previously only `daemon start` created `~/.boi/v2/` (see
/// `cli::daemon::run`'s `create_dir_all` of the log dir), so any other
/// command run before the daemon's first boot (`dispatch`, `spec show`,
/// `log`, `mcp serve`, `clean`, `dashboard`) hit a raw sqlite "unable to open
/// database file" error. This is the single choke point every DB-opening
/// command resolves its URL through, so creating `~/.boi/v2/` here (rather
/// than per-command) fixes all of them at once.
pub fn boi_db_url() -> Result<String, PathError> {
    let root = boi_root()?;
    std::fs::create_dir_all(&root).map_err(|source| PathError::CreateDir {
        path: root.clone(),
        source,
    })?;
    let db = root.join("boi.db");
    Ok(format!("sqlite://{}?mode=rwc", db.display()))
}

/// The Unix-domain control-socket path — `~/.boi/v2/daemon.sock`.
pub fn control_socket() -> Result<PathBuf, PathError> {
    Ok(boi_root()?.join("daemon.sock"))
}

/// The OTel traces directory — `~/.boi/v2/traces/`.
pub fn traces_dir() -> Result<PathBuf, PathError> {
    Ok(boi_root()?.join("traces"))
}

/// The glob the DuckDB query layer reads — every OTel JSONL file under
/// `~/.boi/v2/traces/`.
pub fn traces_glob() -> Result<String, PathError> {
    Ok(format!("{}/**/*.jsonl", traces_dir()?.display()))
}

/// The phase-declaration directory — `~/.boi/v2/phases/`.
pub fn phases_dir() -> Result<PathBuf, PathError> {
    Ok(boi_root()?.join("phases"))
}

/// The pipeline-declaration directory — `~/.boi/v2/pipelines/`.
pub fn pipelines_dir() -> Result<PathBuf, PathError> {
    Ok(boi_root()?.join("pipelines"))
}

/// The Goose-recipe scratch directory — `~/.boi/v2/recipes/`.
pub fn recipes_dir() -> Result<PathBuf, PathError> {
    Ok(boi_root()?.join("recipes"))
}

/// The operator config file — `~/.boi/v2/config.toml`.
///
/// Carries the design-§5 `[worktree]` table (see `config::worktree`). The
/// file is OPTIONAL — absent means every key takes its documented default;
/// present-but-malformed fails daemon boot loudly.
pub fn config_file() -> Result<PathBuf, PathError> {
    Ok(boi_root()?.join("config.toml"))
}

/// The operator secrets directory — `~/.boi/v2/secrets/`.
///
/// Place `*.env` files here containing provider auth tokens (e.g. `claude.env`
/// with `CLAUDE_CODE_OAUTH_TOKEN=…`). The daemon reads these at startup via
/// `runtime::secrets::bootstrap_provider_env()` — no secrets appear in the
/// LaunchAgent plist. Non-hex users drop files here directly; hex users symlink
/// from their workspace secrets directory.
pub fn secrets_dir() -> Result<PathBuf, PathError> {
    Ok(boi_root()?.join("secrets"))
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Every path nests under the `~/.boi/v2/` root with the stable leaf the
    /// daemon and the CLI must agree on.
    ///
    /// The test does NOT mutate the process `$HOME` (that would race every
    /// other test that reads it) — it asserts the path *structure* against
    /// whatever `$HOME` is. The crate is built with `unsafe_code = "deny"`, so
    /// an `unsafe { set_var }` is also a lint failure; structural assertions
    /// avoid both problems.
    #[test]
    fn test_l1_paths_share_the_boi_v2_root() {
        // If `$HOME` is set (the normal case), every path resolves and nests
        // under `~/.boi/v2/` with the expected leaf.
        if std::env::var("HOME").is_ok() {
            assert!(boi_root().unwrap().ends_with(".boi/v2"));
            assert!(boi_db().unwrap().ends_with(".boi/v2/boi.db"));
            assert!(control_socket().unwrap().ends_with(".boi/v2/daemon.sock"));
            assert!(traces_dir().unwrap().ends_with(".boi/v2/traces"));
            assert!(phases_dir().unwrap().ends_with(".boi/v2/phases"));
            assert!(pipelines_dir().unwrap().ends_with(".boi/v2/pipelines"));
            assert!(recipes_dir().unwrap().ends_with(".boi/v2/recipes"));
            assert!(config_file().unwrap().ends_with(".boi/v2/config.toml"));
            assert!(boi_db_url().unwrap().starts_with("sqlite://"));
            assert!(traces_glob().unwrap().ends_with("traces/**/*.jsonl"));
        }
    }
}
