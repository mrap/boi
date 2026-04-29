use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use crate::queue::{FullTaskRecord, SpecRecord};

/// Lightweight view of a spec used for DAG analysis.
/// Constructed from DB records for in-flight/queued specs, or from test fixtures.
#[derive(Debug, Clone)]
pub struct SpecInfo {
    pub id: String,
    pub title: String,
    /// Explicit spec-level dependencies.  The DB stores a single spec ID in
    /// `depends_on`; we also accept a comma-separated list so callers can
    /// express multi-dep cases without changing the schema.
    pub depends_on: Vec<String>,
    /// (spec_content, verify_content) from each task in this spec.
    pub task_texts: Vec<(Option<String>, Option<String>)>,
}

impl SpecInfo {
    pub fn from_db(spec: &SpecRecord, tasks: &[FullTaskRecord]) -> Self {
        let depends_on = parse_depends_on(spec.depends_on.as_deref().unwrap_or(""));
        SpecInfo {
            id: spec.id.clone(),
            title: spec.title.clone(),
            depends_on,
            task_texts: tasks
                .iter()
                .map(|t| (t.spec_content.clone(), t.verify_content.clone()))
                .collect(),
        }
    }
}

fn parse_depends_on(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// A node in the spec-level DAG.
#[derive(Debug, Clone)]
pub struct DagNode {
    pub spec_id: String,
    pub title: String,
    /// Deps declared via --after / depends_on column.
    pub explicit_deps: Vec<String>,
    /// Deps inferred from artifact (file-path) overlap between specs.
    pub implicit_deps: Vec<String>,
}

impl DagNode {
    pub fn all_deps(&self) -> impl Iterator<Item = &str> {
        self.explicit_deps
            .iter()
            .chain(self.implicit_deps.iter())
            .map(String::as_str)
    }
}

/// The spec-level dependency graph.
#[derive(Debug)]
pub struct SpecDag {
    pub nodes: HashMap<String, DagNode>,
}

#[derive(Debug)]
pub enum DagError {
    Cycle(Vec<String>),
}

impl std::fmt::Display for DagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DagError::Cycle(ids) => {
                write!(f, "cycle detected involving specs: {}", ids.join(", "))
            }
        }
    }
}

impl std::error::Error for DagError {}

/// Extracts file-path-like strings from a spec's task text fields.
pub fn collect_artifacts(spec: &SpecInfo) -> Vec<PathBuf> {
    let mut paths: HashSet<PathBuf> = HashSet::new();
    for (spec_content, verify_content) in &spec.task_texts {
        for text in [spec_content.as_deref(), verify_content.as_deref()]
            .into_iter()
            .flatten()
        {
            extract_paths(text, &mut paths);
        }
    }
    paths.into_iter().collect()
}

fn extract_paths(text: &str, out: &mut HashSet<PathBuf>) {
    for word in text.split_whitespace() {
        let word = word.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':'
            )
        });
        if is_likely_path(word) {
            out.insert(PathBuf::from(word));
        }
    }
}

fn is_likely_path(s: &str) -> bool {
    if s.len() < 3 || !s.contains('/') {
        return false;
    }
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with("~/")
        || s.starts_with("src/")
        || s.starts_with("docs/")
        || s.starts_with("tests/")
}

/// Returns IDs of specs in `in_flight` whose artifact set overlaps with
/// `new_spec`'s artifacts, indicating an implicit dependency.
///
/// Skips any in-flight spec that explicitly declares a dep on `new_spec` —
/// that means the overlap runs in the other direction (in-flight is downstream),
/// so adding a reverse edge would create a spurious cycle.
pub fn detect_implicit_deps(new_spec: &SpecInfo, in_flight: &[SpecInfo]) -> Vec<String> {
    let new_artifacts: HashSet<PathBuf> = collect_artifacts(new_spec).into_iter().collect();
    if new_artifacts.is_empty() {
        return vec![];
    }
    in_flight
        .iter()
        .filter(|s| {
            // Don't add A→B if B already declares A as a dep (avoids contradiction cycles).
            if s.depends_on.contains(&new_spec.id) {
                return false;
            }
            let their: HashSet<PathBuf> = collect_artifacts(s).into_iter().collect();
            !their.is_disjoint(&new_artifacts)
        })
        .map(|s| s.id.clone())
        .collect()
}

