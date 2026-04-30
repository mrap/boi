# Diagnostic: `runtime = "openrouter"` silently falls through to shell verify (not OpenRouter, not Claude)

**Date:** 2026-04-29  
**Spec:** boi-comprehensive-phase-logging (S9CE3)  
**Task:** T5FAA  
**Status:** Resolved in v1.3.0 — `ProviderRegistry` refactor replaced the ad-hoc `requires_claude` gate with a proper provider dispatch table. OpenRouter is now a first-class provider; `boi providers list` shows active/disabled state.

---

## Observed symptom

When `runtime = "openrouter"` is set in a phase TOML, every invocation is treated as a
shell verify phase. No LLM call is made. No error is logged. The telemetry record shows
`runtime: "verify"` — not "openrouter" — because the `resolved_runtime` computation itself
is gated on `requires_claude`, which Bug 1 sets to `false`. The original TOML value is
silently discarded before it reaches either the telemetry struct or the dispatch branch.

---

## What we expected vs. what happened

| Step | Expected | Actual |
|------|----------|--------|
| Phase TOML sets `runtime = "openrouter"` | Route to OpenRouter HTTP client | Phase routed to `run_verify_phase()` (shell command runner) |
| `boi.phase.invoked` event `runtime` field | `"openrouter"` | `"verify"` — `resolved_runtime` is set before branching but its own computation also hits the `!requires_claude` gate |
| Actual execution | OpenRouter API call | Shell verify command (or no-op if no verify cmd defined) |

---

## Root cause — two compounding bugs

### Bug 1: `requires_claude` derivation excludes "openrouter" (primary bug)

**File:** `src/phases.rs`, lines 221–229  
**Code:**
```rust
// Derive requires_claude: explicit [phase] setting wins, else derive from worker.runtime.
// "deterministic" and any non-"claude" value → false.
let requires_claude = toml
    .phase.as_ref().and_then(|p| p.requires_claude)
    .unwrap_or_else(|| {
        runtime.as_deref()
            .map(|r| r == "claude")   // ← BUG: only "claude" passes; "openrouter" returns false (line 227)
            .unwrap_or(true)
    });
```

When `runtime = "openrouter"`:
- `runtime.as_deref()` → `Some("openrouter")`
- `.map(|r| r == "claude")` → `Some(false)` (because "openrouter" ≠ "claude")
- `requires_claude` = `false`

In `src/runner.rs` line 304:
```rust
if !phase.requires_claude {
    let verdict = self.run_verify_phase(...);   // ← phase routed here! (line 305)
    return (verdict, String::new(), metrics);
}
```

**Effect:** Any phase with `runtime = "openrouter"` is treated as a shell-verify phase. The
verify command from the TOML's `[completion]` section is run instead of any LLM. If there
is no verify command, it silently returns a default verdict.

**The comment is wrong too.** The comment says `"deterministic" and any non-"claude" value →
false`, but "openrouter" is an AI runtime, not a shell-verify runtime. The intent was
probably `"deterministic" and non-AI values → false`.

### Bug 2: No OpenRouter HTTP client implementation (secondary bug)

Even if Bug 1 were fixed (so `requires_claude = true` for openrouter), `src/runner.rs`
line 335 unconditionally calls `worker::spawn_claude()` for all remaining phases:

```rust
// Claude phase — use the pre-built prompt.
// ...
let result = worker::spawn_claude(          // ← no branch on resolved_runtime (line 335)
    &prompt,
    worktree_path,
    timeout_secs,
    phase.model.as_deref(),
    spec_id,
    &self.claude_bin,
);
```

`worker::spawn_claude()` is in `src/spawn.rs` and only knows how to invoke the Claude CLI
binary. There is no `spawn_openrouter()` or HTTP client anywhere in the codebase.

Note: Bug 1 must be fixed first for Bug 2 to become relevant. With Bug 1 active,
execution never reaches `spawn_claude()` — it exits at `run_verify_phase()` (line 305).

**Evidence:** `grep -rn "openrouter" src/` returns only 3 hits — all in `runner.rs` — and
none of them are in a runtime dispatch branch. `src/spawn.rs` has zero openrouter references.

### Bug 3: `~/.boi/config/providers.yaml` is not read by the Rust binary

`~/.boi/config/providers.yaml` sets `default_provider: openrouter`. This file is from the
legacy Python BOI system. The Rust binary reads only `~/.boi/config.yaml` (via
`src/config.rs:38`). The `Config` struct has no `providers` field. So even the intended
provider routing from the Python system was never wired into the Rust implementation.

---

## Trace through the code

