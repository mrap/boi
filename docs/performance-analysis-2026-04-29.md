# BOI Performance Analysis

**Date:** 2026-04-29
**Data source:** `~/.boi/boi-rust.db` (phase_runs, specs, tasks tables)
**Period:** All data since Rust rewrite (2026-04-29 -- single-day dataset, 256 phase runs, 36 specs dispatched)

---

## 1. Executive Summary

- **Biggest time sink:** Critic-redo cycles consume 11.1% of all wall time (1.27 hours) with zero corrective value -- 50% of completed specs hit the 4-redo cap and auto-advance without the critic ever approving. The critic phase is gatekeeping work that already passed task-verify, adding latency without improving outcomes.
- **Biggest cost driver:** All 7 phase types run Sonnet 4.6 via full CLI spawn (~$0.045/run). Quality gate phases (critic, plan-critique, spec-review) account for 35% of runs but need no tool use -- switching to Gemini 2.5 Flash via OpenRouter would cut their cost by 87% (~$3.50/day savings at current volume).
- **Biggest quality gap:** `generate` mode has a 0% success rate (14/14 failed). Every generate-mode spec passes spec-review then dies at plan-critique. The plan-critique phase has `on_reject = "fail"` -- a single plan-critique rejection kills the entire spec with no retry. This is a broken pipeline, not a quality signal.

---

## 2. Data Tables

### 2a. Phase Timing Breakdown

| Phase | Runs | Avg (s) | P50 (s) | Min (s) | Max (s) | Total (h) | % of Time |
|-------|------|---------|---------|---------|---------|-----------|-----------|
| execute | 79 | 223.2 | 163.1 | 14.1 | 1,458.5 | 4.90 | 43.5% |
| task-verify | 62 | 220.2 | 173.6 | 8.0 | 1,037.2 | 3.79 | 33.7% |
| critic | 48 | 107.4 | 91.3 | 14.1 | 427.1 | 1.43 | 12.7% |
| spec-review | 28 | 69.6 | 45.2 | 14.0 | 234.8 | 0.54 | 4.8% |
| doc-update | 23 | 62.4 | 46.2 | 12.0 | 288.8 | 0.40 | 3.5% |
| plan-critique | 14 | 49.6 | 43.2 | 26.1 | 94.3 | 0.19 | 1.7% |
| **Total** | **256** | **159.5** | | | | **11.25** | **100%** |

### 2b. Execute Phase Duration Distribution

| Range | Count | % |
|-------|-------|---|
| < 30s | 6 | 7.6% |
| 30s -- 1m | 13 | 16.5% |
| 1m -- 2m | 12 | 15.2% |
| 2m -- 5m | 27 | 34.2% |
| 5m -- 10m | 16 | 20.3% |
| > 10m | 5 | 6.3% |

Median execute is 2m43s. The 2-5 minute bucket is the mode. Outliers above 10m (up to 24m) suggest tasks that should have been decomposed further.

### 2c. Task-Verify Duration Distribution

| Range | Count | % |
|-------|-------|---|
| < 30s | 6 | 9.7% |
| 30s -- 1m | 3 | 4.8% |
| 1m -- 2m | 9 | 14.5% |
| 2m -- 5m | 32 | 51.6% |
| 5m -- 10m | 9 | 14.5% |
| > 10m | 4 | 6.5% |

Task-verify runs nearly as long as execute (avg 220s vs 223s). Over half of verify runs take 2-5 minutes. This is expensive for a yes/no judgment call.

### 2d. Outcome Distribution

| Outcome | Count | % |
|---------|-------|---|
| proceed | 194 | 76.1% |
| redo | 40 | 15.7% |
| failed | 21 | 8.2% |

### 2e. Failure Rates by Phase

| Phase | Total | Proceed | Redo | Failed | Failure % |
|-------|-------|---------|------|--------|-----------|
| execute | 79 | 74 | 0 | 5 | 6.3% |
| task-verify | 62 | 60 | 0 | 2 | 3.2% |
| critic | 48 | 8 | 40 | 0 | 83.3% (redo) |
| spec-review | 28 | 28 | 0 | 0 | 0% |
| doc-update | 23 | 23 | 0 | 0 | 0% |
| plan-critique | 14 | 0 | 0 | 14 | 100% |

