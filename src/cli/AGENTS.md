# AGENTS.md — cli (LDA layer 5, outermost)

The `boi` binary: `clap` derive tree, command dispatcher, top-level error renderer.
Imports every layer below; **nothing imports `cli/`** (`module-dep-audit.sh` enforces it).

- Enter at `mod.rs` for the layer `//!` (incl. the process model).
- **Process model:** `boi` runs as many short-lived OS processes; the orchestrator +
  `EventBus` live in ONE long-running `boi daemon`. They talk over a Unix control socket
  (`~/.boi/v2/daemon.sock`), not an in-process bus — see `control.rs` + `daemon.rs`.
- Command map (one file per command): `dispatch.rs`, `log.rs`, `cancel`/`recover.rs`,
  `clean.rs`, `spec.rs`, `traces.rs`, `mcp_serve.rs`, `boot.rs`; `dashboard/` (the TUI);
  `paths.rs`, `read_error.rs`. Authoritative surface: `boi --help`.
- Rule: `cli/` spawns no subprocess (the one interactive-shell spawn for
  `boi resolve-conflict` lives in `runtime::conflict`). Keep commands thin — logic lives
  in `service`/`runtime`.
