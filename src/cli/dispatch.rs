use crate::cli::plan;
use crate::fmt::ensure_db_dir;
use crate::{hooks, queue, spec};
use serde_json::json;
use std::path::PathBuf;

/// Convert a parsed BoiSpec into a SpecInfo for DAG artifact analysis.
fn boi_spec_to_spec_info(boi_spec: &spec::BoiSpec, path: &PathBuf) -> plan::SpecInfo {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    plan::SpecInfo {
        id,
        title: boi_spec.title.clone(),
        depends_on: vec![],
        task_texts: boi_spec
            .tasks
            .iter()
            .map(|t| (t.spec.clone(), t.verify.clone()))
            .collect(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_dispatch(
    spec_path: &PathBuf,
    after: Option<&str>,
    priority: i64,
    mode: Option<&str>,
    max_iter: i64,
    timeout: u32,
    _no_critic: bool,
    project: Option<&str>,
    dry_run: bool,
    _workspace: Option<&str>,
    skip_plan: bool,
    db_str: &str,
    hook_cfg: &hooks::HookConfig,
) {
    let content = match std::fs::read_to_string(spec_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read {:?}: {}", spec_path, e);
            std::process::exit(1);
        }
    };

    let boi_spec = match spec::parse(&content) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: spec validation failed: {}", e);
            std::process::exit(1);
        }
    };

    if dry_run {
        println!("spec valid: {} ({} tasks)", boi_spec.title, boi_spec.tasks.len());
        return;
    }

    ensure_db_dir(db_str);

    // Lightweight DAG check: warn if artifact overlap detected with in-flight specs
    // and no --after was provided. Does NOT block dispatch.
    if !skip_plan && after.is_none() {
        let in_flight = plan::load_in_flight_specs(db_str);
        if !in_flight.is_empty() {
            let new_info = boi_spec_to_spec_info(&boi_spec, spec_path);
            let implicit = plan::detect_implicit_deps(&new_info, &in_flight);
            if !implicit.is_empty() {
                eprintln!(
                    "warn: new spec may implicitly depend on in-flight spec(s): {}",
                    implicit.join(", ")
                );
                eprintln!(
                    "  Suggested: boi dispatch {} --after {}",
                    spec_path.display(),
                    implicit.join(",")
                );
                eprintln!("  Use --skip-plan to suppress this warning.");
            }
        }
    }

    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    let spec_path_str = spec_path.to_str().unwrap_or("");
    let spec_id = match q.enqueue(&boi_spec, Some(spec_path_str)) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: enqueue failed: {}", e);
            std::process::exit(1);
        }
    };

    // Apply CLI overrides via single queue connection
    let timeout_secs = if timeout != 30 {
        Some(timeout as i64 * 60)
    } else {
        None
    };
    if let Err(e) = q.set_spec_fields(
        &spec_id,
        mode,
        if max_iter != 30 { Some(max_iter) } else { None },
        project,
        timeout_secs,
    ) {
        eprintln!("[boi] ERROR: failed to set spec fields for {}: {}", spec_id, e);
    }

    if priority != 100 {
        if let Err(e) = q.set_priority(&spec_id, priority) {
            eprintln!("[boi] ERROR: failed to set priority for {}: {}", spec_id, e);
        }
    }

    if let Some(dep) = after {
        if let Err(e) = q.set_depends_on(&spec_id, dep) {
            eprintln!("[boi] ERROR: failed to set depends_on for {}: {}", spec_id, e);
        }
    }

    let payload = json!({
        "spec_id": spec_id,
        "title": boi_spec.title,
        "spec_path": spec_path_str,
    });
    let _ = hooks::fire(hook_cfg, hooks::ON_DISPATCH, &payload); // intentional: best-effort hook notification

    println!("{}", spec_id);
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod dispatch_dag_warn {
    use crate::cli::plan::{detect_implicit_deps, SpecInfo};

    fn make_spec(id: &str, depends_on: &[&str], texts: &[&str]) -> SpecInfo {
        SpecInfo {
            id: id.to_string(),
            title: format!("Spec {}", id),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            task_texts: texts
                .iter()
                .map(|t| (Some(t.to_string()), None))
                .collect(),
        }
    }

    /// No in-flight specs → no implicit deps detected.
    #[test]
    fn no_in_flight_no_warn() {
        let new_spec = make_spec("new:foo", &[], &["touch src/foo.rs"]);
        let deps = detect_implicit_deps(&new_spec, &[]);
        assert!(deps.is_empty(), "empty in-flight should produce no implicit deps");
    }

    /// In-flight spec shares an artifact path → implicit dep detected.
    #[test]
    fn shared_artifact_triggers_warn() {
        let in_flight = make_spec("S001", &[], &["create src/dag.rs and tests/dag_test.rs"]);
        let new_spec = make_spec("new:bar", &[], &["read src/dag.rs"]);
        let deps = detect_implicit_deps(&new_spec, &[in_flight]);
        assert!(
            deps.contains(&"S001".to_string()),
            "shared artifact should produce implicit dep on S001"
        );
    }

    /// Disjoint artifact sets → no implicit dep.
    #[test]
    fn disjoint_artifacts_no_warn() {
        let in_flight = make_spec("S001", &[], &["create src/alpha.rs"]);
        let new_spec = make_spec("new:bar", &[], &["write src/beta.rs"]);
        let deps = detect_implicit_deps(&new_spec, &[in_flight]);
        assert!(
            deps.is_empty(),
            "non-overlapping artifacts should produce no implicit deps"
        );
    }

    /// In-flight spec already declares new_spec as a dep → no reverse edge to
    /// avoid a spurious cycle warning.
    #[test]
    fn in_flight_already_depends_on_new_no_reverse_warn() {
        let mut in_flight = make_spec("S001", &[], &["work on src/shared.rs"]);
        in_flight.depends_on = vec!["new:foo".to_string()];
        let new_spec = make_spec("new:foo", &[], &["create src/shared.rs"]);
        let deps = detect_implicit_deps(&new_spec, &[in_flight]);
        assert!(
            deps.is_empty(),
            "should not add reverse edge when in-flight already depends on new spec"
        );
    }

    /// Multiple in-flight specs, only the one with overlapping artifact triggers.
    #[test]
    fn only_overlapping_in_flight_triggers() {
        let s1 = make_spec("S001", &[], &["create src/overlap.rs"]);
        let s2 = make_spec("S002", &[], &["create src/unrelated.rs"]);
        let new_spec = make_spec("new:baz", &[], &["use src/overlap.rs"]);
        let deps = detect_implicit_deps(&new_spec, &[s1, s2]);
        assert_eq!(deps, vec!["S001".to_string()], "only S001 shares an artifact");
    }

    /// New spec with no file-path artifacts → no implicit deps regardless.
    #[test]
    fn new_spec_no_artifacts_no_warn() {
        let in_flight = make_spec("S001", &[], &["create src/foo.rs"]);
        let new_spec = make_spec("new:empty", &[], &["run some commands without file paths"]);
        let deps = detect_implicit_deps(&new_spec, &[in_flight]);
        assert!(
            deps.is_empty(),
            "new spec without file-path artifacts should not trigger implicit dep"
        );
    }
}