**Critical findings:**
- **Critic** has an 83% redo rate. Of 16 completed specs, 8 hit the 4-redo cap (max_spec_redos = retry_count = 3, so 4 attempts) and auto-advanced. The critic never approved them -- the system just gave up.
- **Plan-critique** has a 100% failure rate across all 14 generate-mode specs. It has never approved a single spec.
- **Spec-review** and **doc-update** have 0% failure rates -- they always proceed.

### 2f. Spec Success Rate by Mode

| Mode | Total | Completed | Failed | Success Rate |
|------|-------|-----------|--------|-------------|
| execute | 22 | 16 | 6 | 72.7% |
| generate | 14 | 0 | 14 | 0.0% |

### 2g. Critic Redo Pattern Per Spec

| Pattern | Specs | Outcome |
|---------|-------|---------|
| 0 redos, 1 proceed | 4 | Clean pass |
| 2 redos, 1 proceed | 4 | Eventually passed |
| 4 redos, 0 proceeds | 8 | Hit redo cap, auto-completed |

Half of completed specs have the critic run 4 times, find issues every time, and the spec completes anyway because the redo limit was exceeded. That's 32 critic invocations (~54 minutes, ~$1.44 estimated) that produced zero corrective action.

### 2h. Estimated Cost Breakdown (all Sonnet 4.6, ~$0.045/run)

| Phase | Runs | Est. Cost | % of Cost |
|-------|------|-----------|-----------|
| execute | 79 | $3.56 | 31.0% |
| task-verify | 63 | $2.84 | 24.7% |
| critic | 48 | $2.16 | 18.8% |
| spec-review | 28 | $1.26 | 11.0% |
| doc-update | 23 | $1.04 | 9.0% |
| plan-critique | 14 | $0.63 | 5.5% |
| **Total** | **255** | **$11.48** | **100%** |

Token-level data was not populated in `phase_runs` (all NULL), so costs are estimated at $0.045/run (10K input, 1K output at Sonnet rates). Actual costs vary by prompt size.

---

## 3. Top 5 Performance Initiatives

### Initiative 1: Fix the Critic Phase (highest impact)

**Problem:** 83% of critic runs produce `redo`, triggering re-execution of already-verified tasks. 50% of specs auto-complete by hitting the redo cap without the critic ever approving. This means the critic is either (a) too strict, (b) not seeing the actual work output, or (c) finding issues that don't prevent shipping.

**Evidence:**
- 40 redo cycles, 1.27 hours, 11.1% of total wall time
- 8/16 completed specs hit the 4-redo cap with zero approvals
- Critic-triggered re-execution burns additional execute + task-verify cycles (not counted above)

**Proposed fixes (pick one or layer):**
1. **Reduce critic to advisory.** Change `on_reject` from `requeue:execute` to `next` (or a new `log-and-proceed` action). Critic findings get logged but don't block completion. Review findings offline to calibrate.
2. **Make critic context-aware.** The critic prompt reviews "completed work" but may not see actual diff output from the execute phase. Inject `git diff` output or task artifacts into the critic prompt so it judges real output, not spec text.
3. **Lower max_spec_redos to 1.** If the first redo doesn't fix it, more won't either. Currently 4 attempts; cap at 2 (one redo cycle) to save 50% of redo cost.
4. **Skip critic for specs where all task-verifies passed.** If every task's verify command succeeded, the critic is double-checking verified work.

**Estimated savings:** 1.27 hours/day wall time, $1.44/day cost, faster spec completion by 8-20 minutes per spec.

### Initiative 2: Fix Generate Mode (0% success rate)

**Problem:** Every generate-mode spec fails at plan-critique. The pipeline is `spec-review -> plan-critique -> [tasks]`, and plan-critique has `on_reject = "fail"` -- one rejection kills the spec permanently. There is no retry or fix-and-resubmit path.

**Evidence:** 14/14 generate-mode specs failed. All passed spec-review, all failed plan-critique. Zero tasks were ever attempted in generate mode.

