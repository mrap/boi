use crate::fmt::ensure_db_dir;
use crate::{config, hooks, queue, spec};
use serde_json::json;
use std::path::PathBuf;

#[allow(clippy::too_many_arguments)]
pub fn cmd_dispatch(
    spec_path: &PathBuf,
    after: Option<&str>,
    priority: i64,
    mode: Option<&str>,
    max_iter: i64,
    timeout: u32,
    _no_critic: bool,
    project: Option<&str>,
    dry_run: bool,
    _workspace: Option<&str>,
    db_str: &str,
    hook_cfg: &hooks::HookConfig,
) {
    let content = match std::fs::read_to_string(spec_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read {:?}: {}", spec_path, e);
            std::process::exit(1);
        }
    };

    let boi_spec = match spec::parse(&content) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: spec validation failed: {}", e);
            std::process::exit(1);
        }
    };

    if dry_run {
        println!("spec valid: {} ({} tasks)", boi_spec.title, boi_spec.tasks.len());
        return;
    }

    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    // Build project_context from global config and per-spec context_files
    let cfg = config::load();
    let mut context_paths: Vec<String> = cfg
        .context
        .as_ref()
        .and_then(|c| c.always_include.as_ref())
        .cloned()
        .unwrap_or_default();
    if let Some(ref spec_files) = boi_spec.context_files {
        context_paths.extend(spec_files.iter().cloned());
    }
    let project_context = if context_paths.is_empty() {
        None
    } else {
        let content = queue::read_context_files(&context_paths);
        if content.is_empty() { None } else { Some(content) }
    };

    let spec_path_str = spec_path.to_str().unwrap_or("");
    let spec_id = match q.enqueue_with_context(&boi_spec, Some(spec_path_str), project_context.as_deref()) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: enqueue failed: {}", e);
            std::process::exit(1);
        }
    };

    // Apply CLI overrides via single queue connection
    let timeout_secs = if timeout != 30 {
        Some(timeout as i64 * 60)
    } else {
        None
    };
    if let Err(e) = q.set_spec_fields(
        &spec_id,
        mode,
        if max_iter != 30 { Some(max_iter) } else { None },
        project,
        timeout_secs,
    ) {
        eprintln!("[boi] ERROR: failed to set spec fields for {}: {}", spec_id, e);
    }

    if priority != 100 {
        if let Err(e) = q.set_priority(&spec_id, priority) {
            eprintln!("[boi] ERROR: failed to set priority for {}: {}", spec_id, e);
        }
    }

    if let Some(dep) = after {
        if let Err(e) = q.set_depends_on(&spec_id, dep) {
            eprintln!("[boi] ERROR: failed to set depends_on for {}: {}", spec_id, e);
        }
    }

    let payload = json!({
        "spec_id": spec_id,
        "title": boi_spec.title,
        "spec_path": spec_path_str,
    });
    let _ = hooks::fire(hook_cfg, hooks::ON_DISPATCH, &payload); // intentional: best-effort hook notification

    println!("{}", spec_id);
}
