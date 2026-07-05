//! TOML parsing for specs, phase declarations, and pipelines.
//!
//! This is the `config` layer of BOI v2's Layered Domain Architecture. It
//! depends only on `crate::types`; it parses + validates the three TOML
//! formats from design §3–§4 (spec, phase declaration, pipeline) and
//! auto-detects a workspace's verify_spec toolchain (§3.4).
//!
//! - [`spec`] — the `&str` → [`RawSpec`](spec::RawSpec) → [`validate`] →
//!   [`Spec`] ingestion pipeline.
//! - [`phase`] — phase-declaration TOML (`~/.boi/v2/phases/<name>.toml`).
//! - [`pipeline`] — pipeline TOML (`~/.boi/v2/pipelines/<name>.toml`), incl.
//!   the `<tasks>` fan-out boundary sentinel.
//! - [`validate`] — every pre-normalization check on a [`RawSpec`](spec::RawSpec).
//! - [`verify_spec`] — toolchain auto-detection from workspace marker files.
//! - [`load`] — filesystem loaders for the phase + pipeline declarations
//!   (`~/.boi/v2/phases/*.toml`, `~/.boi/v2/pipelines/<name>.toml`) — the
//!   on-disk counterparts of [`phase`] / [`pipeline`]'s string parsers, used
//!   by `boi daemon` + `boi dispatch`.

pub mod load;
pub mod phase;
pub mod pipeline;
pub mod spec;
pub mod validate;
pub mod verify_lint;
pub mod verify_spec;
pub mod worktree;

// Convenience re-exports — the flat public surface of the config layer.
pub use load::{LoadError, load_phases, load_pipeline};
pub use phase::{PhaseDef, PhaseKind, PhaseLevel, RouteRule, VerdictTag, parse_phase};
// `PipelinePhase` (the `Phase(String) | Tasks` fan-out boundary sentinel,
// G13.3) is added here per erratum G17.1 — Phase 5a's orchestrator
// pattern-matches it to find the `<tasks>` boundary, and Task 2.7's original
// re-export list omitted it. Reachable as `crate::config::pipeline::PipelinePhase`
// regardless; this puts it on the flat surface where Phase 5a first imports it.
pub use pipeline::{PipelineDef, PipelinePhase, parse_pipeline};
pub use spec::{AuthoredDecision, ConfigError, Delivery, SkillRef, Spec, TaskDef, parse_spec};
pub use verify_lint::{Finding, lint};
pub use verify_spec::{DetectedToolchain, detect_toolchain};
// The design-§5 `[worktree]` retention config (audit C1) — read from
// `~/.boi/v2/config.toml` at daemon boot.
pub use worktree::{WorktreeConfig, WorktreeConfigError, load_worktree_config};
