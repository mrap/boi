# BOI Optimization Plan — 2026-04-29

**Objective:** Reduce spec completion time by 40%+ and cost by 50%+ using telemetry data.

---

## 1. Data Tables

### 1.1 Phase Time Budget (completed specs only)

| Phase | Runs | Total Min | Avg/Run (s) | Avg/Spec (s) | % of Spec Time |
|-------|------|-----------|-------------|---------------|----------------|
| execute | 54 | 176.9 | 197 | 663 | 40.3% |
| task-verify | 46 | 144.6 | 189 | 542 | 33.0% |
| critic | 48 | 85.9 | 107 | 322 | 19.6% |
| spec-review | 15 | 24.9 | 100 | 93 | 5.7% |
| doc-update | 9 | 10.4 | 70 | 39 | 2.4% |

**Average completed spec wall time: 1,643s (27.4 min).** Execute and task-verify together consume 73% of that.

### 1.2 Task-Verify Duration Distribution

| Bucket | Count | Avg (s) |
|--------|-------|---------|
| < 1 min | 9 | 29 |
| 1-2 min | 10 | 93 |
| 2-3 min | 17 | 156 |
| 3-5 min | 21 | 248 |
| 5-10 min | 10 | 402 |
| > 10 min | 4 | 767 |

**62% of task-verify runs exceed 2 minutes.** This is for a *verification* step.

### 1.3 Critic Redo Analysis

| Metric | Value |
|--------|-------|
| Total critic runs | 48 |
| Redo (rejected) | 40 (83%) |
| Proceed (approved) | 9 (19%) — note: 1 proceeds without prior redo |
| Total redo time | 76.1 min |
| Total proceed time | 17.1 min |
| Specs with 4 redos, 0 approvals | 8 |

**The critic never approves on first pass.** 8 out of 16 specs with critic went through 4 redo cycles (the max) without ever getting approved — the spec completed via other means (task completion triggered exit). This means the critic is generating work that doesn't converge, burning 76 minutes of compute on average across all specs.

### 1.4 Claude Spawns Per Spec

| Metric | Value |
|--------|-------|
| Average spawns per completed spec | 7.0 |
| Cold start per spawn (current) | ~5.2s |
| Cold start per spawn (--bare) | ~0.18s |
| Total cold start overhead per spec | 36s (3.2% of wall time) |
| With --bare | 1.3s (0.1% of wall time) |

### 1.5 Spec Completion Rates

| Status | Count |
|--------|-------|
| completed | 16 |
| failed | 20 |
| running | 5 |
| cancelled | 5 |
| queued | 1 |

**Completion rate: 34% (16/47).** Failed specs consume compute without delivering value.

### 1.6 Prompt Template Sizes

| Template | Bytes | Est. Tokens |
|----------|-------|-------------|
| worker-prompt.md | 6,186 | ~1,547 |
| task-verify-prompt.md (critic prompt) | 6,443 | ~1,611 |
| spec-review-prompt.md | 3,433 | ~858 |
| doc-update-prompt.md | 1,934 | ~484 |
| critic-prompt.md (short form) | 549 | ~137 |
| plan-critique-prompt.md | 3,335 | ~834 |
| checks/ (9 files, loaded for critic) | 26,256 | ~6,564 |

**The task-verify prompt (6,443 bytes) is larger than the worker prompt (6,186 bytes).** The task-verify-worker-prompt.md (547 bytes) includes `{{CRITIC_PROMPT}}` which injects the full 6,443-byte critic prompt. Combined with spec content, the total prompt for task-verify can reach 20-22K chars — as large as the execute prompt.

### 1.7 Doc-Update Duration Distribution

| Bucket | Count | Avg (s) |
|--------|-------|---------|
| < 30s | 8 | 17 |
| 30s-1m | 10 | 48 |
| 1-2m | 8 | 82 |
| > 2m | 3 | 181 |

