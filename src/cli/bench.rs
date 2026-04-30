use crate::fmt::{ensure_db_dir, BOLD, CYAN, GREEN, RESET};
use crate::{queue, spec};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

#[derive(serde::Deserialize)]
struct PipelineTomlFile {
    pipeline: PipelineToml,
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct PipelineToml {
    pub name: String,
    #[serde(default)]
    pub spec_phases: Vec<String>,
    #[serde(default)]
    pub task_phases: Vec<String>,
    #[serde(default)]
    pub post_phases: Vec<String>,
}

pub fn load_pipeline_config(path: &Path) -> PipelineToml {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read pipeline config {:?}: {e}", path);
            std::process::exit(1);
        }
    };
    match toml::from_str::<PipelineTomlFile>(&content) {
        Ok(f) => f.pipeline,
        Err(e) => {
            eprintln!("error: invalid pipeline config {:?}: {e}", path);
            std::process::exit(1);
        }
    }
}

pub fn cmd_bench(
    spec_paths: &[PathBuf],
    pipelines: &[(String, PathBuf)],
    runs: u32,
    db_str: &str,
    json: bool,
) {
    let pipeline_configs: Vec<(String, PipelineToml)> = pipelines
        .iter()
        .map(|(name, path)| (name.clone(), load_pipeline_config(path)))
        .collect();

    let total_runs = spec_paths.len() * pipeline_configs.len() * runs as usize;
    println!(
        "{BOLD}BATTERY: {} specs × {} pipelines × {} runs = {} total runs{RESET}",
        spec_paths.len(),
        pipeline_configs.len(),
        runs,
        total_runs
    );

    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {e}");
            std::process::exit(1);
        }
    };

    let run_id = {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        format!("bench-{ts}")
    };

    let mut results: Vec<queue::BenchResultRecord> = Vec::new();

    for spec_path in spec_paths {
        let content = match std::fs::read_to_string(spec_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: skipping {:?}: {e}", spec_path);
                continue;
            }
        };

        for (pipeline_name, pipeline_cfg) in &pipeline_configs {
            for run_num in 1..=runs {
                print!(
                    "\n  Running [{pipeline_name}] {} run {run_num}/{runs}...",
                    spec_path.file_name().unwrap_or_default().to_string_lossy()
                );
                let _ = std::io::stdout().flush();

                let result = run_one(
                    &q,
                    &content,
                    spec_path,
                    pipeline_name,
                    pipeline_cfg,
                    run_num as i64,
                    &run_id,
                );
                let _ = q.insert_bench_result(&result);
                results.push(result);
            }
        }
    }

    println!();
    print_summary(
        &results,
        &pipeline_configs.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
        json,
    );
}

