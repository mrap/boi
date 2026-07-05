//! The `decisions` table — the append-only "why we did it this way" log.
//!
//! Workers EMIT decisions via the `decision_record(...)` MCP tool; they never
//! read this table. The harness pushes ALL decisions for a spec into
//! `PhaseContext` at clock-in (Q8) — [`fetch_by_spec`] is the curator's
//! primary query.
//!
//! Append-only — a new decision can `supersede` a prior one, but rows never
//! UPDATE or DELETE (design §11).
//!
//! ## The origin / phase_run_id mutex — defence in depth
//!
//! `DecisionRecord`'s constructors ([`crate::types::DecisionRecord`]) already
//! enforce that an `authored` decision has no `phase_run_id` and a
//! `runtime`/`human` one does. [`insert`] does not re-check in Rust — instead
//! the `decisions` CHECK constraint backstops it at the DB level, catching a
//! programming error that bypassed the constructors (a raw struct literal).
//! A violation surfaces loudly as [`RepoError::Sqlx`].

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::repo::db::RepoError;
use crate::types::decision::{DecisionOrigin, DecisionRecord, RejectedAlternative};
use crate::types::ids::{DecisionId, PhaseRunId, SpecId};

/// Insert a decision record (append-only).
///
/// The origin/phase_run_id mutex is enforced by the DB CHECK — a row that
/// violates it (e.g. an `authored` decision carrying a `phase_run_id`) is
/// rejected loudly. A duplicate `id`, or a second decision superseding an
/// already-superseded one (the `supersedes` partial UNIQUE), returns
/// [`RepoError::Duplicate`].
pub async fn insert(pool: &SqlitePool, decision: &DecisionRecord) -> Result<(), RepoError> {
    let id = decision.id.as_str();
    let spec_id = decision.spec_id.as_str();
    let phase_run_id = decision.phase_run_id.as_ref().map(PhaseRunId::as_str);
    let origin = origin_str(decision.origin);
    let alternatives = serde_json::to_value(&decision.alternatives)?;
    let supersedes = decision.supersedes.as_ref().map(DecisionId::as_str);
    let created_at = decision.created_at;

    let res = sqlx::query!(
        "INSERT INTO decisions \
         (id, spec_id, phase_run_id, origin, title, summary, rationale, alternatives, \
          supersedes, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        id,
        spec_id,
        phase_run_id,
        origin,
        decision.title,
        decision.summary,
        decision.rationale,
        alternatives,
        supersedes,
        created_at,
    )
    .execute(pool)
    .await;
    match res {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(RepoError::Duplicate(
            format!("decision {} (id or supersedes already used)", decision.id),
        )),
        Err(e) => Err(RepoError::Sqlx(e)),
    }
}

/// All decisions for a spec, sorted by `created_at` — the curator query (Q8).
///
/// Returns every decision regardless of origin: authored decisions (with a
/// NULL `phase_run_id`) are included, because the filter is on `spec_id`
/// alone.
pub async fn fetch_by_spec(
    pool: &SqlitePool,
    spec_id: &SpecId,
) -> Result<Vec<DecisionRecord>, RepoError> {
    let sid = spec_id.as_str();
    let rows = sqlx::query_as::<_, DecisionRow>(
        "SELECT id, spec_id, phase_run_id, origin, title, summary, rationale, alternatives, \
                supersedes, created_at \
         FROM decisions WHERE spec_id = ?1 ORDER BY created_at",
    )
    .bind(sid)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(DecisionRow::into_record).collect()
}

/// Fetch a single decision by ID.
///
/// Returns [`RepoError::NotFound`] if no such decision exists.
pub async fn fetch_by_id(
    pool: &SqlitePool,
    decision_id: &DecisionId,
) -> Result<DecisionRecord, RepoError> {
    let did = decision_id.as_str();
    let row = sqlx::query_as::<_, DecisionRow>(
        "SELECT id, spec_id, phase_run_id, origin, title, summary, rationale, alternatives, \
                supersedes, created_at \
         FROM decisions WHERE id = ?1",
    )
    .bind(did)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => r.into_record(),
        None => Err(RepoError::NotFound(format!("decision {decision_id}"))),
    }
}

