use crate::fmt::{ensure_db_dir, BOLD, CYAN, GREEN, RESET};
use crate::{queue, spec};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::PathBuf;

pub fn cmd_bench(spec_path: &PathBuf, pipeline_a: &str, pipeline_b: &str, db_str: &str) {
    let content = match std::fs::read_to_string(spec_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read {:?}: {e}", spec_path);
            std::process::exit(1);
        }
    };

    match spec::parse(&content) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("error: spec validation failed: {e}");
            std::process::exit(1);
        }
    }

    let phases_a: Vec<String> = pipeline_a
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let phases_b: Vec<String> = pipeline_b
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if phases_a.is_empty() {
        eprintln!("error: --a pipeline cannot be empty");
        std::process::exit(1);
    }
    if phases_b.is_empty() {
        eprintln!("error: --b pipeline cannot be empty");
        std::process::exit(1);
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let tmp_dir = std::env::temp_dir();
    let path_a = tmp_dir.join(format!("boi_bench_a_{ts}.yaml"));
    let path_b = tmp_dir.join(format!("boi_bench_b_{ts}.yaml"));

    let yaml_a = inject_task_phases(&content, &phases_a);
    let yaml_b = inject_task_phases(&content, &phases_b);

    if let Err(e) = std::fs::write(&path_a, &yaml_a) {
        eprintln!("error: writing temp spec A: {e}");
        std::process::exit(1);
    }
    if let Err(e) = std::fs::write(&path_b, &yaml_b) {
        eprintln!("error: writing temp spec B: {e}");
        std::process::exit(1);
    }

    let spec_a = match spec::parse(&yaml_a) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: pipeline A spec invalid: {e}");
            let _ = std::fs::remove_file(&path_a);
            let _ = std::fs::remove_file(&path_b);
            std::process::exit(1);
        }
    };
    let spec_b = match spec::parse(&yaml_b) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: pipeline B spec invalid: {e}");
            let _ = std::fs::remove_file(&path_a);
            let _ = std::fs::remove_file(&path_b);
            std::process::exit(1);
        }
    };

    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {e}");
            std::process::exit(1);
        }
    };

    let id_a = match q.enqueue(&spec_a, path_a.to_str()) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: enqueue A failed: {e}");
            std::process::exit(1);
        }
    };
    let id_b = match q.enqueue(&spec_b, path_b.to_str()) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: enqueue B failed: {e}");
            std::process::exit(1);
        }
    };

    println!(
        "{BOLD}Bench dispatched:{RESET}\n  A [{id_a}]: {}\n  B [{id_b}]: {}",
        phases_a.join(" → "),
        phases_b.join(" → ")
    );
    println!("\nPolling for completion (Ctrl+C to abort, `boi status` to watch)...");

    let timeout = std::time::Duration::from_secs(7200);
    let start = std::time::Instant::now();

    loop {
        std::thread::sleep(std::time::Duration::from_secs(10));

        if start.elapsed() > timeout {
            eprintln!("\nerror: bench timed out after 2h");
            let _ = std::fs::remove_file(&path_a);
            let _ = std::fs::remove_file(&path_b);
            std::process::exit(1);
        }

        let st_a = q.status(&id_a).ok().flatten();
        let st_b = q.status(&id_b).ok().flatten();

        let done_a = is_terminal(st_a.as_ref());
        let done_b = is_terminal(st_b.as_ref());

        let sa = st_a.as_ref().map(|s| s.spec.status.as_str()).unwrap_or("?");
        let sb = st_b.as_ref().map(|s| s.spec.status.as_str()).unwrap_or("?");
        print!("\r  A: {sa:<12}  B: {sb:<12}  {:.0}s elapsed  ", start.elapsed().as_secs_f64());
        let _ = std::io::stdout().flush();

        if done_a && done_b {
            println!();
            print_comparison(&q, &id_a, &id_b, pipeline_a, pipeline_b);
            break;
        }
    }

    let _ = std::fs::remove_file(&path_a);
    let _ = std::fs::remove_file(&path_b);
}

/// Inject (or replace) the `task_phases` key in a YAML document.
/// Handles both inline (`task_phases: [...]`) and block list forms.
fn inject_task_phases(yaml_content: &str, phases: &[String]) -> String {
    let phases_str = phases
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let new_line = format!("task_phases: [{phases_str}]\n");

    let lines: Vec<&str> = yaml_content.lines().collect();
    let mut out = String::new();
    let mut replaced = false;
    let mut i = 0;

    while i < lines.len() {
        let l = lines[i];
        if l.trim_start().starts_with("task_phases:") && !replaced {
            i += 1;
            // Skip any indented continuation lines (block list / flow continuation)
            while i < lines.len()
                && (lines[i].starts_with(' ') || lines[i].starts_with('\t'))
            {
                i += 1;
            }
            out.push_str(&new_line);
            replaced = true;
        } else {
            out.push_str(l);
            out.push('\n');
            i += 1;
        }
    }

    if !replaced {
        out.push_str(&new_line);
    }
    out
}

