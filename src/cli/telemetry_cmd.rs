use crate::fmt::{ensure_db_dir, BOLD, CYAN, DIM, GREEN, RESET, YELLOW};
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

    if !iterations.is_empty() {
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

    // Phase cost breakdown
    let phase_costs: Vec<queue::PhaseCostSummary> = q.phase_cost_summary(spec_id).unwrap_or_default();

    if phase_costs.is_empty() && iterations.is_empty() {
        println!("no telemetry records for {}", spec_id);
        return;
    }

    if !phase_costs.is_empty() {
        println!(
            "\n{}{}Phase breakdown{}\n",
            BOLD, CYAN, RESET
        );
        println!("  {:>14}  {:>6}  {:>12}  {:>8}", "PHASE", "RUNS", "AVG DURATION", "COST");
        println!("  {}", "-".repeat(50));

        for summary in &phase_costs {
            let avg_ms = if summary.count > 0 {
                summary.total_duration_ms / summary.count
            } else {
                0
            };
            let avg_duration = if avg_ms < 1000 {
                format!("{}ms", avg_ms)
            } else if avg_ms < 60_000 {
                format!("{:.0}s", avg_ms as f64 / 1000.0)
            } else {
                let mins = avg_ms as f64 / 60_000.0;
                let secs = (avg_ms % 60_000) as f64 / 1000.0;
                format!("{}m {:.0}s", mins as i64, secs)
            };
            let cost = if summary.total_cost > 0.0 {
                format!("${:.4}", summary.total_cost)
            } else {
                format!("{}—{}", DIM, RESET)
            };
            println!(
                "  {:>14}  {:>6}  {:>12}  {:>8}",
                summary.phase, summary.count, avg_duration, cost
            );
        }

        let total_cost = q.phase_cost_total(spec_id).unwrap_or(0.0);
        if total_cost > 0.0 {
            println!(
                "\n  {}Total: {}${:.4}{}",
                BOLD, GREEN, total_cost, RESET
            );
        }

        let total_duration_ms: i64 = phase_costs.iter().map(|s| s.total_duration_ms).sum();
        if total_duration_ms > 0 {
            let total_dur = if total_duration_ms < 60_000 {
                format!("{:.1}s", total_duration_ms as f64 / 1000.0)
            } else {
                format!("{:.1}m", total_duration_ms as f64 / 60_000.0)
            };
            println!(
                "  {}Total time: {}{}{}",
                BOLD, YELLOW, total_dur, RESET
            );
        }
    }
}