/// Build a spec-level DAG from the supplied specs.
///
/// Edges come from:
/// 1. Explicit `depends_on` declarations (filtered to specs present in the set).
/// 2. Implicit artifact overlap detected by `detect_implicit_deps`.
///
/// Returns `Err(DagError::Cycle(...))` if a cycle is detected.
pub fn build_dag(specs: &[SpecInfo]) -> Result<SpecDag, DagError> {
    let known_ids: HashSet<&str> = specs.iter().map(|s| s.id.as_str()).collect();

    // First pass: create nodes with explicit deps (restricted to known IDs).
    let mut nodes: HashMap<String, DagNode> = specs
        .iter()
        .map(|spec| {
            let explicit_deps: Vec<String> = spec
                .depends_on
                .iter()
                .filter(|d| known_ids.contains(d.as_str()))
                .cloned()
                .collect();
            (
                spec.id.clone(),
                DagNode {
                    spec_id: spec.id.clone(),
                    title: spec.title.clone(),
                    explicit_deps,
                    implicit_deps: vec![],
                },
            )
        })
        .collect();

    // Second pass: detect implicit deps from artifact overlap.
    // We treat specs earlier in the slice as potential producers ("in-flight") and
    // each spec as a potential consumer ("new"), matching the directional semantics
    // of detect_implicit_deps. This avoids spurious symmetric cycles when two specs
    // share the same path string.
    for i in 1..specs.len() {
        let new_spec = &specs[i];
        let predecessors: Vec<SpecInfo> = specs[..i].iter().cloned().collect();

        let implicit = detect_implicit_deps(new_spec, &predecessors);
        if let Some(node) = nodes.get_mut(&new_spec.id) {
            for dep in implicit {
                if !node.explicit_deps.contains(&dep) && !node.implicit_deps.contains(&dep) {
                    node.implicit_deps.push(dep);
                }
            }
        }
    }

    let dag = SpecDag { nodes };
    // Validate — errors loudly on cycles.
    dag.topological_sort()?;
    Ok(dag)
}

impl SpecDag {
    /// Returns spec IDs in topological order (dependencies before dependents).
    /// Errors if a cycle is present.
    pub fn topological_sort(&self) -> Result<Vec<String>, DagError> {
        let mut in_degree: HashMap<&str, usize> =
            self.nodes.keys().map(|k| (k.as_str(), 0usize)).collect();

        let mut adj: HashMap<&str, Vec<&str>> =
            self.nodes.keys().map(|k| (k.as_str(), vec![])).collect();

        for (id, node) in &self.nodes {
            for dep in node.all_deps() {
                // Skip deps that are outside this DAG (already-completed specs).
                if !self.nodes.contains_key(dep) {
                    continue;
                }
                adj.get_mut(dep).expect("dep in nodes").push(id.as_str());
                *in_degree.get_mut(id.as_str()).expect("id in nodes") += 1;
            }
        }

        let mut queue: VecDeque<&str> = in_degree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&id, _)| id)
            .collect();

        let mut order: Vec<String> = Vec::with_capacity(self.nodes.len());
        while let Some(id) = queue.pop_front() {
            order.push(id.to_string());
            for &dependent in &adj[id] {
                let deg = in_degree.get_mut(dependent).expect("dep in in_degree");
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(dependent);
                }
            }
        }

        if order.len() != self.nodes.len() {
            let cyclic: Vec<String> = in_degree
                .iter()
                .filter(|(_, &d)| d > 0)
                .map(|(&id, _)| id.to_string())
                .collect();
            return Err(DagError::Cycle(cyclic));
        }

        Ok(order)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Concern types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Clone)]