fn run_one(
    q: &queue::Queue,
    spec_content: &str,
    spec_path: &Path,
    pipeline_name: &str,
    pipeline_cfg: &PipelineToml,
    run_num: i64,
    run_id: &str,
) -> queue::BenchResultRecord {
    let spec_file = spec_path.to_string_lossy().to_string();

    let mut modified = spec_content.to_string();
    if !pipeline_cfg.spec_phases.is_empty() {
        modified = inject_yaml_list(&modified, "spec_phases", &pipeline_cfg.spec_phases);
    }
    if !pipeline_cfg.task_phases.is_empty() {
        modified = inject_yaml_list(&modified, "task_phases", &pipeline_cfg.task_phases);
    }
    if !pipeline_cfg.post_phases.is_empty() {
        modified = inject_yaml_list(&modified, "post_phases", &pipeline_cfg.post_phases);
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let tmp_path = std::env::temp_dir().join(format!(
        "boi_bench_{pipeline_name}_{run_num}_{ts}.yaml"
    ));

    let fail = |status: &str, elapsed_ms: i64| queue::BenchResultRecord {
        run_id: run_id.to_string(),
        pipeline: pipeline_name.to_string(),
        spec_file: spec_file.clone(),
        run_number: run_num,
        status: status.to_string(),
        total_ms: elapsed_ms,
        tasks_total: 0,
        tasks_done: 0,
        tasks_failed: 0,
        total_cost_usd: None,
        total_input_tokens: None,
        total_output_tokens: None,
        tasks_skipped: 0,
    };

    if let Err(e) = std::fs::write(&tmp_path, &modified) {
        eprintln!(" error writing temp: {e}");
        return fail("error", 0);
    }

    let parsed = match spec::parse(&modified) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(" spec invalid: {e}");
            let _ = std::fs::remove_file(&tmp_path);
            return fail("invalid-spec", 0);
        }
    };

    let spec_id = match q.enqueue(&parsed, tmp_path.to_str()) {
        Ok(id) => id,
        Err(e) => {
            eprintln!(" enqueue failed: {e}");
            let _ = std::fs::remove_file(&tmp_path);
            return fail("enqueue-error", 0);
        }
    };

    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(7200);

    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));

        if start.elapsed() > timeout {
            let _ = std::fs::remove_file(&tmp_path);
            eprintln!(" timed out");
            return fail("timeout", start.elapsed().as_millis() as i64);
        }

        let st = q.status(&spec_id).ok().flatten();
        if is_terminal(st.as_ref()) {
            let elapsed_ms = start.elapsed().as_millis() as i64;
            let status_str = st
                .as_ref()
                .map(|s| s.spec.status.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let tasks_total = st.as_ref().map(|s| s.tasks.len() as i64).unwrap_or(0);
            let tasks_done = st
                .as_ref()
                .map(|s| s.tasks.iter().filter(|t| t.status == "DONE").count() as i64)
                .unwrap_or(0);
            let tasks_failed = st
                .as_ref()
                .map(|s| {
                    s.tasks
                        .iter()
                        .filter(|t| t.status == "FAILED")
                        .count() as i64
                })
                .unwrap_or(0);

            let tasks_skipped = st
                .as_ref()
                .map(|s| s.tasks.iter().filter(|t| t.status == "SKIPPED").count() as i64)
                .unwrap_or(0);

            let (total_cost_usd, total_input_tokens, total_output_tokens) =
                q.aggregate_spec_cost(&spec_id).unwrap_or((None, None, None));

            println!(" {status_str} ({:.1}s)", elapsed_ms as f64 / 1000.0);
            let _ = std::fs::remove_file(&tmp_path);
            return queue::BenchResultRecord {
                run_id: run_id.to_string(),
                pipeline: pipeline_name.to_string(),
                spec_file,
                run_number: run_num,
                status: status_str,
                total_ms: elapsed_ms,
                tasks_total,
                tasks_done,
                tasks_failed,
                total_cost_usd,
                total_input_tokens,
                total_output_tokens,
                tasks_skipped,
            };
        }

        print!(
            "\r  Running [{pipeline_name}] {} run {run_num}: {:.0}s elapsed   ",
            spec_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            start.elapsed().as_secs_f64()
        );
        let _ = std::io::stdout().flush();
    }
}

struct PipelineMetrics {
    name: String,
    avg_completion_ms: f64,
    completion_rate: f64,
    tasks_done_avg: f64,
    tasks_total_avg: f64,
    tasks_failed_avg: f64,
}

fn compute_pipeline_metrics(
    results: &[queue::BenchResultRecord],
    pipeline_names: &[String],
) -> Vec<PipelineMetrics> {
    pipeline_names
        .iter()
        .map(|name| {
            let runs: Vec<&queue::BenchResultRecord> =
                results.iter().filter(|r| &r.pipeline == name).collect();
            let n = runs.len().max(1);

            let times: Vec<f64> = runs
                .iter()
                .filter(|r| r.total_ms > 0)
                .map(|r| r.total_ms as f64)
                .collect();
            let avg_completion_ms = if times.is_empty() {
                0.0
            } else {
                times.iter().sum::<f64>() / times.len() as f64
            };

            let completed = runs.iter().filter(|r| r.status == "completed").count();
            let completion_rate = if runs.is_empty() {
                0.0
            } else {
                completed as f64 / runs.len() as f64
            };

            let tasks_done_avg = runs.iter().map(|r| r.tasks_done as f64).sum::<f64>() / n as f64;
            let tasks_total_avg =
                runs.iter().map(|r| r.tasks_total as f64).sum::<f64>() / n as f64;
            let tasks_failed_avg =
                runs.iter().map(|r| r.tasks_failed as f64).sum::<f64>() / n as f64;

            PipelineMetrics {
                name: name.clone(),
                avg_completion_ms,
                completion_rate,
                tasks_done_avg,
                tasks_total_avg,
                tasks_failed_avg,
            }
        })
        .collect()
}