**Root cause:** Plan-critique checks for non-executable verifies, unbounded scope, and missing dependencies -- criteria that generate-mode specs inherently violate (they're open-ended by design). The gate was designed for execute-mode specs and never adapted for generate.

**Proposed fixes:**
1. **Skip plan-critique for generate mode.** Remove it from `pipelines.toml` `[mode.generate]` spec_phases. Generate mode is creative/exploratory -- the rigid plan-critique criteria don't apply.
2. **Make plan-critique advisory for generate mode.** Change to `on_reject = "next"` with findings logged, not blocking.
3. **Create a generate-specific critique phase** with criteria appropriate for generative work (scope bounded by token budget, exit conditions defined as iteration limits, etc.).

**Estimated savings:** Unblocks an entire class of work. 14 failed specs would have completed. No cost savings directly (they fail fast at ~1 min each), but the opportunity cost is large.

### Initiative 3: Use `--bare` for All CLI Phases (cold start elimination)

**Problem:** Every phase invocation pays ~5.2s CLI scaffolding overhead (hooks, plugin sync, CLAUDE.md discovery). With 256 runs, that's ~22 minutes of pure startup waste.

**Evidence:** From `llm-cold-start-benchmarks.md`: `--bare` reduces cold start from 5,200ms to 183ms (96.5% reduction). Prompt size and model selection have no effect on cold start -- it's all CLI scaffolding.

**Implementation:**
- Pass `--bare` flag to all `claude -p` invocations in `spawn.rs`
- Inject spec context via `--system-prompt-file` or `--append-system-prompt` instead of CLAUDE.md auto-discovery
- Verify that built-in tools (Read, Write, Bash, Edit) still work with `--bare`

**Estimated savings:** 22 minutes/day at current volume. Scales linearly with run count. Zero cost in dollars -- pure latency reduction.

### Initiative 4: Route Judgment Phases to Cheaper Models

**Problem:** All 7 phases use Sonnet 4.6 via Claude CLI. Quality gates (critic, plan-critique, spec-review) and doc-update need no tool use and could run on cheaper, faster models via OpenRouter HTTP calls.

**Evidence:** From `boi-model-selection.md`:
- Gemini 2.5 Flash: 7.5x cheaper ($0.006 vs $0.045/run), 0.5s TTFT
- DeepSeek V3.2: 16x cheaper ($0.003/run) for code review

**Phase-to-model mapping:**

| Phase | Current | Proposed | Cost Reduction | Latency Reduction |
|-------|---------|----------|---------------|-------------------|
| execute | Sonnet 4.6 CLI | Sonnet 4.6 `--bare` | 0% | -96% cold start |
| task-verify | Sonnet 4.6 CLI | Sonnet 4.6 `--bare` | 0% | -96% cold start |
| critic | Sonnet 4.6 CLI | Gemini 2.5 Flash (OpenRouter) | -87% | -90% |
| spec-review | Sonnet 4.6 CLI | Gemini 2.5 Flash (OpenRouter) | -87% | -90% |
| plan-critique | Sonnet 4.6 CLI | Gemini 2.5 Flash (OpenRouter) | -87% | -90% |
| doc-update | Sonnet 4.6 CLI | Gemini 2.5 Flash (OpenRouter) | -87% | -90% |

**Estimated savings:** $3.50/day on quality gate phases. Requires adding OpenRouter HTTP client to daemon and implementing tool-definition pass-through for any phase that needs tools.

**Implementation priority:** Start with critic (highest volume, no tool use needed). Then spec-review and doc-update. Keep execute and task-verify on Sonnet CLI until `--bare` is validated.

### Initiative 5: Cap Task-Verify Duration

**Problem:** Task-verify averages 220s -- nearly identical to execute (223s). Over half of runs take 2-5 minutes. A verification phase should be a quick yes/no judgment, not a full execution cycle.

**Evidence:**
- 62 task-verify runs, 3.79 hours total (33.7% of all time)
- P50 = 173s, max = 1,037s (17 minutes for a single verify)
- Only 2/62 verifies fail (3.2%) -- the vast majority pass, suggesting most verify time is overhead

**Proposed fixes:**
1. **Set `effort = "low"` on task-verify phase.** Currently inherits default. Low effort tells the model to be concise.
2. **Reduce task-verify timeout** from inherited default (likely 900s) to 120s. If verification can't be determined in 2 minutes, it's the wrong kind of verify command.
3. **Restructure verify prompts** to run the verify command first and short-circuit on pass. Currently the model may be analyzing code before running the verify command.
4. **For simple exit-code verifies** (e.g., `test -f output.md`), run the command directly in the daemon without spawning a Claude session. Reserve Claude-based verify for semantic checks.

**Estimated savings:** Cutting average task-verify from 220s to ~60s would save 2.75 hours/day at current volume.

---

## 4. Model Allocation Recommendation

Based on actual phase characteristics from telemetry:

| Phase | Needs Tools? | Needs Code Gen? | Quality Sensitivity | Recommended Model | Invocation |
|-------|-------------|----------------|--------------------|--------------------|------------|
| execute | Yes (Read/Write/Bash/Edit) | Yes | High | Claude Sonnet 4.6 | `claude --bare` |
| task-verify | Yes (Bash for verify cmd) | No | High | Claude Sonnet 4.6 | `claude --bare` |
| critic | No | No | Medium | Gemini 2.5 Flash | OpenRouter HTTP |
| spec-review | No | No | Medium | Gemini 2.5 Flash | OpenRouter HTTP |
| plan-critique | No | No | Medium | Gemini 2.5 Flash | OpenRouter HTTP |
| doc-update | Yes (Read/Edit) | Yes (docs) | Low | Claude Sonnet 4.6 | `claude --bare` |

**Phased rollout:**
1. **Week 1:** `--bare` for all Claude phases (zero new dependencies)
2. **Week 2:** OpenRouter for critic (most volume, pure judgment, no tools)
3. **Week 3:** OpenRouter for spec-review and plan-critique
4. **Week 4:** Evaluate quality of Flash-based judgments; expand or revert

---

## 5. Quick Wins (Config-Only Changes)

These require no code changes -- just config/template edits:

### 5a. Lower max_spec_redos to 1

**File:** `~/.boi/config.yaml`
**Change:** Add `retry_count: 1`
**Effect:** Critic gets one redo attempt. If it still rejects, spec completes. Saves 50% of current redo waste (20 fewer redo cycles, ~38 minutes).

### 5b. Remove plan-critique from generate pipeline

**File:** `phases/pipelines.toml`
**Change:** `[mode.generate]` spec_phases from `["plan-critique", "critic", "evaluate"]` to `["critic", "evaluate"]`
**Effect:** Unblocks generate mode entirely. 100% failure rate drops to whatever the critic rate is.

### 5c. Set effort = "low" on task-verify and doc-update

**File:** `phases/task-verify.phase.toml`, `phases/doc-update.phase.toml`
**Change:** Under `[worker]`, add or change `effort = "low"`
**Effect:** Model produces shorter responses, reducing inference time. Expected 20-40% duration reduction on these phases.

### 5d. Reduce task-verify timeout to 120s

**File:** `phases/task-verify.phase.toml`
**Change:** `timeout = 120`
**Effect:** Hard cap on runaway verify sessions. Current max observed is 17 minutes -- a 2-minute cap catches the tail.

### 5e. Populate token tracking in phase_runs

**File:** `src/worker.rs` (where `record_phase_run` is called)
**Change:** Parse Claude CLI `--output-format stream-json` output for `input_tokens` and `output_tokens` fields and pass them to `record_phase_run`.
**Effect:** Enables actual cost tracking instead of estimates. Required for data-driven model allocation decisions.

---

## Appendix: Waste Budget

| Waste Category | Hours/Day | % of Total | Fix |
|---------------|-----------|------------|-----|
| Critic redo cycles (no value add) | 1.27 | 11.1% | Initiative 1 |
| Generate mode failures (0% success) | 0.23 | 2.0% | Initiative 2 |
| CLI cold start overhead (~5.2s x 256) | 0.37 | 3.3% | Initiative 3 |
| Overlong task-verify (>2 min above target) | ~2.75 | ~24% | Initiative 5 |
| Quality gates on Sonnet (could be Flash) | -- | -- | Initiative 4 ($3.50/day) |
| **Total addressable waste** | **~4.6** | **~41%** | |

At current volume, 41% of BOI wall time is addressable waste. The top two fixes (critic reform + task-verify caps) alone would recover ~4 hours/day.
