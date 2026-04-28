use crate::fmt::{ensure_db_dir, BOLD, CYAN, RESET};
use crate::queue;

pub fn cmd_telemetry(spec_id: &str, db_str: &str) {
    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    let iterations = match q.get_iterations(spec_id) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    if iterations.is_empty() {
        println!("no iteration records for {}", spec_id);
        return;
    }

    println!(
        "{}{}Iterations for {}{}\n",
        BOLD, CYAN, spec_id, RESET
    );
    println!("  {:>4}  {:>10}  {:>8}  {:>8}  {:>6}  DURATION", "ITER", "PHASE", "TASKS+", "DONE", "EXIT");
    println!("  {}", "-".repeat(60));

    for rec in &iterations {
        let phase = rec.phase.as_deref().unwrap_or("?");
        let duration = rec
            .duration_seconds
            .map(|d| {
                if d < 60.0 {
                    format!("{:.0}s", d)
                } else {
                    format!("{:.1}m", d / 60.0)
                }
            })
            .unwrap_or_else(|| "?".to_string());
        let exit = rec
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_string());

        println!(
            "  {:>4}  {:>10}  {:>8}  {:>8}  {:>6}  {}",
            rec.iteration, phase, rec.tasks_added, rec.tasks_completed, exit, duration
        );
    }
}
