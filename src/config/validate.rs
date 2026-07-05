//! Spec validation — every check that runs on a [`RawSpec`] after a clean
//! parse and before normalization to [`Spec`](crate::config::spec::Spec).
//!
//! Two classes of check live here:
//!
//! 1. **Structural rules** — required fields, the workspace XOR (§3.1), the
//!    verification intent/command mutex, ref uniqueness, dangling-dep
//!    detection, and `blocked_by` DAG-cycle detection.
//! 2. **Removed-field rejections** — `mode` (L2), `max_iterations` (S6),
//!    `clean_state` (S8), `initiative` (S17). `#[serde(deny_unknown_fields)]`
//!    already rejects *any* unrecognized field at parse time; these four are
//!    kept as named [`RawSpec`] fields purely so this layer can raise a
//!    *specific, actionable* error ("modes were removed in v1.0") instead of
//!    a generic "unknown field `mode`".

use std::collections::{HashMap, HashSet};

use crate::config::spec::{ConfigError, RawSpec, RawVerification};

/// Validate a parsed [`RawSpec`].
///
/// Returns the first rule violated, as a typed [`ConfigError`]. A clean
/// `Ok(())` means the spec is safe to normalize.
///
/// Check order is deliberate: the removed-field rejections run first (a spec
/// carrying `mode` is almost certainly a v1 spec — that's the most useful
/// thing to tell the author), then structural rules from coarse to fine.
pub fn validate(raw: &RawSpec) -> Result<(), ConfigError> {
    reject_removed_fields(raw)?;
    check_required_fields(raw)?;
    check_pipeline(raw)?;
    check_workspace_xor(raw)?;
    check_verifications(raw)?;
    check_ref_uniqueness(raw)?;
    check_dependency_graph(raw)?;
    Ok(())
}

/// Raise a typed error for any of the four explicitly-removed top-level fields.
fn reject_removed_fields(raw: &RawSpec) -> Result<(), ConfigError> {
    if raw.mode.is_some() {
        return Err(ConfigError::ModesRemoved);
    }
    if raw.max_iterations.is_some() {
        return Err(ConfigError::MaxIterationsHardcoded);
    }
    if raw.clean_state.is_some() {
        return Err(ConfigError::CleanStateStrict);
    }
    if raw.initiative.is_some() {
        return Err(ConfigError::InitiativeRemoved);
    }
    Ok(())
}

/// Check that every required field is present and non-empty (§3.1).
fn check_required_fields(raw: &RawSpec) -> Result<(), ConfigError> {
    if raw.title.trim().is_empty() {
        return Err(ConfigError::MissingField { field: "title" });
    }
    if raw.contract.scope.trim().is_empty() {
        return Err(ConfigError::MissingField {
            field: "contract.scope",
        });
    }
    if raw.contract.base_branch.trim().is_empty() {
        return Err(ConfigError::MissingField {
            field: "contract.base_branch",
        });
    }
    if raw.tasks.is_empty() {
        return Err(ConfigError::MissingField { field: "tasks" });
    }
    for task in &raw.tasks {
        if task.behavior.trim().is_empty() {
            return Err(ConfigError::MissingField {
                field: "task.behavior",
            });
        }
        if task.verifications.is_empty() {
            return Err(ConfigError::MissingField {
                field: "task.verifications",
            });
        }
    }
    Ok(())
}

/// Check that `pipeline`, if set, is `standard` — the only pipeline in v1.0.
///
/// `RawSpec.pipeline` parses as a free-form `Option<String>`, and `normalize`
/// applies a `standard` default. Without this check an author-supplied
/// `pipeline = "anything"` was *silently overridden* to `standard` rather than
/// rejected (A-SF-2). Mirrors the [`ConfigError::UnknownDelivery`] check —
/// `None` (omitted) is fine; any non-`standard` string is a typed rejection.
fn check_pipeline(raw: &RawSpec) -> Result<(), ConfigError> {
    match raw.pipeline.as_deref() {
        None | Some("standard") => Ok(()),
        Some(other) => Err(ConfigError::UnknownPipeline {
            got: other.to_owned(),
        }),
    }
}

/// Check the §3.1 workspace XOR — exactly one of `contract.workspace` /
/// `contract.workspace_rationale` must be present.
fn check_workspace_xor(raw: &RawSpec) -> Result<(), ConfigError> {
    let has_workspace = raw.contract.workspace.is_some();
    let has_rationale = raw.contract.workspace_rationale.is_some();
    if has_workspace == has_rationale {
        // Both set, or neither set — XOR violated either way.
        return Err(ConfigError::WorkspaceXor);
    }
    Ok(())
}

