//! `PhaseContext` composition — the harness-side half of the decision push
//! model (§7.0).
//!
//! Workers never query persistent state. At every phase clock-in the harness
//! pushes ALL decisions for the spec into [`PhaseContext`] (Q8 — no filter, no
//! cap, no tiered rules; §7.0/§7.3). [`compose`] is the assembler: it runs the
//! Phase 3 composition query ([`compose_for_phase`]) and pairs its output with
//! the caller-supplied authored contracts.
//!
//! [`compose_for_phase`]: crate::repo::composition::compose_for_phase
//!
//! ## `compose` takes contracts, not a `&config::Spec` (review S4)
//!
//! Design §3.0: the spec TOML is unreferenced after dispatch — the orchestrator
//! does not hold the parsed `config::Spec`. `run_phase` (Phase 5a) re-hydrates
//! [`SpecContract`] / [`TaskContract`] from the `spec_versions` snapshot and
//! passes them in. `compose` is therefore a thin assembler over the repo query;
//! it never parses TOML and never touches `crate::config`.
//!
//! ## Skills, no curation
//!
//! `PhaseContext` carries the spec's `[[skill]]` blocks as `Vec<SkillRef>`
//! (G23.1 → G26.3) — the worker-branch [`GooseRuntime`] threads them into the
//! recipe `extensions:` field (§7.4). Skills are re-hydrated from the spec
//! snapshot at clock-in alongside the contracts. There is no curation step:
//! missing context is a harness bug to be fixed in the harness, never a
//! worker problem (§7.0).
//!
//! [`GooseRuntime`]: crate::runtime::goose::GooseRuntime

use sqlx::SqlitePool;

use crate::config::SkillRef;
use crate::repo::composition::compose_for_phase;
use crate::repo::db::RepoError;
use crate::types::context::{PhaseContext, SpecContract, TaskBrief, TaskContract};
use crate::types::ids::{PhaseRunId, SpecId, TaskId};

/// [`compose`] failed to assemble a [`PhaseContext`].
///
/// The only failure mode is the underlying repo query — `compose` itself does
/// no fallible work beyond it. Wrapping (rather than re-exporting `RepoError`)
/// keeps the `service`-layer surface from leaking a `repo`-layer error type and
/// gives a later phase room to add composition-specific variants.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    /// The Phase 3 composition query failed.
    #[error("composition query failed: {0}")]
    Repo(#[from] RepoError),
}

