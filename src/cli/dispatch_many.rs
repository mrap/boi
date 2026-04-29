use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;

use crate::cli::plan::{
    build_dag, critique_dag, load_extra_spec_files, load_in_flight_specs, render_dag_text,
    Concern, DagError, Severity, SpecDag,
};
use crate::fmt::ensure_db_dir;
use crate::{hooks, queue, spec};
use serde_json::json;

// ─────────────────────────────────────────────────────────────────────────────
// Pure helpers (testable without I/O)
// ─────────────────────────────────────────────────────────────────────────────

/// Returns true when concerns contain at least one block-level entry.
/// `--force` does NOT override blocks; only the user can resolve them.
pub fn has_block(concerns: &[Concern]) -> bool {
    concerns.iter().any(|c| c.severity == Severity::Block)
}

/// Build the `--after` chain for a set of specs in topological order.
///
/// `id_map` maps plan IDs (e.g. `"new:foo"` or `"S0ABC"`) to the actual queue
/// IDs that were assigned when each spec was dispatched.  For in-flight specs
/// the plan ID is already the queue ID.
///
/// Returns a map: plan_id → comma-separated after string (empty if no deps).
pub fn compute_after_chain(
    dag: &SpecDag,
    order: &[String],
    id_map: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for plan_id in order {
        let node = match dag.nodes.get(plan_id) {
            Some(n) => n,
            None => continue,
        };
        let after_ids: Vec<String> = node
            .all_deps()
            .filter_map(|dep| id_map.get(dep).filter(|s| !s.is_empty()))
            .cloned()
            .collect();
        result.insert(plan_id.clone(), after_ids.join(","));
    }
    result
}

// ─────────────────────────────────────────────────────────────────────────────
// Single-spec dispatch helper
// ─────────────────────────────────────────────────────────────────────────────

fn enqueue_spec(
    spec_path: &PathBuf,
    after: Option<&str>,
    priority: i64,
    mode: Option<&str>,
    max_iter: i64,
    timeout: u32,
    project: Option<&str>,
    db_str: &str,
    hook_cfg: &hooks::HookConfig,
) -> Result<String, String> {
    let content = std::fs::read_to_string(spec_path)
        .map_err(|e| format!("cannot read {:?}: {}", spec_path, e))?;

    let boi_spec = spec::parse(&content)
        .map_err(|e| format!("spec parse failed: {}", e))?;

    ensure_db_dir(db_str);
    let q = queue::Queue::open(db_str)
        .map_err(|e| format!("cannot open queue: {}", e))?;

    let spec_path_str = spec_path.to_str().unwrap_or("");
    let spec_id = q
        .enqueue(&boi_spec, Some(spec_path_str))
        .map_err(|e| format!("enqueue failed: {}", e))?;

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
        if !dep.is_empty() {
            let _ = q.set_depends_on(&spec_id, dep);
        }
    }

    let payload = json!({
        "spec_id": spec_id,
        "title": boi_spec.title,
        "spec_path": spec_path_str,
    });
    let _ = hooks::fire(hook_cfg, hooks::ON_DISPATCH, &payload);

    Ok(spec_id)
}

