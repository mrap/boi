# AGENTS.md — service (LDA layer 3, business logic)

The orchestrator's brain: event bus + four-phase emit, state-machine enforcement, phase
routing, plan layer, the `<phase_context>` renderer. Depends on `types`/`config`/`repo`.

- Enter at `mod.rs` for the layer `//!` (it maps the modules by build phase).
- Key modules: `bus.rs` (the `EventBus` chokepoint + four-phase emit), `transitions.rs`
  (state-machine legality guard), `orchestrator/` (the spine), `routing.rs` (verdict
  routing + iteration caps — see its `//!`), `context.rs` + `renderer.rs` (§7.5
  `<phase_context>` assembly), `plan_layer.rs`, `adjustment.rs`, `scheduler.rs`,
  `sweeper.rs` (abandoned-run sweep), `mcp.rs` (the 4 worker MCP tools), `registry.rs`
  (the `PhaseExecutor` port).
- Rule: business logic only — persistence is `repo`, subprocess/git/LLM execution is
  `runtime`. A malformed routing graph fails **loudly at startup**, never mid-run.