fn print_summary(
    results: &[queue::BenchResultRecord],
    pipeline_names: &[String],
    json: bool,
) {
    if results.is_empty() {
        if json {
            println!("{{\"error\":\"no results\"}}");
        } else {
            println!("No results.");
        }
        return;
    }

    let metrics = compute_pipeline_metrics(results, pipeline_names);

    let best_speed = metrics
        .iter()
        .filter(|m| m.avg_completion_ms > 0.0)
        .min_by(|a, b| {
            a.avg_completion_ms
                .partial_cmp(&b.avg_completion_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|m| m.name.clone());

    let best_quality = metrics
        .iter()
        .max_by(|a, b| {
            a.completion_rate
                .partial_cmp(&b.completion_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|m| m.name.clone());

    if json {
        let pipelines_json: Vec<serde_json::Value> = metrics
            .iter()
            .map(|m| {
                serde_json::json!({
                    "name": m.name,
                    "avg_completion_ms": m.avg_completion_ms as i64,
                    "completion_rate_pct": (m.completion_rate * 100.0).round() as i64,
                    "tasks_done_avg": (m.tasks_done_avg * 10.0).round() / 10.0,
                    "tasks_total_avg": (m.tasks_total_avg * 10.0).round() / 10.0,
                    "tasks_failed_avg": (m.tasks_failed_avg * 10.0).round() / 10.0,
                })
            })
            .collect();

        let output = serde_json::json!({
            "run_id": results.first().map(|r| r.run_id.as_str()).unwrap_or(""),
            "total_runs": results.len(),
            "pipelines": pipelines_json,
            "winners": {
                "speed": best_speed,
                "quality": best_quality,
            }
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap_or_default());
        return;
    }

    println!("{BOLD}{CYAN}Bench Results{RESET}\n");

    const COL: usize = 12;
    let metric_w = 22usize;

    print!("  {:<metric_w$}", "METRIC");
    for m in &metrics {
        print!("  {:>COL$}", m.name);
    }
    println!();
    let sep_len = metric_w + (COL + 2) * metrics.len() + 2;
    println!("  {}", "─".repeat(sep_len));

    // Avg completion time
    print!("  {:<metric_w$}", "Avg completion");
    for m in &metrics {
        let val = if m.avg_completion_ms > 0.0 {
            fmt_ms(m.avg_completion_ms as i64)
        } else {
            "—".to_string()
        };
        print!("  {:>COL$}", val);
    }
    println!();

    // Completion rate
    print!("  {:<metric_w$}", "Completion rate");
    for m in &metrics {
        let pipeline_runs: Vec<&queue::BenchResultRecord> =
            results.iter().filter(|r| r.pipeline == m.name).collect();
        let val = if pipeline_runs.is_empty() {
            "—".to_string()
        } else {
            format!("{:.0}%", m.completion_rate * 100.0)
        };
        print!("  {:>COL$}", val);
    }
    println!();

    // Tasks completed (avg done / avg total)
    print!("  {:<metric_w$}", "Tasks completed");
    for m in &metrics {
        let val = if m.tasks_total_avg == 0.0 {
            "—".to_string()
        } else {
            format!("{:.1}/{:.0}", m.tasks_done_avg, m.tasks_total_avg)
        };
        print!("  {:>COL$}", val);
    }
    println!();

    // Tasks failed avg
    print!("  {:<metric_w$}", "Tasks failed");
    for m in &metrics {
        let val = if m.tasks_total_avg == 0.0 {
            "—".to_string()
        } else {
            format!("{:.1}", m.tasks_failed_avg)
        };
        print!("  {:>COL$}", val);
    }
    println!();

    println!("  {}", "─".repeat(sep_len));

    // Winners
    if let Some(ref name) = best_quality {
        println!("  {GREEN}Best quality: {name}{RESET}");
    }
    if let Some(ref name) = best_speed {
        println!("  {GREEN}Best speed:   {name}{RESET}");
    }
}

/// Inject or replace a YAML list key at the top level.
fn inject_yaml_list(yaml_content: &str, key: &str, values: &[String]) -> String {
    let values_str = values
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let new_line = format!("{key}: [{values_str}]\n");

    let lines: Vec<&str> = yaml_content.lines().collect();
    let mut out = String::new();
    let mut replaced = false;
    let mut i = 0;

    while i < lines.len() {
        let l = lines[i];
        if l.trim_start().starts_with(&format!("{key}:")) && !replaced {
            i += 1;
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


/// Benchmark a single phase in isolation across N runs.
pub fn cmd_bench_phase(phase_name: &str, spec_path: &Path, runs: u32) {
    let registry = crate::phases::PhaseRegistry::new();
    let phase = match registry.get(phase_name) {
        Some(p) => p.clone(),
        None => {
            let available: Vec<&str> = registry.list().iter().map(|p| p.name.as_str()).collect();
            eprintln!("error: unknown phase '{phase_name}'");
            eprintln!("available: {}", available.join(", "));
            std::process::exit(1);
        }
    };

    let spec_content = match std::fs::read_to_string(spec_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read spec {:?}: {e}", spec_path);
            std::process::exit(1);
        }
    };

    println!(
        "{BOLD}Phase bench: {phase_name}{RESET}  spec={} runs={runs}",
        spec_path.display()
    );

    let claude_bin =
        std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
    let worktree_path = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let timeout_secs = phase.timeout_minutes.unwrap_or(10) as u64 * 60;

    let mut times_ms: Vec<u64> = Vec::new();
    let mut output_lens: Vec<usize> = Vec::new();
    let mut verdicts: Vec<String> = Vec::new();

    for run_num in 1..=runs {
        print!("  run {run_num}/{runs}...");
        let _ = std::io::stdout().flush();

        let prompt = crate::phases::build_phase_prompt(
            &phase,
            &spec_content,
            None,
            &HashMap::new(),
        );

        match crate::worker::spawn_claude(
            &prompt,
            &worktree_path,
            timeout_secs,
            phase.model.as_deref(),
            None,
            &claude_bin,
        ) {
            Ok(cr) => {
                let verdict = crate::phases::parse_phase_output(&phase, &cr.output);
                let verdict_str = match verdict {
                    crate::phases::Verdict::Proceed => "proceed",
                    crate::phases::Verdict::Redo { .. } => "redo",
                    crate::phases::Verdict::Pause { .. } => "pause",
                    crate::phases::Verdict::Done { success: true, .. } => "done-ok",
                    crate::phases::Verdict::Done { success: false, .. } => "done-fail",
                };
                println!(
                    " {verdict_str} ({:.1}s, {} chars)",
                    cr.total_ms as f64 / 1000.0,
                    cr.output.len()
                );
                times_ms.push(cr.total_ms);
                output_lens.push(cr.output.len());
                verdicts.push(verdict_str.to_string());
            }
            Err(e) => {
                println!(" error: {e}");
                times_ms.push(0);
                output_lens.push(0);
                verdicts.push("error".to_string());
            }
        }
    }

    println!();
    print_phase_summary(phase_name, &times_ms, &output_lens, &verdicts);
}

fn print_phase_summary(
    phase_name: &str,
    times_ms: &[u64],
    output_lens: &[usize],
    verdicts: &[String],
) {
    if times_ms.is_empty() {
        println!("No results.");
        return;
    }

    println!("{BOLD}{CYAN}Phase Bench: {phase_name}{RESET}\n");

    let n = times_ms.len() as f64;
    let avg_ms = times_ms.iter().sum::<u64>() as f64 / n;
    let min_ms = *times_ms.iter().min().unwrap_or(&0) as f64;
    let max_ms = *times_ms.iter().max().unwrap_or(&0) as f64;
    let p95_ms = percentile_ms(times_ms, 95);
    let avg_len = output_lens.iter().sum::<usize>() as f64 / n;
    let proceed_count = verdicts.iter().filter(|v| v.as_str() == "proceed").count();
    let proceed_pct = proceed_count as f64 / verdicts.len() as f64 * 100.0;

    const LW: usize = 18;
    println!("  {:<LW$}  VALUE", "METRIC");
    println!("  {}", "─".repeat(36));
    println!("  {:<LW$}  {}", "Avg time", fmt_ms(avg_ms as i64));
    println!("  {:<LW$}  {}", "Min time", fmt_ms(min_ms as i64));
    println!("  {:<LW$}  {}", "Max time", fmt_ms(max_ms as i64));
    println!("  {:<LW$}  {}", "p95 time", fmt_ms(p95_ms as i64));
    println!("  {:<LW$}  {avg_len:.0}", "Avg output chars");
    println!(
        "  {:<LW$}  {proceed_pct:.0}% ({proceed_count}/{})",
        "Proceed rate",
        verdicts.len()
    );
    println!("  {}", "─".repeat(36));
}

fn percentile_ms(values: &[u64], pct: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let idx = ((pct as f64 / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)] as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("boi_bench_{name}_{ts}.toml"))
    }

    #[test]
    fn test_bench_pipeline_config_full() {
        let toml = r#"
[pipeline]
name = "v2"
spec_phases = ["spec-critique", "spec-improve"]
task_phases = ["execute", "review", "commit"]
post_phases = ["doc-update", "critic", "merge"]
"#;
        let path = tmp_path("full");
        std::fs::write(&path, toml).unwrap();
        let cfg = load_pipeline_config(&path);
        assert_eq!(cfg.name, "v2");
        assert_eq!(cfg.spec_phases, vec!["spec-critique", "spec-improve"]);
        assert_eq!(cfg.task_phases, vec!["execute", "review", "commit"]);
        assert_eq!(cfg.post_phases, vec!["doc-update", "critic", "merge"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_bench_pipeline_config_defaults() {
        let toml = "[pipeline]\nname = \"minimal\"\n";
        let path = tmp_path("min");
        std::fs::write(&path, toml).unwrap();
        let cfg = load_pipeline_config(&path);
        assert_eq!(cfg.name, "minimal");
        assert!(cfg.spec_phases.is_empty());
        assert!(cfg.task_phases.is_empty());
        assert!(cfg.post_phases.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_bench_pipeline_config_v1() {
        let toml = r#"
[pipeline]
name = "v1"
spec_phases = ["critic"]
task_phases = ["execute", "task-verify"]
post_phases = []
"#;
        let path = tmp_path("v1");
        std::fs::write(&path, toml).unwrap();
        let cfg = load_pipeline_config(&path);
        assert_eq!(cfg.name, "v1");
        assert_eq!(cfg.spec_phases, vec!["critic"]);
        assert_eq!(cfg.task_phases, vec!["execute", "task-verify"]);
        assert!(cfg.post_phases.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_inject_yaml_list_replace() {
        let yaml = "title: foo\ntask_phases: [old]\nother: bar\n";
        let result = inject_yaml_list(yaml, "task_phases", &["execute".into(), "review".into()]);
        assert!(result.contains("task_phases: [\"execute\", \"review\"]"));
        assert!(!result.contains("old"));
    }

    #[test]
    fn test_inject_yaml_list_append() {
        let yaml = "title: foo\n";
        let result = inject_yaml_list(yaml, "task_phases", &["execute".into()]);
        assert!(result.contains("task_phases: [\"execute\"]"));
    }
}
