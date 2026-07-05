# AGENTS.md — config (LDA layer 1)

TOML parsing + validation for the three formats (spec, phase declaration, pipeline).
Depends only on `types`. The `&str → RawSpec → validate → Spec` ingestion lives here.

- Enter at `mod.rs` for the layer `//!`.
- File map: `spec.rs` (spec TOML → `RawSpec`), `validate.rs` (every pre-normalization
  check), `phase.rs` (`~/.boi/v2/phases/<name>.toml`), `pipeline.rs` (pipeline TOML +
  the `<tasks>` fan-out sentinel), `load.rs` (filesystem loaders), `verify_spec.rs`
  (toolchain auto-detect from workspace markers), `verify_lint.rs` (regex pass over
  `Verification::Command` strings).
- Spec format reference: `tests/fixtures/specs/*.toml` (canonical examples).
- Rule: parse + validate only — no DB writes (that's `repo`), no execution (that's `runtime`).
