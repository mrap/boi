use crate::fmt::ensure_db_dir;
use crate::{hooks, queue, spec};
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

    let spec_path_str = spec_path.to_str().unwrap_or("");
    let spec_id = match q.enqueue(&boi_spec, Some(spec_path_str)) {
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
    let _ = q.set_spec_fields(
        &spec_id,
        mode,
        if max_iter != 30 { Some(max_iter) } else { None },
        project,
        timeout_secs,
    );

    if priority != 100 {
        let _ = q.set_priority(&spec_id, priority);
    }

    if let Some(dep) = after {
        let _ = q.set_depends_on(&spec_id, dep);
    }

    let payload = json!({
        "spec_id": spec_id,
        "title": boi_spec.title,
        "spec_path": spec_path_str,
    });
    let _ = hooks::fire(hook_cfg, hooks::ON_DISPATCH, &payload);

    println!("{}", spec_id);
}