fn is_terminal(st: Option<&queue::SpecStatus>) -> bool {
    st.map(|s| {
        matches!(
            s.spec.status.as_str(),
            "completed" | "failed" | "cancelled"
        )
    })
    .unwrap_or(false)
}

fn print_comparison(
    q: &queue::Queue,
    id_a: &str,
    id_b: &str,
    label_a: &str,
    label_b: &str,
) {
    println!("\n{BOLD}{CYAN}Bench Results{RESET}");
    println!("  A: {label_a}");
    println!("  B: {label_b}\n");

    let costs_a = q.phase_cost_summary(id_a).unwrap_or_default();
    let costs_b = q.phase_cost_summary(id_b).unwrap_or_default();

    let mut phases: Vec<String> = {
        let mut all: BTreeSet<String> = BTreeSet::new();
        for c in &costs_a {
            all.insert(c.phase.clone());
        }
        for c in &costs_b {
            all.insert(c.phase.clone());
        }
        all.into_iter().collect()
    };

    if phases.is_empty() {
        println!("  No phase_runs data — specs may still be queued or pending.");
        return;
    }

    // Silence unused warning if phases would be empty; sort for determinism
    phases.sort();

    const W: usize = 14;
    println!(
        "  {:<W$}  {:>8}  {:>8}  {:>12}  {:>12}",
        "PHASE", "A RUNS", "B RUNS", "A TIME", "B TIME"
    );
    let sep = "-".repeat(W + 2 + 8 + 2 + 8 + 2 + 12 + 2 + 12);
    println!("  {sep}");

    for phase in &phases {
        let a = costs_a.iter().find(|c| &c.phase == phase);
        let b = costs_b.iter().find(|c| &c.phase == phase);
        let a_runs = a.map(|c| c.count.to_string()).unwrap_or_else(|| "—".into());
        let b_runs = b.map(|c| c.count.to_string()).unwrap_or_else(|| "—".into());
        let a_time = a.map(|c| fmt_ms(c.total_duration_ms)).unwrap_or_else(|| "—".into());
        let b_time = b.map(|c| fmt_ms(c.total_duration_ms)).unwrap_or_else(|| "—".into());
        println!(
            "  {:<W$}  {:>8}  {:>8}  {:>12}  {:>12}",
            phase, a_runs, b_runs, a_time, b_time
        );
    }

    let total_ms_a: i64 = costs_a.iter().map(|c| c.total_duration_ms).sum();
    let total_ms_b: i64 = costs_b.iter().map(|c| c.total_duration_ms).sum();
    let total_cost_a = q.phase_cost_total(id_a).unwrap_or(0.0);
    let total_cost_b = q.phase_cost_total(id_b).unwrap_or(0.0);

    println!("  {sep}");
    println!(
        "  {:<W$}  {:>8}  {:>8}  {:>12}  {:>12}",
        "TOTAL",
        "",
        "",
        fmt_ms(total_ms_a),
        fmt_ms(total_ms_b)
    );
    if total_cost_a > 0.0 || total_cost_b > 0.0 {
        println!(
            "  {:<W$}  {:>8}  {:>8}  {:>12}  {:>12}",
            "COST",
            "",
            "",
            format!("${total_cost_a:.4}"),
            format!("${total_cost_b:.4}"),
        );
    }

    println!();
    if total_ms_a > 0 && total_ms_b > 0 {
        if total_ms_a < total_ms_b {
            let pct = (total_ms_b - total_ms_a) as f64 / total_ms_b as f64 * 100.0;
            println!("  {GREEN}Pipeline A is {pct:.0}% faster{RESET}");
        } else if total_ms_b < total_ms_a {
            let pct = (total_ms_a - total_ms_b) as f64 / total_ms_a as f64 * 100.0;
            println!("  {GREEN}Pipeline B is {pct:.0}% faster{RESET}");
        } else {
            println!("  Pipelines completed in equal time");
        }
    }
}

fn fmt_ms(ms: i64) -> String {
    if ms == 0 {
        "—".to_string()
    } else if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.0}s", ms as f64 / 1000.0)
    } else {
        format!("{:.1}m", ms as f64 / 60_000.0)
    }
}
