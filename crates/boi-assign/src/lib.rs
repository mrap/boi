//! BOI assignment plane — HRW selection, capability filtering, and the
//! revision-pinned claim CAS protocol used by Phase 4 (SEADA).
//!
//! The crate composes existing `boi-cluster` primitives:
//! - `nodes::NodeRecord` / `nodes::NodeCaps` for identity + caps,
//! - `membership::MembershipSnapshot` for the etcd-pinned view,
//! - `claims` for the CAS-backed claim protocol.

pub mod assign;
pub mod cooldown;
pub mod hrw;

pub use assign::{assign, AssignError, AssignResult, TaskRecord, MAX_RETRIES, STALE_WINDOW};
pub use cooldown::{
    clear_expired_cooldown, record_claim_failure, record_claim_success, ClaimFailures,
    CLAIM_FAILURES_PREFIX, COOLDOWN_WINDOW_SECS, FAILURE_THRESHOLD, HEALTH_DEGRADED, HEALTH_KEY,
};
pub use hrw::{capability_filter, hrw_rank, AssignNode, CapRequires};
