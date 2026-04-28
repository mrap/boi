use crate::fmt::ensure_db_dir;
use crate::{hooks, queue, worktree};
use serde_json::json;

pub fn cmd_cancel(spec_id: &str, db_str: &str, hook_cfg: &hooks::HookConfig) {
    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    match q.status(spec_id) {
        Ok(None) => {
            eprintln!("error: spec '{}' not found", spec_id);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
        Ok(Some(_)) => {}
    }

    if let Err(e) = q.cancel(spec_id) {
        eprintln!("error: cancel failed: {}", e);
        std::process::exit(1);
    }

    let _ = worktree::cleanup(spec_id);

    let payload = json!({ "spec_id": spec_id });
    let _ = hooks::fire(hook_cfg, hooks::ON_CANCEL, &payload);

    println!("cancelled {}", spec_id);
}
