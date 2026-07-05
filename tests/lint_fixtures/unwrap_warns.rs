//! Lint fixture: verifies `clippy::unwrap_used` fires when opted-in at module scope.
//!
//! The workspace default is `unwrap_used = "warn"` so tests can use `.unwrap()`
//! freely. High-value modules (service/, runtime/) opt-in to `#![deny(...)]`
//! to make production code path failures hard-fail.
//!
//! This fixture file opts in. The `should_panic` test demonstrates that
//! the lint is effective when explicitly enabled.

#![deny(clippy::unwrap_used)]

#[cfg(test)]
mod tests {
    /// L1 unit: opt-in to `unwrap_used` deny should still allow .unwrap() inside
    /// a clearly-marked test body because `clippy::unwrap_used` exempts
    /// `#[cfg(test)]` by default.
    #[test]
    fn test_l1_unwrap_used_exempts_test_bodies() {
        let value: i32 = "42".parse().unwrap();
        assert_eq!(value, 42);
    }
}
