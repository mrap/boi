//! §7.2 composition queries — the data the renderer turns into `PhaseContext`.
//!
//! [`compose_for_phase`] runs the two §7.2 queries:
//!
//! 1. **all decisions for the spec** — every [`DecisionRecord`] WHERE
//!    `spec_id`, sorted by `created_at`.
//! 2. **prior phase runs** — every prior [`PhaseRunSummary`] for the task (or
//!    spec-level when `task_id` is `None`), each with the IDs of the decisions
//!    it recorded.
//!
//! ## Why the decisions query has NO join
//!
//! Authored decisions have `phase_run_id IS NULL`. An `INNER JOIN` against
//! `phase_runs` would silently drop every authored decision — exactly the v1
//! silent-failure class v2 exists to kill (Batch A review L2). Q8 wants "all
//! decisions WHERE spec_id" with no phase-run filter, so the simplest correct
//! form is a plain `SELECT … WHERE spec_id` with no join at all. This module
//! reuses [`crate::repo::decisions::fetch_by_spec`], which is exactly that.

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use crate::repo::db::RepoError;
use crate::repo::decisions::fetch_by_spec;
use crate::types::context::PhaseRunSummary;
use crate::types::decision::DecisionRecord;
use crate::types::ids::{DecisionId, PhaseRunId, SpecId, TaskId};
use crate::types::reasons::ErrorWhyFix;
use crate::types::verdict::{VerdictOutcome, WorkerVerdict};

/// The composed inputs for one phase's `PhaseContext` (§7.2).
#[derive(Debug, Clone)]
pub struct ComposedContext {
    /// ALL decisions for the spec, sorted by `created_at` — authored ones
    /// included (Q8).
    pub decisions: Vec<DecisionRecord>,
    /// Prior phase runs for the task (or spec-level when `task_id` is `None`),
    /// oldest first.
    pub prior_phase_runs: Vec<PhaseRunSummary>,
}

/// Compose the §7.2 context for a phase about to run.
///
/// `current_phase` is accepted for parity with §7.2's signature; v1.0 surfaces
/// the full prior-run history regardless of the upcoming phase, so it does not
/// yet filter on it.
pub async fn compose_for_phase(
    pool: &SqlitePool,
    spec_id: &SpecId,
    task_id: Option<&TaskId>,
    _current_phase: &str,
) -> Result<ComposedContext, RepoError> {
    // `_current_phase` — see fn rustdoc; reserved for a later phase filter.
    let decisions = fetch_by_spec(pool, spec_id).await?;
    let prior_phase_runs = prior_phase_runs(pool, spec_id, task_id).await?;
    Ok(ComposedContext {
        decisions,
        prior_phase_runs,
    })
}

/// Raw row of the §7.2 "prior phase runs" query — `phase_runs` columns plus a
/// `json_group_array` of the decision IDs each run recorded.
#[derive(sqlx::FromRow)]
struct PriorRunRow {
    id: String,
    phase: String,
    phase_iteration: i64,
    provider: String,
    synopsis: String,
    verdict: Option<sqlx::types::Json<WorkerVerdict>>,
    files_touched: sqlx::types::Json<Vec<std::path::PathBuf>>,
    completed_at: Option<DateTime<Utc>>,
    /// `json_group_array(d.id)` — a JSON array string; `[null]` when the
    /// LEFT JOIN matched no decisions.
    decisions_made: sqlx::types::Json<Vec<Option<String>>>,
}

