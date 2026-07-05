//! Entity ID generation — random Crockford-base32 IDs with collision-retry.
//!
//! [`crate::types::ids`] defines the ID *newtypes* and their parse/validate
//! surface; this module *generates* fresh IDs and persists the owning row.
//!
//! ## The allocate / retry contract
//!
//! An `allocate_*` function generates a random ID, attempts the INSERT, and on
//! a PRIMARY KEY collision retries with a fresh ID — capped at
//! [`MAX_ID_ATTEMPTS`] attempts. After the cap it raises
//! [`RepoError::IdExhausted`] (a loud failure — never a silent spin). At a
//! 2^40 ID space a collision is already astronomically unlikely; the cap only
//! exists so that a *bug* (e.g. a broken RNG) cannot loop forever.
//!
//! ## Why two allocator shapes
//!
//! Plan Task 3.3 sketches `allocate_spec_id(pool, now)` and says "similar for
//! task / phase_run / decision" — but `specs` has only `(spec_id, created_at)`,
//! both of which the allocator owns, whereas `task_runtime` / `phase_runs` /
//! `decisions` carry FK columns and domain data the allocator cannot know.
//! That circular dependency (the columns are defined by Tasks 3.7/3.8/3.9) is
//! a plan defect. The minimal fix kept here:
//!
//! - [`allocate_spec_id`] matches the plan's `(pool, now)` signature exactly —
//!   it inserts the complete `specs` row.
//! - [`allocate_id`] is the generic generate-and-retry engine; the
//!   table-specific `allocate_*` wrappers in `specs.rs` / `task_runtime.rs` /
//!   `phase_runs.rs` / `decisions.rs` pass their own INSERT as a closure.
//!
//! `allocate_task_id` / `allocate_phase_run_id` / `allocate_decision_id` are
//! therefore *defined in their owning table modules* (and re-exported from
//! `repo`), not here — each needs that table's column set.

use std::future::Future;

use rand::Rng;
use sqlx::SqlitePool;

use crate::repo::db::RepoError;
use crate::types::ids::SpecId;

/// Crockford base32, lowercase, no confusables (`i`/`l`/`o`/`u`) — the exact
/// alphabet [`crate::types::ids`] validates against.
const ID_ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Random-body length following the 1-char type prefix.
const ID_BODY_LEN: usize = 8;

/// Retry cap for ID allocation. After this many PK collisions, allocation
/// fails loudly with [`RepoError::IdExhausted`] (Batch A review — P1).
pub const MAX_ID_ATTEMPTS: u32 = 5;

/// Generate a fresh random ID: `prefix` + `ID_BODY_LEN` Crockford-base32
/// chars (e.g. `random_id('S')` → `"Sxk3m9p2q"`).
///
/// The body is drawn from `rand`'s thread-local generator. This is collision-
/// resistance, not a cryptographic guarantee — IDs are not secrets and the
/// `allocate_*` retry loop backstops the rare PK collision. Output always
/// passes the [`crate::types::ids`] validator for the matching newtype.
pub fn random_id(prefix: char) -> String {
    let mut rng = rand::thread_rng();
    let mut s = String::with_capacity(ID_BODY_LEN + 1);
    s.push(prefix);
    for _ in 0..ID_BODY_LEN {
        let idx = rng.gen_range(0..ID_ALPHABET.len());
        s.push(ID_ALPHABET[idx] as char);
    }
    s
}

/// Generic generate-and-retry allocation engine.
///
/// Generates a candidate ID via [`random_id`] and hands it to `insert`.
/// `insert` performs the table-specific INSERT and reports back:
///
/// - `Ok(true)`  — the row was inserted; allocation succeeds with this ID.
/// - `Ok(false)` — a PRIMARY KEY collision; retry with a fresh ID.
/// - `Err(e)`    — any other failure; propagated immediately (no retry).
///
/// After [`MAX_ID_ATTEMPTS`] consecutive collisions, returns
/// [`RepoError::IdExhausted`].
pub async fn allocate_id<F, Fut>(prefix: char, mut insert: F) -> Result<String, RepoError>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<bool, RepoError>>,
{
    for _ in 0..MAX_ID_ATTEMPTS {
        let candidate = random_id(prefix);
        if insert(candidate.clone()).await? {
            return Ok(candidate);
        }
    }
    Err(RepoError::IdExhausted {
        prefix,
        attempts: MAX_ID_ATTEMPTS,
    })
}

/// Classify an INSERT result as success / PK-collision / hard error.
///
/// Shared by every `allocate_*` wrapper: a SQLite primary-key violation means
/// "retry with a fresh ID"; anything else is a real failure.
pub(crate) fn insert_result(
    res: Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error>,
) -> Result<bool, RepoError> {
    match res {
        Ok(_) => Ok(true),
        Err(sqlx::Error::Database(e)) if is_pk_collision(&*e) => Ok(false),
        Err(e) => Err(RepoError::Sqlx(e)),
    }
}