pub enum Severity {
    Block,
    Warn,
    Info,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Block => write!(f, "block"),
            Severity::Warn => write!(f, "warn"),
            Severity::Info => write!(f, "info"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Concern {
    pub severity: Severity,
    pub description: String,
    pub fix: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// DB loading
// ─────────────────────────────────────────────────────────────────────────────

pub fn load_in_flight_specs(db_str: &str) -> Vec<SpecInfo> {
    use crate::queue::Queue;

    let q = match Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("warn: cannot open DB at {db_str}: {e}");
            return vec![];
        }
    };

    let all = match q.status_all() {
        Ok(recs) => recs,
        Err(e) => {
            eprintln!("warn: cannot query specs: {e}");
            return vec![];
        }
    };

    all.iter()
        .filter(|s| s.status == "queued" || s.status == "running")
        .filter_map(|spec| {
            let tasks = q.get_tasks_full(&spec.id).ok()?;
            Some(SpecInfo::from_db(spec, &tasks))
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Extra spec file loading
// ─────────────────────────────────────────────────────────────────────────────

pub fn load_extra_spec_files(paths: &[PathBuf]) -> Vec<SpecInfo> {
    paths
        .iter()
        .filter_map(|path| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("warn: cannot read {:?}: {}", path, e);
                    return None;
                }
            };
            let boi_spec = match crate::spec::parse(&content) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("warn: cannot parse {:?}: {}", path, e);
                    return None;
                }
            };
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            let task_texts: Vec<(Option<String>, Option<String>)> = boi_spec
                .tasks
                .iter()
                .map(|t| (t.spec.clone(), t.verify.clone()))
                .collect();

            Some(SpecInfo {
                id: format!("new:{id}"),
                title: boi_spec.title,
                depends_on: vec![],
                task_texts,
            })
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// DAG rendering
// ─────────────────────────────────────────────────────────────────────────────

pub fn render_dag_text(dag: &SpecDag, order: &[String]) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "SPEC DAG ({} spec{})\n",
        dag.nodes.len(),
        if dag.nodes.len() == 1 { "" } else { "s" }
    ));
    out.push_str(&"─".repeat(50));
    out.push('\n');

    for id in order {
        let node = match dag.nodes.get(id) {
            Some(n) => n,
            None => continue,
        };

        let mut dep_parts: Vec<String> = node
            .explicit_deps
            .iter()
            .map(|d| format!("{d} (explicit)"))
            .collect();
        dep_parts.extend(node.implicit_deps.iter().map(|d| format!("{d} (artifact)")));

        if dep_parts.is_empty() {
            out.push_str(&format!("  {id}  \"{}\"\n", node.title));
        } else {
            out.push_str(&format!(
                "  {id}  \"{}\"  → after {}\n",
                node.title,
                dep_parts.join(", ")
            ));
        }
    }

    out.push('\n');
    out.push_str("Proposed execution order:\n");
    for (i, id) in order.iter().enumerate() {
        let node = match dag.nodes.get(id) {
            Some(n) => n,
            None => continue,
        };

        let all_deps: Vec<&str> = node
            .explicit_deps
            .iter()
            .chain(node.implicit_deps.iter())
            .map(String::as_str)
            .collect();

        if all_deps.is_empty() {
            out.push_str(&format!("  {}. {id}  \"{}\"\n", i + 1, node.title));
        } else {
            out.push_str(&format!(
                "  {}. {id}  \"{}\"  --after {}\n",
                i + 1,
                node.title,
                all_deps.join(",")
            ));
        }
    }

    out
}

// ─────────────────────────────────────────────────────────────────────────────
// LLM critique
// ─────────────────────────────────────────────────────────────────────────────

fn build_critique_prompt(dag_text: &str) -> String {
    format!(
        r#"You are reviewing a BOI (Beginning of Infinity) spec DAG — a set of automated agent specs with dependency edges.

{dag_text}

Critique the DAG:
1. Are there specs that should depend on each other but don't (missing edges)?
2. Are specs wrongly serialized when they could run in parallel?
3. Do any specs have overlapping or contradicting scope?

Output ONLY a list of concerns in this exact format (one concern per pair of lines):
CONCERN [<SEVERITY>]: <description>
FIX: <suggested fix>

Where <SEVERITY> is one of: block, warn, info
  block = wrong ordering that will cause failures or data corruption
  warn  = suboptimal but unlikely to break things
  info  = observation, no action required

If there are no concerns, output exactly: NONE
"#
    )
}

/// FNV-1a 64-bit hash — deterministic across runs.
fn stable_hash(s: &str) -> u64 {
    let mut h: u64 = 14695981039346656037;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".boi").join("plan-cache")
}

fn load_cache(hash: u64) -> Option<String> {
    let path = cache_dir().join(format!("{hash:016x}.txt"));
    std::fs::read_to_string(path).ok()
}

fn save_cache(hash: u64, text: &str) {
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{hash:016x}.txt"));
    let _ = std::fs::write(path, text);
}