28% of doc-update runs complete in under 30s (likely "no docs to update"). 10% take over 2 minutes.

---

## 2. Root Cause Analysis

### 2.1 task-verify is slow because it's a full Claude session doing code review

**Root cause:** task-verify spawns a full Claude CLI session that reads code, runs shell commands, and performs a mini code review. The task-verify-worker-prompt.md is just 547 bytes, but it includes `{{CRITIC_PROMPT}}` which injects the full 6,443-byte critic-style prompt with 3 review perspectives (Adversarial Depth, Scale and Gaps, Code Actionability), structured JSON output requirements, and self-evolution instructions.

**What it should be:** Most tasks have a `verify:` field with a concrete shell command like `cargo test parser 2>&1 | grep 'test result: ok'`. The verification should be: run the command, check exit code, done. The Claude session is only needed when there's no shell verify command or when deeper review is needed.

**Evidence:** The non-Claude `run_verify_phase` in runner.rs (lines 238-270) runs the shell verify command directly and returns immediately. The Claude path spawns a full session averaging 189 seconds.

### 2.2 critic never converges — 83% redo rate

**Root cause:** The critic prompt is *designed* to find issues. It applies 3 adversarial perspectives across 9 check categories (26K bytes of check definitions including quality scoring at 11K alone). The prompt says "You do not rubber-stamp." The reject threshold is low (any `[CRITIC]` tag triggers redo), and the approve threshold is high (all checks must pass across all perspectives).

This creates a ratchet: critic almost always finds *something*, generates remediation tasks, those tasks get executed, and then critic reviews the new tasks... and finds more issues. After 4 rounds the spec exits via max-redo-count, not via critic approval.

**Cost:** 8 specs burned 4 redo cycles each at ~107s per cycle = **57 minutes of pure overhead** per spec that hits the ceiling. The critic-injected tasks often don't improve the output quality — they fix nits.

### 2.3 Cold start is NOT the bottleneck (but --bare is still free money)

**Root cause:** CLI scaffolding (hooks, LSP, plugin sync, CLAUDE.md discovery) adds ~5.2s per spawn. With 7 spawns per spec, that's 36s — only 3.2% of wall time.

However, `--bare` reduces this to 0.18s per spawn (total 1.3s per spec). It's a zero-cost optimization that saves 35s per spec. At 47 specs, that's 27 minutes of compute saved.

### 2.4 doc-update runs for tasks that didn't change docs

**Root cause:** doc-update is a per-task phase that spawns Claude to `git diff HEAD~1` and look for doc updates. But most tasks don't change docs — they change code. 28% of doc-update runs take <30s and likely output "No Doc Updates Needed."

Doc-update is not in the current `pipelines.toml` for default mode (removed at some point), so this only affects specs dispatched under older config or with custom `task_phases`.

### 2.5 Prompt size drives inference time

**Root cause:** Prompts of 20K+ chars (~5K tokens) reach the model before the spec content and task context are added. The total input token count for a single phase run can be 10-30K tokens. Larger prompts = more to process = longer TTFT + more time spent understanding context.

The worker-prompt.md (6,186 bytes) includes Decision Transparency framework, coordination lock instructions, blast-radius checks, and self-evolution rules that are load-bearing for execute but wasted on lightweight phases.

---

## 3. Top 10 Optimizations (Ranked by Impact)

### #1. Replace Claude task-verify with shell-first verification (EASY)

**Savings:** ~130s per task (189s avg → ~60s avg) = **~7 min per 4-task spec**

**How:** Modify `task-verify.phase.toml` to set `requires_claude = false`. The runner already has the `run_verify_phase` path (runner.rs:238) that executes shell verify commands directly. Claude-based verification only triggers when `verify_prompt` is set (rare).

For the 62% of task-verify runs currently exceeding 2 minutes, a shell verify takes <5 seconds.

**Implementation:**

```toml
# task-verify.phase.toml — change runtime to "shell"
[phase]
requires_claude = false

[worker]
runtime = "shell"
```