/// True when a database error is a PRIMARY KEY / UNIQUE constraint violation.
fn is_pk_collision(e: &dyn sqlx::error::DatabaseError) -> bool {
    e.is_unique_violation()
}

/// Allocate a fresh [`SpecId`] and insert its `specs` identity row.
///
/// `specs` is self-contained (`spec_id` + `created_at`), so unlike the other
/// three entity tables this allocator owns the entire INSERT — matching the
/// plan's `allocate_spec_id(pool, now)` signature.
pub async fn allocate_spec_id(
    pool: &SqlitePool,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<SpecId, RepoError> {
    let raw = allocate_id('S', |id| async move {
        let res = sqlx::query!(
            "INSERT INTO specs (spec_id, created_at) VALUES (?1, ?2)",
            id,
            now,
        )
        .execute(pool)
        .await;
        insert_result(res)
    })
    .await?;
    // `random_id('S')` always produces a valid SpecId — a parse failure here
    // would be a bug in this module, not bad input.
    SpecId::new(&raw).map_err(|e| RepoError::Duplicate(format!("generated invalid spec id: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::types::ids::{DecisionId, PhaseRunId, TaskId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A newtype-validation probe — `fn` pointer so an array of them shares one
    /// element type (and stays under clippy's type-complexity threshold).
    type Validate = fn(&str) -> bool;

    /// Every body char of a generated ID is in the Crockford alphabet, and the
    /// result parses into the matching newtype.
    #[test]
    fn random_id_is_well_formed_base32() {
        let kinds: [(char, Validate); 4] = [
            ('S', |s| SpecId::new(s).is_ok()),
            ('T', |s| TaskId::new(s).is_ok()),
            ('P', |s| PhaseRunId::new(s).is_ok()),
            ('D', |s| DecisionId::new(s).is_ok()),
        ];
        for (prefix, parse) in kinds {
            for _ in 0..200 {
                let id = random_id(prefix);
                assert_eq!(id.len(), ID_BODY_LEN + 1);
                assert_eq!(id.chars().next(), Some(prefix));
                for c in id.chars().skip(1) {
                    assert!(
                        ID_ALPHABET.contains(&(c as u8)),
                        "char '{c}' not in Crockford alphabet",
                    );
                }
                assert!(parse(&id), "generated id `{id}` failed newtype validation");
            }
        }
    }

    /// A fresh `allocate_spec_id` inserts the `specs` row and returns a valid
    /// `SpecId` that is actually present.
    #[tokio::test]
    async fn allocate_spec_id_inserts_row() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let id = allocate_spec_id(&pool, chrono::Utc::now()).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM specs WHERE spec_id = ?1")
            .bind(id.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    /// The generic allocator retries past a single forced collision and
    /// succeeds on the second attempt.
    #[tokio::test]
    async fn allocate_id_retries_one_collision() {
        let attempts = AtomicUsize::new(0);
        let id = allocate_id('S', |_candidate| {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                // First attempt: simulate a PK collision. Second: succeed.
                Ok(n != 0)
            }
        })
        .await
        .unwrap();
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "should retry exactly once"
        );
        assert!(SpecId::new(&id).is_ok());
    }

    /// Five consecutive collisions exhaust the retry cap and raise
    /// `RepoError::IdExhausted` — a loud failure, never a silent spin.
    #[tokio::test]
    async fn allocate_id_exhaustion_raises_id_exhausted() {
        let attempts = AtomicUsize::new(0);
        let result = allocate_id('T', |_candidate| {
            attempts.fetch_add(1, Ordering::SeqCst);
            async move { Ok(false) } // always "collides"
        })
        .await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            MAX_ID_ATTEMPTS as usize,
            "should make exactly MAX_ID_ATTEMPTS attempts",
        );
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                RepoError::IdExhausted {
                    prefix: 'T',
                    attempts: MAX_ID_ATTEMPTS,
                }
            ),
            "expected IdExhausted, got {err:?}",
        );
    }

    /// A real `specs` PK collision (seed a duplicate, then force the allocator
    /// onto the same first candidate via a closure) is classified as a retry,
    /// not a hard error — `insert_result` maps a unique violation to
    /// `Ok(false)`.
    #[tokio::test]
    async fn insert_result_maps_pk_collision_to_retry() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let now = chrono::Utc::now();
        // Seed a row.
        sqlx::query!(
            "INSERT INTO specs (spec_id, created_at) VALUES (?1, ?2)",
            "S0000000a",
            now,
        )
        .execute(&pool)
        .await
        .unwrap();
        // A duplicate INSERT must classify as a collision (Ok(false)), not Err.
        let res = sqlx::query!(
            "INSERT INTO specs (spec_id, created_at) VALUES (?1, ?2)",
            "S0000000a",
            now,
        )
        .execute(&pool)
        .await;
        assert!(
            !insert_result(res).unwrap(),
            "a PK collision must classify as a retry (Ok(false)), not an error",
        );
    }
}