/// A verification's reporting name, or `<unnamed>` if it has none.
fn verification_label(v: &RawVerification) -> String {
    v.name.clone().unwrap_or_else(|| "<unnamed>".to_owned())
}

/// Check the intent/command mutex on every verification — contract-level and
/// task-level. Each entry must set exactly one of `intent` / `command`.
fn check_verifications(raw: &RawSpec) -> Result<(), ConfigError> {
    let all = raw
        .contract
        .verifications
        .iter()
        .chain(raw.tasks.iter().flat_map(|t| t.verifications.iter()));
    for v in all {
        let has_intent = v.intent.is_some();
        let has_command = v.command.is_some();
        if has_intent == has_command {
            // Neither set, or both set — mutex violated either way.
            return Err(ConfigError::VerificationMutex {
                name: verification_label(v),
            });
        }
    }
    Ok(())
}

/// Check that no two tasks share a `ref`.
fn check_ref_uniqueness(raw: &RawSpec) -> Result<(), ConfigError> {
    let mut seen: HashSet<&str> = HashSet::new();
    for task in &raw.tasks {
        if let Some(r) = &task.task_ref
            && !seen.insert(r.as_str())
        {
            return Err(ConfigError::DuplicateRef {
                task_ref: r.clone(),
            });
        }
    }
    Ok(())
}

/// Check the `blocked_by` graph: every dep resolves to a real ref, and the
/// graph is acyclic.
fn check_dependency_graph(raw: &RawSpec) -> Result<(), ConfigError> {
    // The set of declared refs — the only legal `blocked_by` targets.
    let declared: HashSet<&str> = raw
        .tasks
        .iter()
        .filter_map(|t| t.task_ref.as_deref())
        .collect();

    // First pass: every blocked_by entry must name a declared ref.
    for task in &raw.tasks {
        for dep in &task.blocked_by {
            if !declared.contains(dep.as_str()) {
                return Err(ConfigError::DanglingDep {
                    task_ref: task
                        .task_ref
                        .clone()
                        .unwrap_or_else(|| "<unnamed>".to_owned()),
                    missing: dep.clone(),
                });
            }
        }
    }

    // Second pass: DFS cycle detection over the ref graph. Only tasks WITH a
    // ref can be cycle nodes (an unref'd task cannot be a `blocked_by` target,
    // so it cannot close a cycle).
    let graph: HashMap<&str, &[String]> = raw
        .tasks
        .iter()
        .filter_map(|t| t.task_ref.as_deref().map(|r| (r, t.blocked_by.as_slice())))
        .collect();

    detect_cycle(&graph)
}

