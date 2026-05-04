// Shim: all logic lives in workspace::git::GitWorkspace. This module keeps
// the original free-function API so existing call sites in builtins.rs,
// phases.rs, and worker.rs compile unchanged until T8BB9 migrates them.

use crate::workspace::git::GitWorkspace;
use crate::workspace::WorkspaceBackend;
use std::path::PathBuf;

fn backend() -> GitWorkspace {
    GitWorkspace::new()
}

pub fn branch_name(spec_id: &str) -> String {
    crate::workspace::git::branch_name(spec_id)
}

pub fn create(spec_id: &str, repo_path: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    backend().create(spec_id, repo_path).map_err(|e| e.to_string().into())
}

pub fn commit_changes(spec_id: &str, message: &str) -> Result<bool, Box<dyn std::error::Error>> {
    backend().commit_changes(spec_id, message).map_err(|e| e.to_string().into())
}

pub fn merge_back(spec_id: &str, repo_path: &str) -> Result<String, Box<dyn std::error::Error>> {
    backend().merge_back(spec_id, repo_path).map_err(|e| e.to_string().into())
}

pub fn cleanup(spec_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    backend().cleanup(spec_id).map_err(|e| e.to_string().into())
}

pub fn delete_branch(spec_id: &str, repo_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    backend().delete_branch(spec_id, repo_path).map_err(|e| e.to_string().into())
}

pub fn cleanup_stale() -> Result<(), Box<dyn std::error::Error>> {
    backend().cleanup_stale().map_err(|e| e.to_string().into())
}
