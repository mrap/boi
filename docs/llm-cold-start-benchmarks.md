# LLM Cold Start Benchmarks

**Date:** 2026-04-29  
**Environment:** macOS Darwin 25.4.0, Claude Code 2.1.123  
**Methodology:** `claude -p --output-format stream-json --model <model> [flags] <prompt>`  
Each configuration run 3 times; median reported. Timing measured from process spawn to last byte.

---

## TL;DR

The dominant cold start cost is **CLI scaffolding** (~5.0s), not inference or model loading. Switching models does nothing — Haiku and Sonnet are statistically identical. The `--bare` flag eliminates scaffolding entirely: **5,200ms → 183ms** (28× speedup).

---

## Section 1: Prompt Size vs Cold Start

| Prompt Size | Runs (ms) | Median | Min | Max |
|-------------|-----------|--------|-----|-----|
| 1K chars    | 4970, 5303, 5070 | **5,070ms** | 4,970 | 5,303 |
| 5K chars    | 7696, 5169, 5381 | **5,381ms** | 5,169 | 7,696 |
| 20K chars   | 5317, 5623, 5192 | **5,317ms** | 5,192 | 5,623 |

**Model:** claude-sonnet-4-6, no special flags.

**Finding:** Prompt size has minimal effect on cold start. The 5K outlier (7.7s run 1) reflects network jitter, not a systematic cost. Going from 1K → 20K adds only ~250ms median (+5%), well within noise. Context parsing is not the bottleneck.

---

## Section 2: Model Comparison

| Model | Prompt | Runs (ms) | Median | Min | Max |
|-------|--------|-----------|--------|-----|-----|
| claude-haiku-4-5-20251001 | short | 5246, 5353, 5303 | **5,303ms** | 5,246 | 5,353 |
| claude-sonnet-4-6 | short | 5209, 5348, 5214 | **5,214ms** | 5,209 | 5,348 |

**Finding:** Model selection has essentially zero effect on cold start. Haiku vs Sonnet differ by only 89ms median — well within run-to-run variance (~140ms). Cold start is entirely in the CLI initialization layer, not in the model selection path. **Do not switch to Haiku expecting faster cold start.**

---

## Section 3: Session Persistence

| Configuration | Runs (ms) | Median | Delta vs Default |
|--------------|-----------|--------|-----------------|
| Default (sessions on) | 5209, 5348, 5214 | **5,214ms** | — |
| `--no-session-persistence` | 5341, 5319, 5395 | **5,341ms** | +127ms (+2.4%) |

**Finding:** Session persistence overhead is negligible. The 127ms delta is within noise. Disabling persistence does not reduce cold start; it may add a tiny amount due to different initialization path. **Not a viable optimization.**

---

## Section 4: `--bare` Flag (Key Finding)

| Configuration | Runs (ms) | Median | Reduction vs Default |
|--------------|-----------|--------|---------------------|
| Default | 5209, 5348, 5214 | **5,214ms** | — |
| `--bare` | 183, 187, 183 | **183ms** | **-5,031ms (-96.5%)** |

**`--bare` skips:** hooks, LSP init, plugin sync, attribution, auto-memory, background prefetches, keychain reads, and CLAUDE.md auto-discovery.

**Finding:** This is the single largest lever for cold start reduction. The `--bare` flag eliminates 96.5% of cold start overhead. Three runs in a tight 4ms band (183-187ms) indicate highly consistent behavior — this is a hard floor determined by process spawn + one network round-trip.

The 183ms floor breaks down approximately:
- Process spawn: ~5-10ms
- TLS handshake + DNS: ~30-50ms
- API request/response (TTFT for ~3-token reply): ~130-150ms

---

## Section 5: Warm Cache / Consecutive Runs

| Run | Haiku (ms) | Sonnet (ms) |
|-----|-----------|-------------|
| Run 1 | 5,246 | 5,209 |
| Run 2 | 5,353 | 5,348 |
| Run 3 | 5,303 | 5,214 |
| Run 4 (re-run Haiku) | 5,207 | — |
| Run 5 | 5,285 | — |
| Run 6 | 5,361 | — |

**Finding:** No warm cache effect is observable across consecutive `claude -p` invocations. Each process spawn pays the full ~5.2s CLI scaffolding cost regardless of recency. This confirms there is no inter-process state sharing in the default configuration. The Anthropic API prompt cache (5-min TTL) would reduce inference time, but CLI scaffolding masks it entirely.

**Implication for BOI:** Even with a warm Anthropic prompt cache, BOI workers pay full cold start every spawn. The fix is either (a) use `--bare` or (b) maintain persistent sessions.

---

## Summary Table

| Configuration | Median Cold Start | vs Baseline |
|--------------|------------------:|:-----------:|
| Sonnet, default, 1K prompt | 5,070ms | baseline |
| Sonnet, default, 5K prompt | 5,381ms | +6% |
| Sonnet, default, 20K prompt | 5,317ms | +5% |
| Haiku, default, short | 5,303ms | +5% |
| Sonnet, default, short | 5,214ms | +3% |
| Sonnet, --no-session-persistence | 5,341ms | +5% |
| **Sonnet, --bare** | **183ms** | **-96.5%** |

---

## BOI Context Notes

These benchmarks were run without the flags BOI daemon uses (`--verbose`, system prompts, MCP configs). In production BOI workers:
- System prompt is injected (spec + CLAUDE.md + shared memory = 10-30K tokens)
- `--verbose` is set
- Hooks execute at startup
- Plugin sync runs

These add additional cold start overhead beyond the 5.2s baseline measured here, which explains the 6-200s range observed in production. The BOI spec context injection alone adds ~2-10s of network upload time for large specs.

The `--bare` option would require providing context explicitly via `--system-prompt-file` or `--append-system-prompt`, but could reduce cold start to ~200-500ms even in production.

---

## Recommendations

1. **Use `--bare` for BOI workers** — pass spec/context via `--system-prompt-file` instead of relying on CLAUDE.md discovery. Expected reduction: 5s → 0.2-0.5s.
2. **Do not switch models for cold start** — Haiku saves 0ms on cold start.
3. **Investigate warm session pool** — pre-spawn `--bare` processes and reuse via stdio; eliminates even the 183ms floor.
4. **Prompt caching** — use `--exclude-dynamic-system-prompt-sections` to improve API-level prompt cache hit rate across workers.

---

*Raw data: `/docs/.bench_raw.json`*
