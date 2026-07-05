//! BOI v2 — single-binary harness for orchestrating LLM-powered SWE tasks.
//!
//! Layered Domain Architecture enforced by module structure:
//!
//! ```text
//! types → config → repo → service → runtime → cli
//! ```
//!
//! Forward-only deps are enforced by `scripts/checks/module-dep-audit.sh`.
//! See `docs/design/2026-05-16-design.md` §13 for the full LDA rationale.

pub mod cli;
pub mod config;
pub mod repo;
pub mod runtime;
pub mod service;
pub mod types;