```
Phase TOML: runtime = "openrouter"
    ↓
phases.rs:218   runtime = Some("openrouter")
phases.rs:227   requires_claude = ("openrouter" == "claude") = false   ← BUG 1
    ↓
runner.rs:140   resolved_runtime:
                  phase.runtime == "deterministic"? No
                  !phase.requires_claude = !false = true  → resolved_runtime = "verify"  ← BUG 1 cascades
runner.rs:214   PhaseInvocation { runtime: "verify" }    ← NOT "openrouter"; raw TOML lost here
runner.rs:242   telemetry.emit_phase_invoked(...)         logs runtime = "verify"
    ↓
runner.rs:304   if !phase.requires_claude {   → TRUE (because requires_claude = false)
runner.rs:305       run_verify_phase(...)     ← execution lands here, NOT in Claude/OpenRouter
runner.rs:313       return                   ← exits before reaching spawn_claude()
    ↓
RESULT: shell verify command runs, or empty verdict if no verify defined.
        No LLM call. No error. No warning. Telemetry says "verify", original intent "openrouter" is gone.
```

If Bug 1 were fixed (so `requires_claude = true` for "openrouter"), `resolved_runtime` would
reach the `else` branch and correctly produce `"openrouter"`. But execution would still fall to:
```
runner.rs:335   worker::spawn_claude(...)    ← BUG 2: still Claude CLI, not OpenRouter
```

---

## Why telemetry shows "verify" instead of "openrouter"

`resolved_runtime` is computed at line 140 using three branches:

```rust
let resolved_runtime = if phase.runtime.as_deref() == Some("deterministic") {
    "deterministic"
} else if !phase.requires_claude {   // Bug 1 makes this true for "openrouter"
    "verify"
} else {
    phase.runtime.as_deref().unwrap_or("claude")  // "openrouter" would only reach here
};
```

Because Bug 1 sets `requires_claude = false` for any `runtime = "openrouter"` phase,
the second branch fires and `resolved_runtime = "verify"`. The raw TOML value `"openrouter"`
never propagates — it's discarded at line 142. The telemetry struct (`PhaseInvocation.runtime`)
uses `resolved_runtime`, so it also shows `"verify"`, not `"openrouter"`.

**The telemetry thus shows the wrong runtime (`"verify"`) and the wrong execution happens
(shell verify command instead of an LLM call).** Neither value reflects the TOML intent.
This makes the bug harder to spot from telemetry alone — you'd need to compare the raw
TOML against the `runtime` field to notice the discrepancy.

---

## Current state of phase TOMLs (2026-04-29)

Both `plan-critique` and `spec-critique` TOMLs currently read `runtime = "claude"` in both
the repo (`phases/`) and the production install (`~/.boi/phases/`). They have always been
set to "claude" in git history — the openrouter setting was not committed. The diagnostic
above explains what would happen (and has happened in local experiments) when "openrouter"
is set.

---

## Fix required (separate dispatch)

Two independent fixes are needed:

### Fix A — `requires_claude` derivation in `src/phases.rs:227`

Change the fallback logic to treat both "claude" and "openrouter" as AI runtimes:

```rust
// Before (broken):
.map(|r| r == "claude")

// After (correct):
.map(|r| matches!(r, "claude" | "openrouter"))
```

Or more explicitly, treat only explicitly non-AI runtimes as `requires_claude = false`:
```rust
.map(|r| !matches!(r, "deterministic" | "verify"))
```

### Fix B — Implement OpenRouter HTTP client dispatch in `src/runner.rs`

After the `requires_claude` gate, branch on `resolved_runtime`:
```rust
if resolved_runtime == "openrouter" {
    let result = worker::spawn_openrouter(&prompt, ...);
    // ...
} else {
    let result = worker::spawn_claude(&prompt, ...);
    // ...
}
```

`spawn_openrouter()` would call OpenRouter's `/chat/completions` endpoint using
`OPENROUTER_API_KEY`, the model from `phase.model`, and the prompt. Response parsing would
need to handle OpenAI-compatible streaming format rather than Claude's stream-json format.

### Fix C — Wire `~/.boi/config/providers.yaml` into Rust config (optional)

If per-provider configuration (base_url, credentials) needs to be user-overridable, add a
`providers` field to `src/config.rs:Config` and load it alongside `config.yaml`.

---

## Files to change (fix scope)

| File | Change |
|------|--------|
| `src/phases.rs:227` | Fix `requires_claude` derivation to include "openrouter" |
| `src/runner.rs:335` | Add `resolved_runtime == "openrouter"` branch |
| `src/spawn.rs` | Add `spawn_openrouter()` function |
| `src/config.rs` | Optionally: add `providers` config field |