/// Three-colour DFS cycle detection. `White` = unvisited, `Grey` = on the
/// current DFS stack, `Black` = fully explored. Encountering a `Grey` node is
/// a back-edge → a cycle.
fn detect_cycle(graph: &HashMap<&str, &[String]>) -> Result<(), ConfigError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Colour {
        White,
        Grey,
        Black,
    }

    let mut colour: HashMap<&str, Colour> = graph.keys().map(|&k| (k, Colour::White)).collect();

    // Iterative DFS — an explicit stack avoids blowing the call stack on a
    // pathologically deep dependency chain.
    for &start in graph.keys() {
        if colour[start] != Colour::White {
            continue;
        }
        // Stack frames: (node, whether we are entering or leaving it).
        let mut stack: Vec<(&str, bool)> = vec![(start, false)];
        while let Some((node, leaving)) = stack.pop() {
            if leaving {
                colour.insert(node, Colour::Black);
                continue;
            }
            if colour[node] == Colour::Grey {
                // Already on the stack via another path — skip re-entry.
                continue;
            }
            colour.insert(node, Colour::Grey);
            // Schedule the "leave" frame, then push children.
            stack.push((node, true));
            for dep in graph.get(node).copied().unwrap_or(&[]) {
                match colour.get(dep.as_str()).copied() {
                    Some(Colour::Grey) => {
                        return Err(ConfigError::DependencyCycle {
                            task_ref: dep.clone(),
                        });
                    }
                    Some(Colour::White) => stack.push((dep.as_str(), false)),
                    // Black = fully explored, no cycle through it; None =
                    // dangling (already rejected by the first pass).
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loads a spec fixture by stem from `tests/fixtures/specs/`.
    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/specs/{name}.toml",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read fixture {path}: {e}"))
    }

    /// Parse a fixture to a RawSpec (panics on parse failure — fixtures here
    /// are all parseable; validation is the thing under test).
    fn raw(name: &str) -> RawSpec {
        RawSpec::from_toml(&fixture(name)).unwrap()
    }

    // --- Happy paths: the four valid fixtures all validate clean. ---

    #[test]
    fn minimum_spec_validates() {
        assert!(validate(&raw("01_minimum")).is_ok());
    }

    #[test]
    fn multi_task_dag_validates() {
        assert!(validate(&raw("02_multi_task_dag")).is_ok());
    }

    #[test]
    fn authored_decisions_spec_validates() {
        assert!(validate(&raw("03_with_authored_decisions")).is_ok());
    }

    #[test]
    fn skills_spec_validates() {
        assert!(validate(&raw("04_with_skills")).is_ok());
    }

    // --- Removed-field rejections (one fixture each). ---

    #[test]
    fn modes_field_is_rejected() {
        assert!(matches!(
            validate(&raw("05_rejects_modes")).unwrap_err(),
            ConfigError::ModesRemoved
        ));
    }

    #[test]
    fn max_iterations_field_is_rejected() {
        assert!(matches!(
            validate(&raw("06_rejects_max_iterations")).unwrap_err(),
            ConfigError::MaxIterationsHardcoded
        ));
    }

    #[test]
    fn clean_state_field_is_rejected() {
        assert!(matches!(
            validate(&raw("07_rejects_clean_state")).unwrap_err(),
            ConfigError::CleanStateStrict
        ));
    }

    #[test]
    fn initiative_field_is_rejected() {
        assert!(matches!(
            validate(&raw("09_rejects_initiative")).unwrap_err(),
            ConfigError::InitiativeRemoved
        ));
    }

    // --- Structural rejections. ---

    #[test]
    fn dag_cycle_is_rejected() {
        assert!(matches!(
            validate(&raw("08_invalid_cycle")).unwrap_err(),
            ConfigError::DependencyCycle { .. }
        ));
    }

    #[test]
    fn workspace_xor_both_set_is_rejected() {
        assert!(matches!(
            validate(&raw("10_rejects_workspace_xor")).unwrap_err(),
            ConfigError::WorkspaceXor
        ));
    }

    #[test]
    fn dangling_blocked_by_is_rejected() {
        let err = validate(&raw("11_rejects_dangling_dep")).unwrap_err();
        match err {
            ConfigError::DanglingDep { missing, .. } => {
                assert_eq!(missing, "does-not-exist");
            }
            other => panic!("expected DanglingDep, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_ref_is_rejected() {
        let err = validate(&raw("12_rejects_dup_ref")).unwrap_err();
        match err {
            ConfigError::DuplicateRef { task_ref } => assert_eq!(task_ref, "middleware"),
            other => panic!("expected DuplicateRef, got {other:?}"),
        }
    }

    #[test]
    fn verification_with_both_intent_and_command_is_rejected() {
        assert!(matches!(
            validate(&raw("13_rejects_verification_mutex")).unwrap_err(),
            ConfigError::VerificationMutex { .. }
        ));
    }

    #[test]
    fn unknown_pipeline_is_rejected() {
        // A-SF-2 regression: `pipeline = "turbo"` parses cleanly but is NOT
        // `standard` — it must be a typed rejection, not a silent override.
        let err = validate(&raw("15_rejects_unknown_pipeline")).unwrap_err();
        match err {
            ConfigError::UnknownPipeline { got } => assert_eq!(got, "turbo"),
            other => panic!("expected UnknownPipeline, got {other:?}"),
        }
    }

    #[test]
    fn standard_pipeline_and_omitted_pipeline_both_validate() {
        // The two legal forms: an explicit `standard` and an omitted pipeline.
        // `01_minimum` omits `pipeline` entirely; a synthetic spec sets it.
        assert!(validate(&raw("01_minimum")).is_ok());
        let toml = valid_toml().replace(
            r#"title = "valid""#,
            "title = \"valid\"\npipeline = \"standard\"",
        );
        assert!(validate(&RawSpec::from_toml(&toml).unwrap()).is_ok());
    }

    // --- deny_unknown_fields catches a genuinely-unknown field at parse
    //     time — before validate() ever runs. ---

    #[test]
    fn genuinely_unknown_field_is_rejected_at_parse_time() {
        // `flavor` is not a named ex-field — the parse stage rejects it with a
        // generic ConfigError::Toml, distinct from the four typed rejections.
        let err = RawSpec::from_toml(&fixture("14_rejects_unknown_field")).unwrap_err();
        assert!(matches!(err, ConfigError::Toml(_)));
    }

    // --- Sad paths the named fixtures do not cover — synthetic specs. ---

    /// A baseline valid spec, built in code so individual fields can be broken.
    fn valid_toml() -> String {
        r#"
title = "valid"

[contract]
scope = "do the thing"
base_branch = "main"
workspace = "/repo"

[[tasks]]
behavior = "implement it"
verifications = [{ intent = "it works" }]
"#
        .to_owned()
    }

    #[test]
    fn baseline_synthetic_spec_validates() {
        assert!(validate(&RawSpec::from_toml(&valid_toml()).unwrap()).is_ok());
    }

    #[test]
    fn empty_title_is_rejected() {
        let toml = valid_toml().replace(r#"title = "valid""#, r#"title = """#);
        match validate(&RawSpec::from_toml(&toml).unwrap()).unwrap_err() {
            ConfigError::MissingField { field } => assert_eq!(field, "title"),
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn empty_scope_is_rejected() {
        let toml = valid_toml().replace(r#"scope = "do the thing""#, r#"scope = """#);
        match validate(&RawSpec::from_toml(&toml).unwrap()).unwrap_err() {
            ConfigError::MissingField { field } => assert_eq!(field, "contract.scope"),
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn missing_workspace_and_rationale_is_rejected() {
        // Neither workspace nor workspace_rationale — the XOR's other failure
        // mode (the dup-fixture covers "both set").
        let toml = valid_toml().replace("workspace = \"/repo\"\n", "");
        assert!(matches!(
            validate(&RawSpec::from_toml(&toml).unwrap()).unwrap_err(),
            ConfigError::WorkspaceXor
        ));
    }

    #[test]
    fn task_with_no_verifications_is_rejected() {
        let toml = valid_toml().replace(
            "verifications = [{ intent = \"it works\" }]",
            "verifications = []",
        );
        match validate(&RawSpec::from_toml(&toml).unwrap()).unwrap_err() {
            ConfigError::MissingField { field } => assert_eq!(field, "task.verifications"),
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn verification_with_neither_intent_nor_command_is_rejected() {
        // The mutex's other failure mode — an empty `{}` verification entry.
        let toml = valid_toml().replace(
            "verifications = [{ intent = \"it works\" }]",
            "verifications = [{ name = \"empty\" }]",
        );
        match validate(&RawSpec::from_toml(&toml).unwrap()).unwrap_err() {
            ConfigError::VerificationMutex { name } => assert_eq!(name, "empty"),
            other => panic!("expected VerificationMutex, got {other:?}"),
        }
    }

    #[test]
    fn self_blocking_task_is_a_cycle() {
        // A task that lists its own ref in blocked_by is a trivial 1-cycle.
        let toml = r#"
title = "valid"

[contract]
scope = "do the thing"
base_branch = "main"
workspace = "/repo"

[[tasks]]
ref = "loop"
behavior = "implement it"
blocked_by = ["loop"]
verifications = [{ intent = "it works" }]
"#;
        assert!(matches!(
            validate(&RawSpec::from_toml(toml).unwrap()).unwrap_err(),
            ConfigError::DependencyCycle { .. }
        ));
    }

    #[test]
    fn deep_acyclic_chain_validates() {
        // a <- b <- c <- d — a long but acyclic chain must pass (exercises the
        // iterative DFS without a false cycle report).
        let toml = r#"
title = "valid"

[contract]
scope = "do the thing"
base_branch = "main"
workspace = "/repo"

[[tasks]]
ref = "a"
behavior = "step a"
verifications = [{ intent = "a ok" }]

[[tasks]]
ref = "b"
behavior = "step b"
blocked_by = ["a"]
verifications = [{ intent = "b ok" }]

[[tasks]]
ref = "c"
behavior = "step c"
blocked_by = ["b"]
verifications = [{ intent = "c ok" }]

[[tasks]]
ref = "d"
behavior = "step d"
blocked_by = ["c"]
verifications = [{ intent = "d ok" }]
"#;
        assert!(validate(&RawSpec::from_toml(toml).unwrap()).is_ok());
    }

    #[test]
    fn diamond_dependency_is_not_a_cycle() {
        // a <- {b, c} <- d — a diamond is a DAG, not a cycle. A naive
        // visited-set check would false-flag the second path into `a`.
        let toml = r#"
title = "valid"

[contract]
scope = "do the thing"
base_branch = "main"
workspace = "/repo"

[[tasks]]
ref = "a"
behavior = "root"
verifications = [{ intent = "a ok" }]

[[tasks]]
ref = "b"
behavior = "left"
blocked_by = ["a"]
verifications = [{ intent = "b ok" }]

[[tasks]]
ref = "c"
behavior = "right"
blocked_by = ["a"]
verifications = [{ intent = "c ok" }]

[[tasks]]
ref = "d"
behavior = "join"
blocked_by = ["b", "c"]
verifications = [{ intent = "d ok" }]
"#;
        assert!(validate(&RawSpec::from_toml(toml).unwrap()).is_ok());
    }
}