// ─────────────────────────────────────────────────────────────────────────────
// boi dispatch-many command
// ─────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn cmd_dispatch_many(
    spec_paths: &[PathBuf],
    yes: bool,
    force: bool,
    priority: i64,
    mode: Option<&str>,
    max_iter: i64,
    timeout: u32,
    project: Option<&str>,
    db_str: &str,
    hook_cfg: &hooks::HookConfig,
) -> i32 {
    if spec_paths.is_empty() {
        eprintln!("error: dispatch-many requires at least one spec file");
        return 1;
    }

    // 1. Load in-flight specs + new spec files
    let in_flight = load_in_flight_specs(db_str);
    let new_specs = load_extra_spec_files(spec_paths);

    let mut all_specs = in_flight;
    all_specs.extend(new_specs);

    // 2. Build DAG — refuse loudly on cycle
    let dag = match build_dag(&all_specs) {
        Ok(d) => d,
        Err(DagError::Cycle(ids)) => {
            eprintln!("ERROR: dependency cycle detected: {}", ids.join(", "));
            eprintln!("Fix the cycle before dispatching.");
            return 1;
        }
    };

    let order = dag.topological_sort().expect("cycle already checked");

    // 3. Render and print proposed order
    let dag_text = render_dag_text(&dag, &order);
    println!("{dag_text}");

    // 4. LLM critique
    let concerns = critique_dag(&dag_text, &dag, &order, false);

    // 5. Print concerns
    if concerns.is_empty() {
        println!("LLM critique: no concerns.");
    } else {
        println!("LLM critique:");
        for c in &concerns {
            let label = match c.severity {
                Severity::Block => "[BLOCK]",
                Severity::Warn => "[WARN] ",
                Severity::Info => "[INFO] ",
            };
            println!("  {label} {}", c.description);
            if let Some(fix) = &c.fix {
                println!("         Fix: {fix}");
            }
        }
    }
    println!();

    // 6. Refuse on block-severity (force cannot override blocks)
    if has_block(&concerns) {
        eprintln!("Blocking concerns found — resolve before dispatching.");
        eprintln!("(--force overrides warns, not blocks)");
        return 1;
    }

    // 7. Prompt unless --yes / --force suppresses it
    let has_warn_concern = concerns.iter().any(|c| c.severity == Severity::Warn);
    if !yes && !force {
        let prompt_msg = if has_warn_concern {
            "Proceed with dispatch despite warnings? [y/N]: "
        } else {
            "Dispatch in the order above? [y/N]: "
        };
        eprint!("{prompt_msg}");
        let stdin = std::io::stdin();
        let mut input = String::new();
        let approved = stdin
            .lock()
            .read_line(&mut input)
            .is_ok()
            && input.trim().eq_ignore_ascii_case("y");
        if !approved {
            eprintln!("Aborted.");
            return 0;
        }
    } else if force && has_warn_concern {
        println!("--force: proceeding despite warn-level concerns.");
    }

    // 8. Dispatch in topological order with correct --after chain
    // Map plan_id ("new:<stem>") → original PathBuf
    let path_index: HashMap<String, &PathBuf> = spec_paths
        .iter()
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.to_string();
            Some((format!("new:{stem}"), p))
        })
        .collect();

    // Track dispatched plan_id → queue_id so later specs can reference deps
    let mut id_map: HashMap<String, String> = HashMap::new();

    for plan_id in &order {
        if !plan_id.starts_with("new:") {
            // Already in-flight; its plan_id IS the queue_id
            id_map.insert(plan_id.clone(), plan_id.clone());
            continue;
        }

        let path = match path_index.get(plan_id) {
            Some(p) => p,
            None => {
                eprintln!("warn: no path found for {plan_id}, skipping");
                continue;
            }
        };

        // Collect queue IDs of deps that were dispatched in this run
        let node = match dag.nodes.get(plan_id) {
            Some(n) => n,
            None => continue,
        };
        let after_ids: Vec<String> = node
            .all_deps()
            .filter_map(|dep| id_map.get(dep).filter(|s| !s.is_empty()))
            .cloned()
            .collect();
        let after_str = if after_ids.is_empty() {
            None
        } else {
            Some(after_ids.join(","))
        };

        match enqueue_spec(
            path,
            after_str.as_deref(),
            priority,
            mode,
            max_iter,
            timeout,
            project,
            db_str,
            hook_cfg,
        ) {
            Ok(queue_id) => {
                let after_display = after_str.as_deref().unwrap_or("(none)");
                println!("dispatched: {plan_id} → {queue_id}  --after {after_display}");
                id_map.insert(plan_id.clone(), queue_id);
            }
            Err(e) => {
                eprintln!("error: failed to dispatch {plan_id}: {e}");
                return 1;
            }
        }
    }

    0
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod dispatch_many {
    use super::*;
    use crate::cli::plan::{build_dag, SpecInfo};

    fn make_spec(id: &str, title: &str, depends_on: &[&str], texts: &[&str]) -> SpecInfo {
        SpecInfo {
            id: id.to_string(),
            title: title.to_string(),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            task_texts: texts
                .iter()
                .map(|t| (Some(t.to_string()), None))
                .collect(),
        }
    }

    fn pos_map(order: &[String]) -> HashMap<&str, usize> {
        order
            .iter()
            .enumerate()
            .map(|(i, s)| (s.as_str(), i))
            .collect()
    }

    /// Three-spec chain: S2 implicitly depends on S1 (shared file path),
    /// S3 explicitly depends on S2.  dispatch-many must produce the right
    /// --after chain: S2 → S001, S3 → S002.
    #[test]
    fn three_spec_implicit_then_explicit_after_chain() {
        let s1 = make_spec("S001", "Write foo", &[], &["Create src/foo.rs"]);
        let s2 = make_spec("S002", "Process foo", &[], &["Read src/foo.rs"]);
        let s3 = make_spec("S003", "Finalize", &["S002"], &[]);

        let dag = build_dag(&[s1, s2, s3]).unwrap();
        let order = dag.topological_sort().unwrap();
        let pos = pos_map(&order);

        assert!(pos["S001"] < pos["S002"], "S001 must precede S002");
        assert!(pos["S002"] < pos["S003"], "S002 must precede S003");

        // Simulate dispatch with identity id_map (plan_id == queue_id)
        let id_map: HashMap<String, String> =
            order.iter().map(|id| (id.clone(), id.clone())).collect();
        let chain = compute_after_chain(&dag, &order, &id_map);

        assert!(
            chain.get("S001").map(|s| s.is_empty()).unwrap_or(true),
            "S001 should have no --after, got {:?}",
            chain.get("S001")
        );
        assert_eq!(chain["S002"], "S001", "S002 should be after S001");
        assert_eq!(chain["S003"], "S002", "S003 should be after S002");
    }

    /// Cycle in declared deps must be detected and refused.
    #[test]
    fn cycle_causes_refusal() {
        let s1 = make_spec("S001", "A", &["S002"], &[]);
        let s2 = make_spec("S002", "B", &["S001"], &[]);
        assert!(
            matches!(build_dag(&[s1, s2]), Err(DagError::Cycle(_))),
            "cycle should be detected and returned as DagError::Cycle"
        );
    }

    /// --force can override warns but NOT blocks.
    /// We test `has_block` directly since it encodes the gate logic.
    #[test]
    fn force_overrides_warn_not_block() {
        let warn_concerns = vec![Concern {
            severity: Severity::Warn,
            description: "suboptimal ordering".into(),
            fix: None,
        }];
        let block_concerns = vec![Concern {
            severity: Severity::Block,
            description: "wrong ordering will cause data loss".into(),
            fix: None,
        }];

        // Warns never block dispatch (even without --force)
        assert!(
            !has_block(&warn_concerns),
            "warn-only concerns should not block"
        );
        // Blocks remain regardless of --force (caller checks has_block before respecting the flag)
        assert!(
            has_block(&block_concerns),
            "block concern must still block even if force=true"
        );
    }

    /// compute_after_chain returns empty string for root specs (no deps).
    #[test]
    fn root_specs_have_no_after() {
        let s1 = make_spec("S001", "Root A", &[], &[]);
        let s2 = make_spec("S002", "Root B", &[], &[]);

        let dag = build_dag(&[s1, s2]).unwrap();
        let order = dag.topological_sort().unwrap();
        let id_map: HashMap<String, String> =
            order.iter().map(|id| (id.clone(), id.clone())).collect();
        let chain = compute_after_chain(&dag, &order, &id_map);

        for id in &order {
            assert!(
                chain.get(id).map(|s| s.is_empty()).unwrap_or(true),
                "{id} should have empty --after (no deps)"
            );
        }
    }
}
