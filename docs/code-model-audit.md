# code_model Field Audit

**Date:** 2026-05-12
**Spec:** S1AAA / TF395
**Question:** Is `code_model` in `PhaseConfig` actively used (read and applied to model selection), or is it dead code?

---

## All References Found

### src/phases.rs

**Line 91 — Struct field declaration (WRITE/definition)**
```
88:     pub on_crash: Option<String>,
89:     pub min_lines_changed: Option<u32>,
90:     pub model: Option<String>,
91:     pub code_model: Option<String>,      ← struct field
92:     pub effort: Option<String>,
93:     pub hooks_pre: Vec<String>,
94:     pub hooks_post: Vec<String>,
```
Usage: **WRITE** (struct field declaration in `PhaseConfig`)

---

**Line 150 — TOML deserialization struct (WRITE/definition)**
```
147:     #[serde(default)]
148:     model: Option<String>,
149:     #[serde(default)]
150:     code_model: Option<String>,          ← in PhaseTomlWorker
151: }
```
Usage: **WRITE** (field in intermediate TOML deserialization struct `PhaseTomlWorker`)

---

**Line 260 — Extract from TOML into local variable (READ from TOML struct)**
```
257:         let on_crash = completion.and_then(|c| c.on_crash.clone());
258:         let min_lines_changed = toml.trigger.as_ref().and_then(|t| t.min_lines_changed);
259:         let model = toml.worker.as_ref().and_then(|w| w.model.clone());
260:         let code_model = toml.worker.as_ref().and_then(|w| w.code_model.clone()); ← extracted
261:         let effort = toml.worker.as_ref().and_then(|w| w.effort.clone());
```
Usage: **READ** from `PhaseTomlWorker`, stores into local `code_model` variable

---

**Line 284 — Store into PhaseConfig (WRITE)**
```
281:             on_crash,
282:             min_lines_changed,
283:             model,
284:             code_model,                   ← stored into PhaseConfig
285:             effort,
286:             hooks_pre,
287:             hooks_post,
```
Usage: **WRITE** (stored into `PhaseConfig` struct during construction)

---

**Lines 1002, 1026 — Test fixtures (WRITE)**
```
999:             on_crash: None,
1000:            min_lines_changed: None,
1001:            model: None,
1002:            code_model: None,             ← test fixture
1003:            effort: None,
```
(Same pattern at 1026, 1597–1600, 1629–1632, 1660–1663, 1713–1716, 1800–1803)
Usage: **WRITE** (test fixture initialization, always `None`)

---

**Line 1349 — Test fixture TOML string (WRITE)**
```
1346: [worker]
1347: runtime = "claude"
1348: model = "claude-sonnet-4-6"
1349: code_model = ""                          ← inline TOML in test
1350: prompt_template = "templates/worker-prompt.md"
```
Usage: **WRITE** (inline TOML string in test, set to empty string `""`)

---

### src/runner.rs

**Line 889 — Test fixture (WRITE)**
```
886:             on_crash: None,
887:             min_lines_changed: None,
888:             model: None,
889:             code_model: None,             ← test fixture
890:             effort: None,
```
Usage: **WRITE** (test fixture, always `None`)

---

### src/runtime/mod.rs

**Lines 207, 373, 546, 935 — Test fixtures (WRITE)**
```
code_model: None,                             ← test fixtures (4 occurrences)
```
Usage: **WRITE** (test fixtures in `PhaseConfig` construction, always `None`)

---

### src/runtime/claude.rs

**Line 113 — Test fixture (WRITE)**
```
110:             on_crash: None,
111:             min_lines_changed: None,
112:             model: None,
113:             code_model: None,             ← test fixture
114:             effort: None,
```
Usage: **WRITE** (test fixture, always `None`)

---

### src/builtins.rs

**Line 102 — Test fixture (WRITE)**
```
99:              on_crash: None,
100:             min_lines_changed: None,
101:             model: None,
102:             code_model: None,             ← test fixture
103:             effort: None,
```
Usage: **WRITE** (test fixture, always `None`)

---

### tests/test_phase_override_apply.rs