/// Allocate a fresh, unused [`DecisionId`].
///
/// Unlike the other `allocate_*` functions this is a pure ID generator — it
/// does NOT perform the INSERT. A [`DecisionRecord`] takes its `id` at
/// construction (via `new_authored` / `new_runtime` / `new_human`), so the
/// allocator cannot build the row; the caller allocates the ID, builds the
/// record, then calls [`insert`]. Generation retries a fresh `random_id('D')`
/// against an existence check, capped at 5 attempts, then
/// [`RepoError::IdExhausted`] — the same loud-failure contract as the other
/// allocators. `insert`'s `id` PRIMARY KEY is the durable backstop against the
/// (astronomically unlikely) TOCTOU race.
pub async fn allocate_decision_id(pool: &SqlitePool) -> Result<DecisionId, RepoError> {
    for _ in 0..crate::repo::ids::MAX_ID_ATTEMPTS {
        let candidate = crate::repo::ids::random_id('D');
        let taken = sqlx::query_scalar!("SELECT id FROM decisions WHERE id = ?1", candidate)
            .fetch_optional(pool)
            .await?
            .is_some();
        if !taken {
            return DecisionId::new(&candidate)
                .map_err(|e| RepoError::Duplicate(format!("generated invalid decision id: {e}")));
        }
    }
    Err(RepoError::IdExhausted {
        prefix: 'D',
        attempts: crate::repo::ids::MAX_ID_ATTEMPTS,
    })
}

/// Stable lowercase string form of a [`DecisionOrigin`] for the `origin`
/// column (matches the schema CHECK's `IN ('authored','runtime','human')`).
fn origin_str(origin: DecisionOrigin) -> &'static str {
    match origin {
        DecisionOrigin::Authored => "authored",
        DecisionOrigin::Runtime => "runtime",
        DecisionOrigin::Human => "human",
    }
}

/// The raw `decisions` row as read from SQLite, before conversion to the typed
/// [`DecisionRecord`]. Kept separate because the typed record uses ID newtypes
/// and a typed enum that `sqlx::FromRow` cannot construct directly.
#[derive(sqlx::FromRow)]
struct DecisionRow {
    id: String,
    spec_id: String,
    phase_run_id: Option<String>,
    origin: String,
    title: String,
    summary: String,
    rationale: String,
    alternatives: Value,
    supersedes: Option<String>,
    created_at: DateTime<Utc>,
}