fn call_llm_critique(dag_text: &str, hash: u64) -> String {
    use crate::runtime::openrouter::OpenRouterRuntime;
    use crate::runtime::PhaseRuntime;

    let rt = OpenRouterRuntime::new();
    let prompt = build_critique_prompt(dag_text);

    match rt.execute(&prompt, "haiku", Duration::from_secs(60)) {
        Ok(out) => {
            let text = out.text.trim().to_string();
            save_cache(hash, &text);
            text
        }
        Err(e) => {
            eprintln!("warn: LLM critique unavailable: {e}");
            eprintln!("hint: set OPENROUTER_API_KEY to enable automatic DAG critique");
            "LLM_UNAVAILABLE".to_string()
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Concern parsing
// ─────────────────────────────────────────────────────────────────────────────

pub fn parse_concerns(text: &str) -> Vec<Concern> {
    let trimmed = text.trim();
    if trimmed == "NONE" || trimmed == "LLM_UNAVAILABLE" || trimmed.is_empty() {
        return vec![];
    }

    let mut concerns: Vec<Concern> = vec![];
    let mut current: Option<Concern> = None;

    for line in text.lines() {
        let line = line.trim();

        if let Some(rest) = line.strip_prefix("CONCERN [") {
            if let Some(c) = current.take() {
                concerns.push(c);
            }
            if let Some((sev_str, desc)) = rest.split_once("]:") {
                let severity = match sev_str.trim().to_lowercase().as_str() {
                    "block" => Severity::Block,
                    "warn" | "warning" => Severity::Warn,
                    _ => Severity::Info,
                };
                current = Some(Concern {
                    severity,
                    description: desc.trim().to_string(),
                    fix: None,
                });
            }
        } else if let Some(fix_text) = line.strip_prefix("FIX:") {
            if let Some(ref mut c) = current {
                c.fix = Some(fix_text.trim().to_string());
            }
        }
    }

    if let Some(c) = current {
        concerns.push(c);
    }

    concerns
}

// ─────────────────────────────────────────────────────────────────────────────
// Public critique helper (used by dispatch-many)
// ─────────────────────────────────────────────────────────────────────────────

/// Run the LLM critique on an already-rendered DAG text and return parsed concerns.
///
/// Uses the persistent cache keyed by DAG topology + titles.  Pass
/// `force_refresh = true` to bypass the cache.
pub fn critique_dag(
    dag_text: &str,
    dag: &SpecDag,
    order: &[String],
    force_refresh: bool,
) -> Vec<Concern> {
    let cache_input = {
        let mut parts: Vec<String> = order
            .iter()
            .map(|id| {
                let node = &dag.nodes[id];
                let mut deps = node.explicit_deps.clone();
                deps.extend_from_slice(&node.implicit_deps);
                deps.sort();
                format!("{id}:{};deps={}", node.title, deps.join(","))
            })
            .collect();
        parts.sort();
        parts.join("|")
    };
    let hash = stable_hash(&cache_input);

    let critique_text = if !force_refresh {
        match load_cache(hash) {
            Some(cached) => {
                eprintln!("(using cached LLM critique)");
                cached
            }
            None => call_llm_critique(dag_text, hash),
        }
    } else {
        call_llm_critique(dag_text, hash)
    };

    parse_concerns(&critique_text)
}

// ─────────────────────────────────────────────────────────────────────────────
// boi plan command
// ─────────────────────────────────────────────────────────────────────────────

/// Run `boi plan`: build DAG from in-flight specs + optional new spec files,
/// run LLM critique, and print proposed dispatch order.
///
/// Returns 0 on clean/warn, 1 on cycle or block-severity concern.
pub fn cmd_plan(extra_spec_paths: &[PathBuf], db_str: &str, force_refresh: bool) -> i32 {
    let mut specs = load_in_flight_specs(db_str);
    specs.extend(load_extra_spec_files(extra_spec_paths));

    if specs.is_empty() {
        println!("No in-flight specs and no spec files provided — nothing to plan.");
        return 0;
    }

    let dag = match build_dag(&specs) {
        Ok(d) => d,
        Err(DagError::Cycle(ids)) => {
            eprintln!("ERROR: cycle detected in spec DAG: {}", ids.join(", "));
            eprintln!("Fix the dependency cycle before dispatching.");
            return 1;
        }
    };

    // topological_sort is guaranteed to succeed after build_dag succeeds
    let order = dag.topological_sort().expect("cycle already checked");

    let dag_text = render_dag_text(&dag, &order);
    println!("{dag_text}");

    let concerns = critique_dag(&dag_text, &dag, &order, force_refresh);
    let has_block = concerns.iter().any(|c| c.severity == Severity::Block);

    if concerns.is_empty() {
        println!("LLM critique: no concerns.\n");
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
        println!();
    }

    if has_block {
        eprintln!("Blocking concerns found — resolve before dispatching.");
        eprintln!("Use --force in dispatch-many to override warns (not blocks).");
        return 1;
    }

    0
}

#[cfg(test)]
mod dag_build {
    use super::*;

    fn spec(id: &str, title: &str, depends_on: &[&str], texts: &[&str]) -> SpecInfo {
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
        order.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect()
    }

    #[test]
    fn empty_queue() {
        let dag = build_dag(&[]).unwrap();
        assert!(dag.nodes.is_empty());
        assert!(dag.topological_sort().unwrap().is_empty());
    }

    #[test]
    fn single_spec() {
        let dag = build_dag(&[spec("S001", "One", &[], &[])]).unwrap();
        assert_eq!(dag.nodes.len(), 1);
        assert_eq!(dag.topological_sort().unwrap(), vec!["S001"]);
    }

    #[test]
    fn two_spec_chain() {
        let specs = [
            spec("S001", "First", &[], &[]),
            spec("S002", "Second", &["S001"], &[]),
        ];
        let dag = build_dag(&specs).unwrap();
        let order = dag.topological_sort().unwrap();
        let pos = pos_map(&order);
        assert!(pos["S001"] < pos["S002"]);
    }

    #[test]
    fn fan_out() {
        let specs = [
            spec("S001", "Root", &[], &[]),
            spec("S002", "Branch A", &["S001"], &[]),
            spec("S003", "Branch B", &["S001"], &[]),
        ];
        let dag = build_dag(&specs).unwrap();
        let order = dag.topological_sort().unwrap();
        let pos = pos_map(&order);
        assert!(pos["S001"] < pos["S002"]);
        assert!(pos["S001"] < pos["S003"]);
    }

    #[test]
    fn diamond() {
        let specs = [
            spec("S001", "Root", &[], &[]),
            spec("S002", "Left", &["S001"], &[]),
            spec("S003", "Right", &["S001"], &[]),
            spec("S004", "Merge", &["S002", "S003"], &[]),
        ];
        let dag = build_dag(&specs).unwrap();
        let order = dag.topological_sort().unwrap();
        let pos = pos_map(&order);
        assert!(pos["S001"] < pos["S002"]);
        assert!(pos["S001"] < pos["S003"]);
        assert!(pos["S002"] < pos["S004"]);
        assert!(pos["S003"] < pos["S004"]);
    }

    #[test]
    fn cycle_detection() {
        let specs = [
            spec("S001", "A", &["S002"], &[]),
            spec("S002", "B", &["S001"], &[]),
        ];
        assert!(matches!(build_dag(&specs), Err(DagError::Cycle(_))));
    }

    #[test]
    fn implicit_dep_detection() {
        // S002 mentions the same path as S001 — should detect S001 as an implicit dep.
        let path = "src/cli/plan.rs";
        let s1 = spec("S001", "Write plan.rs", &[], &[&format!("Create {path}")]);
        let s2 = spec("S002", "Use plan.rs", &[], &[&format!("Read {path}")]);
        let implicit = detect_implicit_deps(&s2, &[s1]);
        assert!(
            implicit.contains(&"S001".to_string()),
            "expected S001 in implicit deps, got {:?}",
            implicit
        );
    }

    #[test]
    fn implicit_dep_wired_into_dag() {
        // Same as above but verify build_dag captures the implicit edge.
        let path = "src/cli/plan.rs";
        let s1 = spec("S001", "Write plan.rs", &[], &[&format!("Create {path}")]);
        let s2 = spec("S002", "Use plan.rs", &[], &[&format!("Read {path}")]);
        let dag = build_dag(&[s1, s2]).unwrap();
        let order = dag.topological_sort().unwrap();
        let pos = pos_map(&order);
        assert!(
            pos["S001"] < pos["S002"],
            "expected S001 before S002 via implicit dep, order={:?}",
            order
        );
    }

    #[test]
    fn no_false_implicit_deps_on_empty_artifacts() {
        // Specs with no recognizable paths should produce no implicit deps.
        let s1 = spec("S001", "Alpha", &[], &["do some work"]);
        let s2 = spec("S002", "Beta", &[], &["do other work"]);
        let implicit = detect_implicit_deps(&s2, &[s1]);
        assert!(implicit.is_empty());
    }

    #[test]
    fn collect_artifacts_recognizes_paths() {
        let s = SpecInfo {
            id: "X".into(),
            title: "t".into(),
            depends_on: vec![],
            task_texts: vec![(
                Some("Edit src/cli/plan.rs and ~/config.toml".into()),
                Some("cd /Users/mrap/boi && cargo test".into()),
            )],
        };
        let artifacts = collect_artifacts(&s);
        let strs: Vec<&str> = artifacts.iter().map(|p| p.to_str().unwrap()).collect();
        assert!(strs.iter().any(|s| *s == "src/cli/plan.rs"), "{:?}", strs);
        assert!(strs.iter().any(|s| *s == "~/config.toml"), "{:?}", strs);
    }
}
