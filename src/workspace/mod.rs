pub mod git;

use std::path::{Path, PathBuf};

pub type BackendResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Output from a command executed inside a workspace.
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Pluggable workspace isolation for BOI.
///
/// Invariants every backend must satisfy:
/// 1. Isolation: concurrent spec_ids get non-overlapping directories.
/// 2. Idempotent create: calling create twice with the same spec_id returns the same path.
/// 3. Best-effort cleanup: cleanup must not fail fatally if the workspace is already gone.
/// 4. Exec runs in-directory: the command's working directory is the workspace root.
pub trait WorkspaceBackend: Send + Sync {
    /// Create an isolated workspace for `spec_id` from `source` (a repo or directory path).
    /// Returns the absolute path to the workspace directory.
    fn create(&self, spec_id: &str, source: &str) -> BackendResult<PathBuf>;

    /// Execute `command` inside the workspace at `workspace_path`.
    fn exec(&self, workspace_path: &Path, command: &str) -> BackendResult<ExecResult>;

    /// Merge changes from `workspace_path` back into `target` (branch name, path, etc.).
    /// Optional: only called when the spec declares `merge_back: true`.
    fn merge(&self, _workspace_path: &Path, _target: &str) -> BackendResult<()> {
        Ok(())
    }

    /// Remove the workspace for `spec_id`. Must not fail fatally if already gone.
    fn cleanup(&self, spec_id: &str) -> BackendResult<()>;
}