/// Run the §7.2 "prior phase runs" query and map rows to [`PhaseRunSummary`].
///
/// `task_id = Some` → runs for that task; `task_id = None` → spec-level runs
/// (`phase_runs.task_id IS NULL`). The query is a `LEFT JOIN` to `decisions`
/// so a run that recorded nothing is still returned.
async fn prior_phase_runs(
    pool: &SqlitePool,
    spec_id: &SpecId,
    task_id: Option<&TaskId>,
) -> Result<Vec<PhaseRunSummary>, RepoError> {
    let sid = spec_id.as_str();
    // SQLite cannot parametrize `task_id = ?` vs `task_id IS NULL`, so the two
    // cases use distinct WHERE clauses; both are LEFT JOINs (§7.2).
    let rows: Vec<PriorRunRow> = match task_id {
        Some(tid) => {
            sqlx::query_as::<_, PriorRunRow>(
                "SELECT pr.id, pr.phase, pr.phase_iteration, pr.provider, pr.synopsis, \
                        pr.verdict, pr.files_touched, pr.completed_at, \
                        json_group_array(d.id) AS decisions_made \
                 FROM phase_runs pr \
                 LEFT JOIN decisions d ON d.phase_run_id = pr.id \
                 WHERE pr.spec_id = ?1 AND pr.task_id = ?2 \
                 GROUP BY pr.id ORDER BY pr.started_at, pr.phase_iteration",
            )
            .bind(sid)
            .bind(tid.as_str())
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as::<_, PriorRunRow>(
                "SELECT pr.id, pr.phase, pr.phase_iteration, pr.provider, pr.synopsis, \
                        pr.verdict, pr.files_touched, pr.completed_at, \
                        json_group_array(d.id) AS decisions_made \
                 FROM phase_runs pr \
                 LEFT JOIN decisions d ON d.phase_run_id = pr.id \
                 WHERE pr.spec_id = ?1 AND pr.task_id IS NULL \
                 GROUP BY pr.id ORDER BY pr.started_at, pr.phase_iteration",
            )
            .bind(sid)
            .fetch_all(pool)
            .await?
        }
    };
    rows.into_iter().map(row_to_summary).collect()
}

/// Convert a raw §7.2 row into a typed [`PhaseRunSummary`].
fn row_to_summary(row: PriorRunRow) -> Result<PhaseRunSummary, RepoError> {
    let bad = |what: &str| RepoError::NotFound(format!("corrupt phase_runs row: {what}"));

    let id = PhaseRunId::new(&row.id).map_err(|_| bad("id"))?;
    let phase_iteration = u32::try_from(row.phase_iteration).map_err(|_| bad("phase_iteration"))?;

    // Decode the verdict once: it yields both the outcome string and (for a
    // Fail/Blocked verdict) the error_why_fix to forward (Q3 patch).
    let (verdict_outcome, error_why_fix) = match row.verdict {
        Some(sqlx::types::Json(v)) => {
            let outcome = verdict_outcome_str(&v);
            (Some(outcome.to_owned()), error_why_fix_of(&v))
        }
        None => (None, None),
    };

    // `json_group_array` over the LEFT JOIN yields `[null]` when no decision
    // matched — filter the NULL out, then validate the rest.
    let decisions_made: Vec<DecisionId> = row
        .decisions_made
        .0
        .into_iter()
        .flatten()
        .map(|s| DecisionId::new(&s).map_err(|_| bad("decisions_made id")))
        .collect::<Result<_, _>>()?;

    Ok(PhaseRunSummary {
        id,
        phase: row.phase,
        phase_iteration,
        provider: row.provider,
        synopsis: row.synopsis,
        verdict_outcome,
        files_touched: row.files_touched.0,
        decisions_made,
        completed_at: row.completed_at,
        error_why_fix,
    })
}

/// The `verdict_outcome` string for a verdict (`passing`/`redo`/`blocked`/`fail`).
fn verdict_outcome_str(v: &WorkerVerdict) -> &'static str {
    match v.outcome {
        VerdictOutcome::Passing { .. } => "passing",
        VerdictOutcome::Redo { .. } => "redo",
        VerdictOutcome::Blocked { .. } => "blocked",
        VerdictOutcome::Fail { .. } => "fail",
        VerdictOutcome::Canceled => "canceled",
    }
}

