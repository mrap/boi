use crate::failure::FailureReason;
use crate::fmt::ensure_db_dir;
use crate::queue;

pub fn cmd_why(spec_id: &str, db_str: &str) {
    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    let st = match q.status(spec_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("error: spec '{}' not found", spec_id);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    match st.spec.error.as_deref() {
        Some(e) if !e.is_empty() => {
            let reason = FailureReason::from_db(e);
            println!("{}", reason.detail());
        }
        _ => {
            println!("No failure recorded for spec {}.", spec_id);
            println!("Status: {}", st.spec.status);
        }
    }
}