/// Assemble the full [`PhaseContext`] for a phase clock-in (§7.1).
///
/// Pulls `decisions` + `prior_phase_runs` via the Phase 3 composition query
/// ([`compose_for_phase`]) and pairs them with the caller-supplied contracts.
/// The query already returns ALL decisions for the spec (Q8 — no filter, no
/// cap), sorted by `created_at`, plus `prior_phase_runs` carrying the
/// `error_why_fix` of any Fail/Blocked run (Q3). `compose` maps that onto the
/// Phase 1 [`PhaseContext`] struct and attaches the contracts.
///
/// [`compose_for_phase`]: crate::repo::composition::compose_for_phase
///
/// `spec_contract` / `task_contract` are passed by value — re-hydrated by
/// `run_phase` (Phase 5a) from the `spec_versions` snapshot, never parsed from
/// a `config::Spec` (review S4). `task_contract` is `None` for spec-level
/// phases; `task_id` is then `None` too.
#[allow(clippy::too_many_arguments)]
pub async fn compose(
    pool: &SqlitePool,
    spec_contract: SpecContract,
    task_contract: Option<TaskContract>,
    tasks: Vec<TaskBrief>,
    skills: Vec<SkillRef>,
    spec_id: &SpecId,
    task_id: Option<&TaskId>,
    phase: &str,
    phase_run_id: &PhaseRunId,
    iteration: u32,
) -> Result<PhaseContext, ContextError> {
    let composed = compose_for_phase(pool, spec_id, task_id, phase).await?;
    Ok(PhaseContext {
        spec_id: spec_id.clone(),
        task_id: task_id.cloned(),
        phase: phase.to_owned(),
        phase_run_id: phase_run_id.clone(),
        iteration,
        spec_contract,
        task_contract,
        tasks,
        skills,
        decisions: composed.decisions,
        prior_phase_runs: composed.prior_phase_runs,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use serde_json::json;

    use super::*;
    use crate::repo::db::connect;
    use crate::repo::decisions::insert as insert_decision;
    use crate::repo::phase_runs::{insert_start, update_end};
    use crate::repo::spec_versions::{VersionTrigger, append_version};
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::insert_task;
    use crate::types::context::{TaskContract, Verification};
    use crate::types::decision::DecisionRecord;
    use crate::types::ids::DecisionId;
    use crate::types::verdict::{VerdictOutcome, WorkerVerdict};

    /// A pool seeded with a spec (v1) and one task.
    async fn seeded() -> (SqlitePool, SpecId, TaskId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        let task = TaskId::new("T0000001a").unwrap();
        insert_spec(&pool, &spec, Utc::now()).await.unwrap();
        append_version(
            &pool,
            &spec,
            1,
            &json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        insert_task(&pool, &task, &spec, None).await.unwrap();
        (pool, spec, task)
    }

    fn spec_contract() -> SpecContract {
        SpecContract {
            scope: "rate limiting".into(),
            workspace: PathBuf::from("/repo"),
            base_branch: "main".into(),
            exclusions: vec![],
            verifications: vec![],
            must_emit: vec![],
        }
    }

    fn task_contract() -> TaskContract {
        TaskContract {
            behavior: "add token bucket".into(),
            verifications: vec![Verification::Command {
                name: None,
                command: "cargo test".into(),
            }],
        }
    }

    /// A seeded spec with 1 authored + 1 runtime decision and 1 prior run →
    /// `compose` returns a `PhaseContext` carrying both decisions and the run.
    #[tokio::test]
    async fn test_l2_compose_assembles_decisions_and_prior_runs() {
        let (pool, spec, task) = seeded().await;
        let pr_old = PhaseRunId::new("P00000a1a").unwrap();
        insert_start(
            &pool,
            &pr_old,
            &spec,
            Some(&task),
            "plan",
            0,
            1,
            "claude_code",
            None,
            Utc::now() - chrono::Duration::minutes(10),
        )
        .await
        .unwrap();
        let passing = WorkerVerdict {
            synopsis: "planned".into(),
            outcome: VerdictOutcome::Passing {
                evidence: Default::default(),
            },
        };
        update_end(&pool, &pr_old, "planned", &passing, &[], 0, 0, Utc::now())
            .await
            .unwrap();

        // 1 authored (NULL phase_run) + 1 runtime (parent = pr_old).
        insert_decision(
            &pool,
            &DecisionRecord::new_authored(
                DecisionId::new("D0000001a").unwrap(),
                spec.clone(),
                None,
                "authored".into(),
                "s".into(),
                "r".into(),
                vec![],
                None,
                Utc::now() - chrono::Duration::minutes(5),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        insert_decision(
            &pool,
            &DecisionRecord::new_runtime(
                DecisionId::new("D0000002b").unwrap(),
                spec.clone(),
                Some(pr_old.clone()),
                "runtime".into(),
                "s".into(),
                "r".into(),
                vec![],
                None,
                Utc::now(),
            )
            .unwrap(),
        )
        .await
        .unwrap();

        let next_run = PhaseRunId::new("P00000b2b").unwrap();
        let ctx = compose(
            &pool,
            spec_contract(),
            Some(task_contract()),
            vec![],
            vec![],
            &spec,
            Some(&task),
            "execute",
            &next_run,
            1,
        )
        .await
        .unwrap();

        assert_eq!(ctx.decisions.len(), 2, "both decisions pushed (Q8)");
        assert_eq!(ctx.prior_phase_runs.len(), 1, "the one prior run");
        assert_eq!(ctx.prior_phase_runs[0].id, pr_old);
        assert_eq!(ctx.spec_id, spec);
        assert_eq!(ctx.task_id, Some(task));
        assert_eq!(ctx.phase, "execute");
        assert_eq!(ctx.phase_run_id, next_run);
        assert_eq!(ctx.iteration, 1);
    }

    /// A spec-level phase: `task_id = None` ⇒ `task_contract` is `None` and the
    /// composed `PhaseContext` carries no task identity.
    #[tokio::test]
    async fn test_l2_compose_spec_level_phase_has_no_task_contract() {
        let (pool, spec, _task) = seeded().await;
        let pr = PhaseRunId::new("P0000001a").unwrap();
        let ctx = compose(
            &pool,
            spec_contract(),
            None,
            vec![],
            vec![],
            &spec,
            None,
            "plan",
            &pr,
            1,
        )
        .await
        .unwrap();
        assert!(ctx.task_id.is_none());
        assert!(ctx.task_contract.is_none());
    }

    /// `error_why_fix` from a prior `Fail` run reaches `prior_phase_runs` —
    /// the Q3 forwarding path is intact through `compose`.
    #[tokio::test]
    async fn test_l2_compose_forwards_error_why_fix_from_fail_run() {
        let (pool, spec, task) = seeded().await;
        let pr = PhaseRunId::new("P0000001a").unwrap();
        insert_start(
            &pool,
            &pr,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        let fail = WorkerVerdict {
            synopsis: "build broke".into(),
            outcome: VerdictOutcome::Fail {
                error: "E0432".into(),
                why: "missing import".into(),
                fix: "add `use std::fmt;`".into(),
            },
        };
        update_end(&pool, &pr, "build broke", &fail, &[], 0, 0, Utc::now())
            .await
            .unwrap();

        let next_run = PhaseRunId::new("P00000b2b").unwrap();
        let ctx = compose(
            &pool,
            spec_contract(),
            Some(task_contract()),
            vec![],
            vec![],
            &spec,
            Some(&task),
            "execute",
            &next_run,
            2,
        )
        .await
        .unwrap();

        let summary = &ctx.prior_phase_runs[0];
        assert_eq!(summary.verdict_outcome.as_deref(), Some("fail"));
        let ewf = summary
            .error_why_fix
            .as_ref()
            .expect("a Fail run forwards error_why_fix");
        assert_eq!(ewf.error, "E0432");
        assert_eq!(ewf.why, "missing import");
        assert_eq!(ewf.fix, "add `use std::fmt;`");
    }
}
