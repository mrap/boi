//! Red test for task T36d62j66 (spec Setw4sqzr — "strip $ from BOI v2,
//! keep tokens").
//!
//! Per the 2026-06-01 directive, the per-run dollar field has been
//! ripped out of the `phase_runs` schema (migration 0003) and the pricing
//! module deleted. The matching Rust DTO fields must also leave —
//! `PhaseRunRow`'s row-level dollar column AND `SpecPhaseMetrics`'s spec-
//! level dollar rollup. T36d62j66 is the task that removes them.
//!
//! ## How the test pinpoints the contract
//!
//! Each struct is exhaustively destructured below WITHOUT a `..` wildcard,
//! listing every field that MUST remain post-strip. Token fields stay
//! (explicitly listed); any leftover dollar field surfaces as
//! `E0027 pattern does not mention field <name>` right at the destructure
//! site — the compiler names the exact field the strip missed.
//!
//! ## Red / green
//!
//! Today: each struct still carries the dollar field, so the non-
//! exhaustive patterns fail to compile (E0027) — RED.
//!
//! Once T36d62j66 removes the dollar fields: the patterns match
//! exhaustively, the binary compiles, and both `#[test]` bodies are
//! trivially true — GREEN.
//!
//! ## Test-binary isolation
//!
//! This file is its own cargo-auto-discovered test binary, separate from
//! the `lib` test binary and from the L3 integration binary. The compile
//! error here therefore does not block the other binaries from running —
//! the red signal is scoped exactly to the structural contract this task
//! must satisfy.
//!
//! The test text below does NOT mention the to-be-stripped column name as
//! a literal string — the destructure naming the SURVIVING fields is the
//! whole contract — so the file lives forever as a regression guard
//! (re-adding the dollar field re-introduces E0027).

use boi::repo::phase_runs::{PhaseRunRow, SpecPhaseMetrics};

#[test]
fn test_l2_phase_run_row_post_strip_field_set_is_exact() {
    // Compile-time witness: PhaseRunRow has exactly these sixteen fields.
    // Tokens stay (tokens_in / tokens_out). No dollar field. If the strip
    // misses one, the compiler errors here naming it.
    fn _bind(row: PhaseRunRow) {
        let PhaseRunRow {
            id: _,
            spec_id: _,
            task_id: _,
            phase: _,
            phase_iteration: _,
            spec_version: _,
            provider: _,
            worker_id: _,
            files_touched: _,
            synopsis: _,
            verdict: _,
            last_heartbeat_at: _,
            started_at: _,
            completed_at: _,
            tokens_in: _,
            tokens_out: _,
        } = row;
    }
}

#[test]
fn test_l2_spec_phase_metrics_post_strip_field_set_is_exact() {
    // Compile-time witness: SpecPhaseMetrics has exactly these four
    // fields. Token totals stay. No dollar rollup — any `metrics`
    // consumer of this struct must therefore not carry the dollar
    // total either.
    fn _bind(m: SpecPhaseMetrics) {
        let SpecPhaseMetrics {
            phases_run: _,
            duration_ms: _,
            total_tokens_in: _,
            total_tokens_out: _,
        } = m;
    }
}
