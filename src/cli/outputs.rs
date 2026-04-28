use crate::config;

pub fn cmd_outputs(spec_id: &str, cfg: &config::Config) {
    let worktrees_dir = cfg.worktrees_dir();
    let spec_wt = worktrees_dir.join(spec_id);
    let logs_dir = cfg.logs_dir().join(spec_id);

    let mut found = false;

    if spec_wt.exists() {
        println!("worktree: {}", spec_wt.display());
        found = true;
    }

    if logs_dir.exists() {
        println!("logs: {}", logs_dir.display());
        if let Ok(entries) = std::fs::read_dir(&logs_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                println!("  {}", entry.path().display());
            }
        }
        found = true;
    }

    if !found {
        println!("no outputs found for {}", spec_id);
    }
}