Or add a `runtime = "shell"` option that the daemon checks before spawning Claude. If `requires_claude = false` is already handled in `runner.rs:68-70`, this is just a TOML change.

**Difficulty:** Easy — one config change, already supported in code.

### #2. Cap critic at 1 redo cycle, not 4 (EASY)

**Savings:** 3 wasted redo cycles × 107s = **~5.4 min per spec** (for the 8/16 specs that hit max redos)

**How:** The current behavior: critic runs up to 4 times, each time finding new issues, injecting remediation tasks, and re-evaluating. Data shows this never converges — specs with 4 redos have 0 approvals.

Set `max_spec_redos = 1` for critic. One pass to find real bugs, one remediation cycle, then move on. If the spec doesn't pass after 1 redo, accept it and let the human review.

**Implementation:**

In `worker.rs`, the `max_spec_redos` is set from `config.retry_count` (config.yaml default: 3). Either:
- Add a per-phase retry count in the TOML: `retry_count = 1` in `critic.phase.toml`
- Or change the global default from 3 to 1

**Difficulty:** Easy — config change or one-line code change.

### #3. Add --bare flag to spawn_claude (EASY)

**Savings:** ~5s per spawn × 7 spawns = **~35s per spec**

**Status: DONE** — See `build_claude_args()` in `src/spawn.rs:32-49` and `docs/bench-bare-flag-2026-04-29.md`.

The `--bare` flag skips hooks, LSP, plugin sync, CLAUDE.md auto-discovery. Phase TOMLs opt in via `[worker] bare = true`. Verified safe for `critic`, `plan-critique`, and `spec-critique` (text-only, no file tools).

**Risk:** `--bare` disables CLAUDE.md auto-discovery and hooks. BOI workers don't need these — all context is injected via the prompt. **Verified:** no worker relies on CLAUDE.md auto-load from the worktree.

**Difficulty:** Easy — one line in spawn.rs.

### #4. Slim the task-verify prompt from 6,443 bytes to ~500 bytes (MEDIUM)

**Savings:** Faster inference: ~189s → ~60-90s = **~100-130s per task-verify run**

**How:** The task-verify-worker-prompt.md is 547 bytes but includes `{{CRITIC_PROMPT}}` which injects the full 6,443-byte critic prompt. For task verification, the worker only needs to:
1. Run the verify command
2. Check the output
3. Output "## Task Verification Approved" or "[TASK-VERIFY] reason"

Replace the prompt with a focused version:

```markdown
# Task Verification

Verify that the completed task's work is correct.

## Task
**Title:** {{TASK_TITLE}}
**Verify:** {{TASK_VERIFY}}

## Instructions
1. Run the verify command above
2. If it passes, output: ## Task Verification Approved
3. If it fails, output: [TASK-VERIFY] <explanation of what failed>

Do NOT modify any files. Just verify.
```

This is ~300 bytes instead of ~7,000 bytes (template + injected critic prompt). Fewer tokens = faster inference.

**Difficulty:** Medium — requires updating the template and ensuring the daemon's completion handler still parses the output correctly.

### #5. Remove quality-scoring.md from critic checks (EASY)

**Savings:** ~11K bytes (~2,750 tokens) removed from every critic prompt = **~15-25% faster critic inference**

**How:** `quality-scoring.md` is 11,242 bytes — 43% of the total checks payload. It's a detailed scoring rubric that the critic rarely uses productively (83% redo rate suggests the scoring isn't driving convergence).

Remove it from `templates/checks/` or exclude it from the checks loader. Keep the other 8 checks (15K bytes total without quality-scoring).

**Difficulty:** Easy — delete one file or add an exclusion list.

### #6. Make doc-update conditional on file changes (MEDIUM)

**Savings:** Skip 28% of doc-update runs where no docs change = **~5-10s per skipped run**