**Line 45 — Test fixture (WRITE)**
```
42:             on_crash: None,
43:             min_lines_changed: None,
44:             model: None,
45:             code_model: None,             ← test fixture
46:             effort: None,
```
Usage: **WRITE** (test fixture, always `None`)

---

### phases/execute.phase.toml

**Line 13 — Production phase config (WRITE)**
```
10: [worker]
11: runtime = "claude"
12: model = "claude-sonnet-4-6"
13: code_model = ""                           ← set to empty string in real phase file
14: prompt_template = "templates/worker-prompt.md"
```
Usage: **WRITE** (the only production `.toml` file with `code_model` set — to `""`)

---

## Summary Table

| File | Line(s) | Type | Usage |
|------|---------|------|-------|
| src/phases.rs | 91 | Struct field declaration | WRITE |
| src/phases.rs | 150 | TOML deser struct field | WRITE |
| src/phases.rs | 260 | Extract from TOML | READ (from TOML struct) |
| src/phases.rs | 284 | Store into PhaseConfig | WRITE |
| src/phases.rs | 1002,1026,1600,1632,1663,1716,1803 | Test fixtures | WRITE (always None) |
| src/phases.rs | 1349 | Test TOML string | WRITE (empty string "") |
| src/runner.rs | 889 | Test fixture | WRITE (always None) |
| src/runtime/mod.rs | 207,373,546,935 | Test fixtures | WRITE (always None) |
| src/runtime/claude.rs | 113 | Test fixture | WRITE (always None) |
| src/builtins.rs | 102 | Test fixture | WRITE (always None) |
| tests/test_phase_override_apply.rs | 45 | Test fixture | WRITE (always None) |
| phases/execute.phase.toml | 13 | Production phase config | WRITE (empty string "") |

## Key Finding

`code_model` is **NEVER READ back from `PhaseConfig`** after being stored there.

The runner (`src/runner.rs`) uses `phase.model` (not `phase.code_model`) everywhere:
- Line 249: `if let Some(m) = &phase.model { args.push("--model"...`
- Line 277: `model: phase.model.clone()`
- Line 433: `model: phase.model.as_deref().unwrap_or("")`

The only "read" is at `src/phases.rs:260` where it's extracted from the TOML intermediate struct to be stored in `PhaseConfig` — but that stored value is never consumed again.

**No phase.toml files (other than `execute.phase.toml`) set `code_model`.** The one that does sets it to `""` (empty string), which maps to `None` after the `Option<String>` deserialization logic at line 260 (empty string → `Some("")` which gets stored, but is never read).

The field `code_model` is dead code. Setting it in a phase.toml silently has no effect.

---

## Root Cause Summary

`code_model` in `PhaseConfig` is dead code introduced at some point with the intent of allowing per-phase model overrides for the "code" role (distinct from the orchestration `model`). However, the consumer side was never implemented: `src/runner.rs` reads only `phase.model` when constructing `--model` args (lines 249, 277, 433). No code path reads `phase.code_model` after it is stored in `PhaseConfig`. As a result, any user who sets `code_model` in a `phase.toml` will receive no error, no warning, and silently no effect — a silent misconfiguration hazard.

## Recommendation

**Short term (done):** Add deprecation comments to both struct fields so the next reader understands the field is inert. Remove `code_model = ""` from `phases/execute.phase.toml` where it was misleadingly present.

**Long term (not done here):** Either (a) implement the feature — wire `phase.code_model` into the runner so it actually overrides the model for code tasks — or (b) fully remove the field: drop it from `PhaseConfig`, remove the extraction at `src/phases.rs:260`, and purge the ~15 test-fixture initialization sites. Removing requires a coordinated multi-file cleanup; deprecation comments are the safe minimal fix for now.

## Action Taken

1. **`src/phases.rs:91`** — Added deprecation comment to `PhaseConfig.code_model` explaining the field is parsed but never consumed, and directing users to `model`.
2. **`src/phases.rs:150`** — Added comment to `PhaseTomlWorker.code_model` noting it is kept for TOML backwards compatibility only.
3. **`phases/execute.phase.toml`** — Removed `code_model = ""` line (the only production phase file that set it). It was dead and misleading.
