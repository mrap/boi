use crate::config;

pub fn cmd_log(spec_id: &str, full: bool, cfg: &config::Config) {
    let logs_dir = cfg.logs_dir();
    let spec_log_dir = logs_dir.join(spec_id);

    if !spec_log_dir.exists() {
        println!("no logs found for {}", spec_id);
        return;
    }

    let mut entries: Vec<_> = match std::fs::read_dir(&spec_log_dir) {
        Ok(e) => e.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            eprintln!("error reading log dir: {}", e);
            std::process::exit(1);
        }
    };

    entries.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());

    let log_file = if let Some(last) = entries.last() {
        last.path()
    } else {
        println!("no log files found for {}", spec_id);
        return;
    };

    let content = match std::fs::read_to_string(&log_file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error reading {}: {}", log_file.display(), e);
            std::process::exit(1);
        }
    };

    if full {
        print!("{}", content);
    } else {
        let lines: Vec<&str> = content.lines().collect();
        let start = if lines.len() > 50 { lines.len() - 50 } else { 0 };
        for line in &lines[start..] {
            println!("{}", line);
        }
    }
}
