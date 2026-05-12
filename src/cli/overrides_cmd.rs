use crate::phases::user_phases_dir;

pub fn cmd_overrides_list() {
    let dir = user_phases_dir();
    if !dir.is_dir() {
        println!("No phase overrides active.");
        return;
    }
    let entries: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .collect(),
        Err(_) => {
            println!("No phase overrides active.");
            return;
        }
    };
    if entries.is_empty() {
        println!("No phase overrides active.");
    } else {
        for entry in entries {
            println!("{}", entry.file_name().to_string_lossy());
        }
    }
}

pub fn cmd_overrides_clear(name: &str) {
    let path = user_phases_dir().join(name);
    if !path.exists() {
        eprintln!("error: override '{}' not found.", name);
        std::process::exit(1);
    }
    if let Err(e) = std::fs::remove_file(&path) {
        eprintln!("error: failed to remove '{}': {}", name, e);
        std::process::exit(1);
    }
    println!("Removed {}.", name);
}

pub fn cmd_overrides_clear_all() {
    let dir = user_phases_dir();
    if !dir.is_dir() {
        println!("Removed 0 overrides.");
        return;
    }
    let entries: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .collect(),
        Err(e) => {
            eprintln!("error: could not read overrides directory: {}", e);
            std::process::exit(1);
        }
    };
    let count = entries.len();
    for entry in entries {
        if let Err(e) = std::fs::remove_file(entry.path()) {
            eprintln!("warning: failed to remove {}: {}", entry.file_name().to_string_lossy(), e);
        }
    }
    println!("Removed {} override{}.", count, if count == 1 { "" } else { "s" });
}