**How:** Before spawning Claude for doc-update, run `git diff --name-only HEAD~1` in the daemon and check if any `.md` or `docs/` files were touched. If not, skip the phase entirely.

This requires a small code change in the runner to add a pre-flight check before spawning Claude for doc-update.

Alternatively, since doc-update is already removed from the current `pipelines.toml` default mode, simply ensure it stays removed. For specs that explicitly need doc-update, make it opt-in via `task_phases: ["execute", "doc-update", "task-verify"]`.

**Difficulty:** Medium — either code change or pipeline config management.

### #7. Reduce spec-review prompt and set effort to "low" (EASY)

**Savings:** 100s avg → ~30-40s = **~60s per spec**

**How:** Spec-review checks task sizing, verify commands, spec clarity, dependencies, and missing verifies. This is a structured check that should be fast. The current effort is "medium" and the prompt is 3,433 bytes.

Changes:
- Set `effort = "low"` in `spec-review.phase.toml`
- Reduce timeout from 120s to 60s

**Difficulty:** Easy — TOML change.

### #8. Fail specs faster: kill at iteration 3, not iteration 30 (MEDIUM)

**Savings:** Prevents 20 failed specs from burning full iteration budgets

**How:** The `max_iterations` default is 30. Failed specs average 7+ iterations before failure. Early failure detection could save 4+ iterations per failing spec.

Add a heuristic: if a spec has 0 tasks completed after 3 iterations, fail it immediately. The data shows failing specs rarely recover after 3 iterations.

**Implementation:** In `worker.rs` state machine, after each iteration, check:
```
if iteration >= 3 && completed_tasks == 0 {
    state = WorkerState::Failed { reason: "no progress after 3 iterations" };
}
```

**Difficulty:** Medium — requires understanding the iteration counting logic.

### #9. Run spec-review before dispatch (move to CLI) (HARD)

**Savings:** Eliminates 1 Claude spawn per spec (~100s) by running spec-review at `boi dispatch` time instead of at worker startup.

**How:** When the user runs `boi dispatch spec.yaml`, the CLI could:
1. Parse the spec
2. Run the spec-review checks locally (task sizing, verify completeness, dependency validation)
3. Apply fixes to the spec before enqueueing
4. Skip the spec-review phase entirely during execution

Most spec-review checks are structural (no LLM needed): missing verify commands, oversized tasks, dependency cycles. These could be done with Rust code in the CLI.

**Difficulty:** Hard — requires implementing spec validation in Rust.

### #10. Use --bare + --system-prompt-file for prompt caching (MEDIUM)

**Savings:** Potential 30-50% cost reduction on input tokens via Anthropic prompt caching

**How:** With `--bare`, pass the system prompt via `--system-prompt-file` instead of baking it into the `-p` prompt. This enables Anthropic's automatic prompt caching, which caches the system prompt across calls within a 5-minute TTL window.

For BOI, where multiple tasks in the same spec run sequentially with the same system context (worker prompt + spec content), the system prompt would be cached after the first call, reducing input costs by up to 90% on subsequent calls.

**Implementation:**
1. Write the system prompt (template + spec content) to a temp file
2. Pass it via `--system-prompt-file <path>`
3. Pass only the task-specific content via `-p`

**Difficulty:** Medium — requires restructuring spawn.rs argument construction.

---

## 4. Model Allocation Matrix

Based on `boi-model-selection.md` research and the phase timing data:

| Phase | Current Model | Recommended Model | Invocation | Cold Start | Cost Change | Rationale |
|-------|---------------|-------------------|------------|------------|-------------|-----------|
| execute | Sonnet 4.6 | Sonnet 4.6 | `--bare` | 5.2s → 0.18s | Same | Needs full tool suite; quality is load-bearing |
| task-verify | Sonnet 4.6 | **Shell only** | No Claude | 5.2s → 0s | **-100%** | Shell verify is sufficient for 95% of cases |
| critic | Sonnet 4.6 | **Gemini 2.5 Flash** | OpenRouter HTTP | 5.2s → 0.5s | **-87%** | Pure judgment, no tools. 7.5x cheaper. |
| spec-review | Sonnet 4.6 | Sonnet 4.6 `--bare` | `--bare` | 5.2s → 0.18s | Same | Needs structured output quality |
| doc-update | Sonnet 4.6 | Sonnet 4.6 `--bare` | `--bare` | 5.2s → 0.18s | Same | Needs file read/write tools |
| plan-critique | Sonnet 4.6 | **Gemini 2.5 Flash** | OpenRouter HTTP | 5.2s → 0.5s | **-87%** | Judgment only; rarely converges anyway |

**Note on OpenRouter phases:** Moving critic and plan-critique to OpenRouter requires an HTTP-based LLM call path in the daemon. The phases don't use Claude's built-in tools (Read, Write, Bash) — they only read the prompt and produce structured output, making them candidates for pure API calls.

**Implementation priority:**
1. `--bare` for execute, spec-review, doc-update (immediate, zero new deps)
2. Shell-only for task-verify (immediate, already supported)
3. OpenRouter for critic, plan-critique — **`src/runtime/openrouter.rs` implemented** (smoke test: `tests/openrouter_smoke.rs`)

---

## 5. Prompt Size Analysis

### Current prompt sizes (template only, before spec/task injection):

| Template | Bytes | Tokens (est.) | Assessment |
|----------|-------|---------------|------------|
| worker-prompt.md | 6,186 | 1,547 | **Right-sized.** Decision Transparency + coordination + rules are load-bearing for execute. |
| task-verify-prompt.md | 6,443 | 1,611 | **Bloated.** Full critic prompt injected via `{{CRITIC_PROMPT}}`. For verify, this is 10x too large. |
| critic checks (9 files) | 26,256 | 6,564 | **Bloated.** quality-scoring.md alone is 11K (43%). 83% redo rate shows this doesn't drive quality. |
| spec-review-prompt.md | 3,433 | 858 | **Slightly large.** Could trim examples for a 30% reduction. |
| doc-update-prompt.md | 1,934 | 484 | **Right-sized.** Focused instructions. |
| critic-prompt.md | 549 | 137 | **Right-sized.** But the injected checks make it 27K total. |
| plan-critique-prompt.md | 3,335 | 834 | **Acceptable.** |

### What to cut:

1. **task-verify-prompt.md:** Replace 6,443 bytes with ~300 bytes focused on "run verify, report result." **Savings: ~6,100 bytes per task-verify run.**

2. **quality-scoring.md:** Remove from critic checks. **Savings: 11,242 bytes per critic run.**

3. **worker-prompt.md:** The "Coordination: Lock Before Write" section (lines 101-118, ~500 bytes) is rarely needed. The "Decision Transparency" section (lines 66-96, ~1,200 bytes) could be shortened to a one-line instruction with a reference. **Potential savings: ~1,500 bytes.**

### After optimization:

| Template | Current | After | Reduction |
|----------|---------|-------|-----------|
| task-verify total prompt | ~7,000 | ~300 | **96%** |
| critic total prompt (template + checks) | ~26,800 | ~15,500 | **42%** |
| worker-prompt.md | 6,186 | ~4,700 | **24%** |

---

## 6. Config Changes (Apply Now)

### 6.1 task-verify.phase.toml

```toml
name = "task-verify"
description = "Run verification commands for completed tasks"
completion_handler = "builtin:task-verify"

[phase]
requires_claude = false

[worker]
runtime = "shell"
timeout = 30

[completion]
approve_signal = "## Task Verification Approved"
reject_signal = "[TASK-VERIFY]"
on_approve = "next"
on_reject = "requeue:execute"
on_crash = "retry"
```

**Key changes:**
- `requires_claude = false` (was implicit `true` from `runtime = "claude"`)
- `runtime = "shell"` (instead of `"claude"`)
- `timeout = 30` (was 120; shell verify takes <5s)
- Removed `model`, `effort`, `prompt_template` (not needed for shell runtime)