impl DecisionRow {
    /// Convert a raw row into a typed [`DecisionRecord`].
    ///
    /// Any malformed stored value (a bad ID, an unknown `origin`, corrupt
    /// `alternatives` JSON) is a loud [`RepoError`], never a silent default.
    fn into_record(self) -> Result<DecisionRecord, RepoError> {
        let bad = |what: &str| RepoError::NotFound(format!("corrupt decisions row: {what}"));

        let id = DecisionId::new(&self.id).map_err(|_| bad("id"))?;
        let spec_id = SpecId::new(&self.spec_id).map_err(|_| bad("spec_id"))?;
        let phase_run_id = match self.phase_run_id {
            Some(p) => Some(PhaseRunId::new(&p).map_err(|_| bad("phase_run_id"))?),
            None => None,
        };
        let origin = match self.origin.as_str() {
            "authored" => DecisionOrigin::Authored,
            "runtime" => DecisionOrigin::Runtime,
            "human" => DecisionOrigin::Human,
            other => return Err(bad(&format!("origin '{other}'"))),
        };
        let alternatives: Vec<RejectedAlternative> = serde_json::from_value(self.alternatives)?;
        let supersedes = match self.supersedes {
            Some(s) => Some(DecisionId::new(&s).map_err(|_| bad("supersedes"))?),
            None => None,
        };

        Ok(DecisionRecord {
            id,
            spec_id,
            phase_run_id,
            origin,
            title: self.title,
            summary: self.summary,
            rationale: self.rationale,
            alternatives,
            supersedes,
            created_at: self.created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::phase_runs::insert_start;
    use crate::repo::spec_versions::{VersionTrigger, append_version};
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::insert_task;
    use crate::types::ids::TaskId;
    use serde_json::json;

    /// A pool with a spec (version 1), a task, and one phase run — so a
    /// `runtime`/`human` decision has a real `phase_run_id` to FK to.
    async fn seeded_pool() -> (SqlitePool, SpecId, PhaseRunId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        let task = TaskId::new("T0000001a").unwrap();
        let pr = PhaseRunId::new("P0000001a").unwrap();
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
        (pool, spec, pr)
    }

    fn dec_id(s: &str) -> DecisionId {
        DecisionId::new(s).unwrap()
    }

    /// An authored decision (phase_run_id = None) and a runtime decision
    /// (phase_run_id = Some) both insert and round-trip through `fetch_by_id`.
    #[tokio::test]
    async fn insert_authored_and_runtime_both_succeed() {
        let (pool, spec, pr) = seeded_pool().await;

        let authored = DecisionRecord::new_authored(
            dec_id("D0000001a"),
            spec.clone(),
            None,
            "Use TOML".into(),
            "All config is TOML.".into(),
            "Single parser.".into(),
            vec![RejectedAlternative {
                name: "YAML".into(),
                reason: "whitespace".into(),
            }],
            None,
            Utc::now(),
        )
        .unwrap();
        insert(&pool, &authored).await.unwrap();

        let runtime = DecisionRecord::new_runtime(
            dec_id("D0000002b"),
            spec.clone(),
            Some(pr.clone()),
            "Use sqlx".into(),
            "Compile-checked queries.".into(),
            "Type safety.".into(),
            vec![],
            None,
            Utc::now(),
        )
        .unwrap();
        insert(&pool, &runtime).await.unwrap();

        let back_authored = fetch_by_id(&pool, &dec_id("D0000001a")).await.unwrap();
        assert_eq!(back_authored.origin, DecisionOrigin::Authored);
        assert!(back_authored.phase_run_id.is_none());
        assert_eq!(back_authored.alternatives.len(), 1);

        let back_runtime = fetch_by_id(&pool, &dec_id("D0000002b")).await.unwrap();
        assert_eq!(back_runtime.origin, DecisionOrigin::Runtime);
        assert_eq!(back_runtime.phase_run_id, Some(pr));
    }

    /// `fetch_by_spec` returns all decisions for the spec, sorted by
    /// created_at — including the authored one with a NULL phase_run_id.
    #[tokio::test]
    async fn fetch_by_spec_returns_all_sorted() {
        let (pool, spec, pr) = seeded_pool().await;
        let t0 = Utc::now() - chrono::Duration::minutes(2);
        let t1 = Utc::now();

        // Insert runtime-then-authored, but authored has the EARLIER timestamp.
        let runtime = DecisionRecord::new_runtime(
            dec_id("D0000002b"),
            spec.clone(),
            Some(pr),
            "second".into(),
            "s".into(),
            "r".into(),
            vec![],
            None,
            t1,
        )
        .unwrap();
        insert(&pool, &runtime).await.unwrap();
        let authored = DecisionRecord::new_authored(
            dec_id("D0000001a"),
            spec.clone(),
            None,
            "first".into(),
            "s".into(),
            "r".into(),
            vec![],
            None,
            t0,
        )
        .unwrap();
        insert(&pool, &authored).await.unwrap();

        let all = fetch_by_spec(&pool, &spec).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            all[0].title, "first",
            "sorted by created_at — authored first"
        );
        assert_eq!(all[1].title, "second");
    }

    /// The DB CHECK rejects an authored decision that carries a phase_run_id —
    /// a raw struct literal bypassing the constructor is still caught.
    #[tokio::test]
    async fn authored_with_phase_run_rejected_by_check() {
        let (pool, spec, pr) = seeded_pool().await;
        // Hand-craft the illegal record (the constructor would refuse this).
        let illegal = DecisionRecord {
            id: dec_id("D0000003c"),
            spec_id: spec,
            phase_run_id: Some(pr), // <- illegal for origin = Authored
            origin: DecisionOrigin::Authored,
            title: "x".into(),
            summary: "s".into(),
            rationale: "r".into(),
            alternatives: vec![],
            supersedes: None,
            created_at: Utc::now(),
        };
        let err = insert(&pool, &illegal).await.unwrap_err();
        assert!(
            matches!(err, RepoError::Sqlx(_)),
            "origin/phase_run_id mutex CHECK must reject this, got {err:?}",
        );
    }

    /// The `supersedes` partial UNIQUE allows only one decision to supersede a
    /// given prior decision — a second one is `RepoError::Duplicate`.
    #[tokio::test]
    async fn supersedes_unique_enforced() {
        let (pool, spec, pr) = seeded_pool().await;
        // The prior decision everyone wants to supersede.
        let prior = DecisionRecord::new_authored(
            dec_id("D0000001a"),
            spec.clone(),
            None,
            "prior".into(),
            "s".into(),
            "r".into(),
            vec![],
            None,
            Utc::now(),
        )
        .unwrap();
        insert(&pool, &prior).await.unwrap();

        // First superseding decision — accepted.
        let first = DecisionRecord::new_runtime(
            dec_id("D0000002b"),
            spec.clone(),
            Some(pr.clone()),
            "supersede A".into(),
            "s".into(),
            "r".into(),
            vec![],
            Some(dec_id("D0000001a")),
            Utc::now(),
        )
        .unwrap();
        insert(&pool, &first).await.unwrap();

        // Second decision superseding the SAME prior — rejected by the
        // partial UNIQUE index.
        let second = DecisionRecord::new_runtime(
            dec_id("D0000003c"),
            spec,
            Some(pr),
            "supersede A again".into(),
            "s".into(),
            "r".into(),
            vec![],
            Some(dec_id("D0000001a")),
            Utc::now(),
        )
        .unwrap();
        let err = insert(&pool, &second).await.unwrap_err();
        assert!(matches!(err, RepoError::Duplicate(_)), "got {err:?}");
    }

    /// `allocate_decision_id` yields a valid, currently-unused DecisionId that
    /// a subsequent `insert` accepts; two allocations differ.
    #[tokio::test]
    async fn allocate_decision_id_yields_usable_id() {
        let (pool, spec, pr) = seeded_pool().await;

        let id = allocate_decision_id(&pool).await.unwrap();
        let other = allocate_decision_id(&pool).await.unwrap();
        assert_ne!(id, other, "distinct allocations");

        // The allocated id is genuinely insertable.
        let record = DecisionRecord::new_runtime(
            id.clone(),
            spec,
            Some(pr),
            "t".into(),
            "s".into(),
            "r".into(),
            vec![],
            None,
            Utc::now(),
        )
        .unwrap();
        insert(&pool, &record).await.unwrap();
        assert_eq!(fetch_by_id(&pool, &id).await.unwrap().id, id);
    }

    /// `fetch_by_id` on a missing decision is `RepoError::NotFound`.
    #[tokio::test]
    async fn fetch_missing_decision_is_not_found() {
        let (pool, _spec, _pr) = seeded_pool().await;
        let err = fetch_by_id(&pool, &dec_id("D0000009z")).await.unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)), "got {err:?}");
    }
}
