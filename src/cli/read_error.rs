//! The shared `ReadError` type for read-only CLI commands.
//!
//! Originally in `cli::status`; relocated here when `boi status` was removed
//! (decision: `boi-status-replaced-by-dashboard-2026-05-21`). All three
//! read-only commands — [`log`](crate::cli::log), [`spec`](crate::cli::spec),
//! and [`dashboard`](crate::cli::dashboard) — share this error type because
//! they are all SQLite-direct readers.

use crate::cli::paths::PathError;
use crate::repo::db::RepoError;

/// A read-only CLI command (`log` / `spec show` / `dashboard`) failed.
///
/// Shared by [`log`](crate::cli::log), [`spec`](crate::cli::spec), and
/// [`dashboard`](crate::cli::dashboard) — all three are SQLite-direct readers,
/// so one error type covers them. (`spec show`'s own variants ride here too.)
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The `~/.boi/v2/` path layout could not be resolved.
    #[error(transparent)]
    Path(#[from] PathError),
    /// A repo-layer query failed.
    #[error("read failed: {0}")]
    Repo(#[from] RepoError),
    /// The id argument was not a well-formed spec / task id.
    #[error("invalid id: {0}")]
    BadId(String),
    /// `boi spec show --version N` named a version that does not exist.
    #[error("spec {spec_id} has no version {version}")]
    NoSuchVersion {
        /// The spec queried.
        spec_id: String,
        /// The out-of-range version.
        version: i64,
    },
}