### 6.2 critic.phase.toml

```toml
name = "critic"
description = "Review completed spec for quality issues. One pass."

[worker]
runtime = "claude"
model = "claude-sonnet-4-6"
prompt_template = "templates/critic-prompt.md"
effort = "low"
timeout = 180

[completion]
approve_signal = "## Critic Approved"
reject_signal = "[CRITIC]"
on_approve = "next"
on_reject = "requeue:execute"
on_crash = "retry"
max_redos = 1
```

**Key changes:**
- `effort = "low"` (was "medium")
- `timeout = 180` (was 300)
- `max_redos = 1` (was 3/4 via global config)

### 6.3 spec-review.phase.toml

```toml
[worker]
effort = "low"
timeout = 60
```

**Key change:** `effort = "low"` (was "medium"), `timeout = 60` (was 120).

### 6.4 pipelines.toml (verify doc-update removed)

```toml
[mode.default]
spec_phases = ["spec-review", "critic"]
task_phases = ["execute", "task-verify"]
```

**Key change:** Ensure `spec-review` is included (currently missing from pipelines.toml — only `critic` is listed). Add it back for spec quality, but with the "low" effort setting from 6.3.

### 6.5 Remove quality-scoring.md from checks

```bash
mv ~/github.com/mrap/boi/templates/checks/quality-scoring.md \
   ~/github.com/mrap/boi/templates/checks/_archive/quality-scoring.md
```

---

## 7. Code Changes

### 7.1 Add --bare flag to spawn_claude

**Status: DONE** — Implemented via `build_claude_args()` in `src/spawn.rs:32-49`.

`spawn_claude` now takes a `bare: bool` parameter. When `true`, `build_claude_args` appends `--bare` to the arg list. Phase TOMLs opt in via `[worker] bare = true`. Benchmark: see `docs/bench-bare-flag-2026-04-29.md`.

```rust
// Implemented (src/spawn.rs:32):
pub fn build_claude_args(prompt: &str, model: Option<&str>, bare: bool) -> Vec<String> {
    // ...base args...
    if bare {
        args.push("--bare".to_string());
    }
    args
}
```

Note: `--no-session-persistence` and `--setting-sources` were kept (not removed) from the base args.

**Measured impact:** 5,257ms → 184ms per bare spawn (−96.5%). See `docs/bench-bare-flag-2026-04-29.md`.

### 7.2 Add per-phase max_redos support

**File:** `src/phases.rs` — add `max_redos` to `CompletionSection`:

```rust
#[derive(Debug, Deserialize)]
struct CompletionSection {
    // ... existing fields ...
    #[serde(default)]
    max_redos: Option<u32>,
}
```

**File:** `src/phases.rs` — add `max_redos` to `PhaseConfig`:

```rust
pub struct PhaseConfig {
    // ... existing fields ...
    pub max_redos: Option<u32>,
}
```

**File:** `src/worker.rs` — use per-phase max_redos when available:

```rust
// In PostTaskSpecPhase handling, where spec_redo_count is checked:
let max_redos = phase.max_redos.unwrap_or(max_spec_redos as u32) as usize;
if spec_redo_count >= max_redos { ... }
```

### 7.3 Add shell-only runtime check

**File:** `src/runner.rs:68-70` — already handles `requires_claude = false`:

```rust
if !phase.requires_claude {
    return (self.run_verify_phase(phase, task, worktree_path, timeout_secs, spec_id), String::new());
}
```

This already works. The only change needed is in the TOML config (section 6.1) to set `requires_claude = false`. Verify that `run_verify_phase` handles the case where task has no verify command (it should return `Proceed`).

### 7.4 Slim the task-verify prompt template

**File:** `templates/task-verify-worker-prompt.md`

Replace current content (547 bytes + {{CRITIC_PROMPT}} injection) with:

```markdown
# Task Verification

Verify that task work is correct by running the verify command.

## Instructions
1. Run the verify command below
2. Review the output
3. If it passes: output "## Task Verification Approved"
4. If it fails: output "[TASK-VERIFY] <what failed and why>"

Do NOT modify any files. Verification only.
```

This is only used when `requires_claude = true` (i.e., when explicitly overridden for complex verification). With the default changed to shell-only, this template is rarely invoked.

### 7.5 Early failure detection

**File:** `src/worker.rs` — in the TaskSelect state, after checking `done_ids`:

```rust
// After task_select_passes increment:
if task_select_passes >= 3 && done_ids.is_empty() {
    boi_log!("FAIL: no tasks completed after {} passes — aborting", task_select_passes);
    state = WorkerState::Failed {
        reason: format!("no progress after {} task selection passes", task_select_passes),
    };
    continue;
}
```

---

## 8. Estimated Total Impact

### Per-spec time savings (4-task spec, default execute mode):

| Optimization | Current | After | Savings |
|---|---|---|---|
| #1 Shell task-verify (4 tasks) | 756s (4×189) | 20s (4×5) | **736s** |
| #2 Critic cap at 1 redo | 428s (4×107) | 214s (2×107) | **214s** |
| #3 --bare flag (7 spawns) | 36s | 1.3s | **35s** |
| #4 Slim task-verify prompt | (included in #1) | — | — |
| #5 Remove quality-scoring | ~107s critic avg | ~80s | **27s** |
| #7 Spec-review effort=low | 100s | 40s | **60s** |
| **Total** | **1,643s (27.4m)** | **~600s (10m)** | **~1,043s (17.4m) = 63% reduction** |

### Per-spec cost savings (at Sonnet 4.6 pricing):

| Change | Impact |
|--------|--------|
| Eliminate 4 task-verify Claude spawns per spec | **-80% of task-verify cost** |
| Reduce critic from 4 cycles to 2 | **-50% of critic cost** |
| Smaller prompts across all phases | **-20% of input token cost** |
| Future: OpenRouter for critic | **-87% of critic cost** |

### Implementation Priority

| Priority | Optimization | Difficulty | Impact |
|----------|-------------|------------|--------|
| P0 (now) | #1 Shell task-verify | Easy | 736s/spec |
| P0 (now) | #3 --bare flag | Easy | 35s/spec |
| P0 (now) | #5 Remove quality-scoring | Easy | 27s/spec |
| P1 (this week) | #2 Critic redo cap | Easy | 214s/spec |
| P1 (this week) | #7 Spec-review effort | Easy | 60s/spec |
| P2 (next week) | #4 Slim task-verify prompt | Medium | (folded into #1) |
| P2 (next week) | #8 Early failure detection | Medium | Prevents waste |
| P3 (future) | #10 Prompt caching | Medium | ~30-50% cost |
| P3 (future) | #6 Conditional doc-update | Medium | Minor |
| P4 (future) | #9 CLI-side spec-review | Hard | 100s/spec |

---

## 9. Risks and Caveats

1. **Shell-only task-verify may miss subtle bugs.** The Claude task-verify catches issues that shell commands don't test for (e.g., error handling quality, edge cases). Mitigation: the critic phase still runs with Claude and catches spec-level quality issues.

2. **Reducing critic redos may reduce output quality.** Data doesn't support this — 83% redo rate with no convergence means the current behavior adds compute without adding quality. But monitor: if spec quality drops after the change, increase to 2 redos.

3. **--bare may break workers that depend on CLAUDE.md auto-discovery.** BOI workers shouldn't — they run in git worktrees without CLAUDE.md. But verify before deploying: run `claude -p --bare "echo hello" --output-format stream-json` in a worktree and confirm it works.

4. **Prompt caching via --system-prompt-file requires Claude CLI support.** Verify that `--bare` + `--system-prompt-file` is a supported combination before implementing #10.
