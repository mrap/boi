# AGENTS.md — types (LDA layer 0)

Pure data types: IDs, enums, `BoiEvent`, structs. **No I/O, no async, no internal deps.**
Depended on by every other layer — change a type here and the whole crate recompiles.

- Enter at `mod.rs` for the layer `//!`.
- File map: `ids.rs` (Crockford-b32 `SpecId`/`TaskId`/…), `event.rs` (`BoiEvent`),
  `decision.rs`, `plan.rs`, `reasons.rs`, `state.rs` (status enums), `step.rs`,
  `verdict.rs` (`WorkerVerdict`), `context.rs`.
- Rule: keep this layer **pure** — anything touching the filesystem, DB, or a subprocess
  belongs in `config`/`repo`/`runtime`, never here.
