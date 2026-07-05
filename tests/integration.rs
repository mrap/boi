//! The L3 orchestrator-integration test crate (Phase 10).
//!
//! Cargo auto-discovers `tests/integration.rs` as one test binary; the L3
//! modules live in `tests/integration/` and are wired in here.
//!
//! ## L3 ≠ end-to-end (review (b))
//!
//! These tests drive the **real** orchestrator, event bus, and SQLite repo
//! against [`boi::service::testkit::MockExecutor`] — the
//! `#[cfg(feature = "testkit")]` test double (G16.1). They are an
//! **orchestrator-integration** tier: fast, fully hermetic, no subprocess and
//! no live LLM. The `runtime/` provider modules the mock bypasses
//! (`recipe` / `stream` / `goose` / `preflight` / `mcp_server`) get their
//! §13.3 coverage from the Docker E2E (Task 10.5), recorded in
//! `tests/E2E_COVERED.toml`. "End-to-end" is reserved for that harness.
//!
//! - [`harness`] (Task 10.2) — the dispatch-a-fixture / drive-to-quiescence
//!   harness every L3 test is built on.
//! - [`fixtures`] (Task 10.3) — one `test_l3_*` per §13 fixture spec.
//! - [`failures`] (Task 10.4) — a producing L3 test per producible
//!   `BlockedReason` / `FailureReason` (§13.3, Lec 10).
//! - [`recovery`] (audit A2) — the design-§6 operator-recovery loop, driven
//!   end-to-end: block → spec still revivable → `boi unblock` → completed.
//!
//! ## Lint posture
//!
//! `Cargo.toml`'s `unwrap_used` / `expect_used` / `panic` are `warn` lints;
//! `clippy --all-targets -D warnings` escalates them. `clippy.toml`'s
//! `allow-*-in-tests` keys exempt `#[test]` / `#[cfg(test)]` bodies — but this
//! crate's *harness helper functions* are not `#[test]` items, so clippy still
//! flags them. A test harness `.expect()`-ing on a malformed fixture, or
//! `panic!`-ing on an unresolvable id, is the correct loud-fail behaviour — so
//! the three lints are allowed crate-wide here (this whole crate IS test
//! support; there is no production code in it).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

// `tests/integration.rs` is the test crate's ROOT file, so a bare `mod foo;`
// resolves to `tests/foo.rs` (the crate-root rule), not `tests/integration/`.
// Explicit `#[path]` points each module at its file under `tests/integration/`.
#[path = "integration/harness.rs"]
mod harness;

#[path = "integration/failures.rs"]
mod failures;
#[path = "integration/fixtures.rs"]
mod fixtures;
#[path = "integration/reclaim.rs"]
mod reclaim;
#[path = "integration/recovery.rs"]
mod recovery;
#[path = "integration/sweeper.rs"]
mod sweeper;
