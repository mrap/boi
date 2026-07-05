//! Red test for task T2xs95v6j (spec Setw4sqzr — "strip $ from BOI v2,
//! keep tokens").
//!
//! Per the 2026-06-01 directive ("strip dollars everywhere, keep tokens
//! everywhere"), the per-phase cost column is gone from `boi log` output —
//! but the rendering module still carries five textual `$` artifacts:
//! two comments referencing the strip, the `{:<width$}` format-spec syntax
//! used for the label column, and the in-file assertion (`!report.contains('$')`,
//! plus its message) that was the previous test for the rendered output.
//!
//! T2xs95v6j is the task that finishes the job — drop the artifacts and
//! sweep the now-vestigial in-file `$`-asserting test. The workspace
//! verification (`! grep -q '\$' src/cli/log.rs`) makes the same demand;
//! this test makes it a Rust-level regression guard so re-adding any `$`
//! (a stray comment, a re-introduced `{:<W$}` width spec, a re-added cost
//! column) trips a unit test instead of silently surviving CI.
//!
//! ## How the test pinpoints the contract
//!
//! `include_str!` pulls the live source of `src/cli/log.rs` at compile
//! time, then a single `matches('\u{24}').count()` walks every byte and
//! reports the position of the first remaining `$` — which is the exact
//! spot the strip missed. The dollar-sign codepoint is named via its
//! Unicode escape (`\u{24}`) rather than the literal `'$'` so this test
//! file itself stays grep-clean (the workspace verification only scopes
//! `src/cli/log.rs`, but this keeps the regression-guard's own source
//! free of the very character it forbids — no exception carved for it).
//!
//! ## Red / green
//!
//! Today: five `$` characters remain in `src/cli/log.rs` (two comments,
//! one format-spec width syntax, one assertion literal, one assertion
//! message) — the test reports `5 != 0` with the first byte offset and
//! FAILS. RED.
//!
//! After T2xs95v6j strips them and sweeps the in-file `'$'`-asserting
//! test: `matches('\u{24}').count() == 0`, the test passes. GREEN.
//!
//! ## Test-binary isolation
//!
//! This file is its own cargo-auto-discovered integration-test binary,
//! separate from the `lib` test binary and from the L3 integration
//! binary. It pulls in no boi-crate symbols — it's a pure compile-time
//! source-content check — so its red signal is scoped exactly to the
//! source-level invariant this task must satisfy, and it never blocks
//! the other binaries from running.

/// The live source of `src/cli/log.rs`, pulled in at compile time.
/// Relative path is from this file's directory (`tests/`) up one level
/// into the crate root, then into `src/cli/log.rs`.
const LOG_RS_SOURCE: &str = include_str!("../src/cli/log.rs");

/// The dollar-sign codepoint, named via its Unicode escape so this
/// regression-guard file itself stays grep-clean for `'$'`.
const DOLLAR: char = '\u{24}';

#[test]
fn test_l2_log_module_source_carries_no_dollar_sign_artifacts() {
    let count = LOG_RS_SOURCE.matches(DOLLAR).count();
    if count != 0 {
        // Pinpoint the first remaining occurrence so the worker fixing
        // this knows exactly where to look.
        let first = LOG_RS_SOURCE
            .find(DOLLAR)
            .expect("count > 0 implies find returns Some");
        // A short window of context around the offending byte makes the
        // failure message self-diagnosing without needing to crack open
        // the file.
        let lo = first.saturating_sub(20);
        let hi = (first + 20).min(LOG_RS_SOURCE.len());
        // Walk to char boundaries so the slice is valid UTF-8.
        let safe_lo = (lo..=first)
            .rev()
            .find(|&i| LOG_RS_SOURCE.is_char_boundary(i))
            .unwrap_or(0);
        let safe_hi = (first..=hi)
            .find(|&i| LOG_RS_SOURCE.is_char_boundary(i))
            .unwrap_or(LOG_RS_SOURCE.len());
        let snippet = &LOG_RS_SOURCE[safe_lo..safe_hi];
        panic!(
            "src/cli/log.rs must carry zero dollar-sign artifacts after \
             the 2026-06-01 strip-dollars directive; found {count} \
             occurrence(s) — first at byte offset {first}, context: {snippet:?}"
        );
    }
}

/// Companion green-rail: the strip must leave the tokens column intact.
/// `tokens=` is the contract — `format_metrics` builds the per-row
/// `tokens={tokens_in}/{tokens_out}` string. If the worker fixing
/// T2xs95v6j over-rotates and accidentally drops the tokens column too,
/// this test catches it.
#[test]
fn test_l2_log_module_source_still_renders_the_tokens_column() {
    assert!(
        LOG_RS_SOURCE.contains("tokens="),
        "src/cli/log.rs must still render the per-row `tokens=X/Y` column \
         (it's what the dollar column was replaced with); the literal \
         `tokens=` was not found in the file"
    );
}