/// The [`ErrorWhyFix`] to forward from a verdict — populated only for a
/// `Fail` or `Blocked` outcome (Q3 patch). A `Fail` always carries the triple;
/// a `Blocked` carries an optional one.
fn error_why_fix_of(v: &WorkerVerdict) -> Option<ErrorWhyFix> {
    match &v.outcome {
        VerdictOutcome::Fail { error, why, fix } => Some(ErrorWhyFix {
            error: error.clone(),
            why: why.clone(),
            fix: fix.clone(),
        }),
        VerdictOutcome::Blocked { error_why_fix, .. } => error_why_fix.clone(),
        VerdictOutcome::Passing { .. } | VerdictOutcome::Redo { .. } | VerdictOutcome::Canceled => {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::decisions::insert as insert_decision;
    use crate::repo::phase_runs::{insert_start, update_end};
    use crate::repo::spec_versions::{VersionTrigger, append_version};
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::insert_task;
    use crate::types::verdict::Evidence;
    use serde_json::json;

    /// Pool with a spec (v1) and one task.
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

    /// The regression guard: a spec with one AUTHORED decision (phase_run_id
    /// NULL) plus two runtime decisions — `compose_for_phase` returns all 3.
    /// An INNER JOIN to phase_runs would drop the authored one.
    #[tokio::test]
    async fn composition_returns_authored_decision() {
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

        // 1 authored (NULL phase_run) + 2 runtime (phase_run = pr).
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
                Utc::now() - chrono::Duration::minutes(3),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        for did in ["D0000002b", "D0000003c"] {
            insert_decision(
                &pool,
                &DecisionRecord::new_runtime(
                    DecisionId::new(did).unwrap(),
                    spec.clone(),
                    Some(pr.clone()),
                    did.into(),
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
        }

        let ctx = compose_for_phase(&pool, &spec, Some(&task), "review")
            .await
            .unwrap();
        assert_eq!(
            ctx.decisions.len(),
            3,
            "all 3 decisions returned — the authored one is the INNER-JOIN regression guard",
        );
        // The authored decision (earliest created_at) sorts first.
        assert_eq!(ctx.decisions[0].title, "authored");
    }

    /// Prior phase runs come back oldest-first (§7.2 `ORDER BY started_at`),
    /// and each run lists the decision IDs it recorded.
    #[tokio::test]
    async fn prior_runs_sorted_and_carry_decisions() {
        let (pool, spec, task) = seeded().await;
        let t0 = Utc::now() - chrono::Duration::minutes(10);
        let t1 = Utc::now() - chrono::Duration::minutes(5);

        let pr_old = PhaseRunId::new("P00000a1a").unwrap();
        let pr_new = PhaseRunId::new("P00000b2b").unwrap();
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
            t0,
        )
        .await
        .unwrap();
        insert_start(
            &pool,
            &pr_new,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            t1,
        )
        .await
        .unwrap();
        update_end(&pool, &pr_old, "planned", &passing(), &[], 0, 0, t1)
            .await
            .unwrap();
        // A decision recorded by the older run.
        insert_decision(
            &pool,
            &DecisionRecord::new_runtime(
                DecisionId::new("D0000001a").unwrap(),
                spec.clone(),
                Some(pr_old.clone()),
                "from plan".into(),
                "s".into(),
                "r".into(),
                vec![],
                None,
                t1,
            )
            .unwrap(),
        )
        .await
        .unwrap();

        let ctx = compose_for_phase(&pool, &spec, Some(&task), "review")
            .await
            .unwrap();
        assert_eq!(ctx.prior_phase_runs.len(), 2);
        // Oldest first.
        assert_eq!(ctx.prior_phase_runs[0].id, pr_old);
        assert_eq!(ctx.prior_phase_runs[1].id, pr_new);
        // The older run carries the decision it recorded; the newer none.
        assert_eq!(
            ctx.prior_phase_runs[0].decisions_made,
            vec![DecisionId::new("D0000001a").unwrap()],
        );
        assert!(ctx.prior_phase_runs[1].decisions_made.is_empty());
    }

    /// `error_why_fix` propagates from a `Fail` verdict's error/why/fix triple.
    #[tokio::test]
    async fn error_why_fix_propagates_from_fail_verdict() {
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

        let ctx = compose_for_phase(&pool, &spec, Some(&task), "review")
            .await
            .unwrap();
        let summary = &ctx.prior_phase_runs[0];
        assert_eq!(summary.verdict_outcome.as_deref(), Some("fail"));
        let ewf = summary.error_why_fix.as_ref().expect("fail carries ewf");
        assert_eq!(ewf.error, "E0432");
        assert_eq!(ewf.why, "missing import");
        assert_eq!(ewf.fix, "add `use std::fmt;`");
    }

    /// A spec-level phase (`task_id = None`) sees only `task_id IS NULL` runs,
    /// not the task-scoped ones.
    #[tokio::test]
    async fn spec_level_composition_excludes_task_runs() {
        let (pool, spec, task) = seeded().await;
        // A task-scoped run.
        let pr_task = PhaseRunId::new("P00000t1a").unwrap();
        insert_start(
            &pool,
            &pr_task,
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
        // A spec-level run (task_id NULL).
        let pr_spec = PhaseRunId::new("P00000s2b").unwrap();
        insert_start(
            &pool,
            &pr_spec,
            &spec,
            None,
            "plan",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();

        let ctx = compose_for_phase(&pool, &spec, None, "review")
            .await
            .unwrap();
        assert_eq!(ctx.prior_phase_runs.len(), 1, "only the spec-level run");
        assert_eq!(ctx.prior_phase_runs[0].id, pr_spec);
    }

    fn passing() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "ok".into(),
            outcome: VerdictOutcome::Passing {
                evidence: Evidence::default(),
            },
        }
    }
}
