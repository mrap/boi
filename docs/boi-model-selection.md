# BOI Model Selection — Research & Recommendations

**Date:** 2026-04-29
**Context:** Cold start analysis for BOI agent workers. Companion doc to `llm-cold-start-benchmarks.md`.

---

## TL;DR

The Claude CLI adds ~12–18s of startup overhead per invocation. The underlying model TTFT is ~1–1.5s regardless of model size. Three actionable fixes:

1. **Use `--bare`** — reduces CLI cold start from 5.2s to 183ms (96.5% reduction; see benchmark doc)
2. **Use direct API** (`messages.create`) — eliminates CLI scaffolding entirely, ~1–3s total per call
3. **Use OpenRouter for judgment phases** — Gemini 2.5 Flash at 10x lower cost, ~0.5s TTFT

---

## Section 1: Direct API vs Claude CLI

### The Core Finding

Cold start is entirely CLI scaffolding, not model loading. Two paths to eliminate it:

| Invocation Method | Cold Start | TTFT | Per-call Total | Notes |
|---|---|---|---|---|
| `claude -p` (default) | ~5,200ms | ~1,000ms | ~6,200ms | Hooks, plugin sync, CLAUDE.md discovery |
| `claude -p --bare` | ~0ms | ~1,000ms | ~1,000ms | Skips all scaffolding |
| Direct API (`messages.create`) | 0ms | ~1,000–1,500ms | ~1,000–1,500ms | No CLI, no subprocess |
| `claude -p` inside project (hooks) | ~12,000–18,000ms | ~1,000ms | ~13,000–19,000ms | Hooks add Node.js spawns |

### Direct API Approach

A minimal `messages.create` call in Python/curl:

```python
import anthropic, time
client = anthropic.Anthropic()
start = time.time()
msg = client.messages.create(
    model="claude-sonnet-4-6",
    max_tokens=10,
    messages=[{"role": "user", "content": "echo hello"}]
)
print(f"Total: {time.time() - start:.2f}s")  # ~1.0–1.5s
```

**Verdict:** Direct API eliminates all CLI startup overhead. TTFT of 1–1.5s is the hard floor (network + model inference). For BOI workers making many short calls, this is the correct baseline to compare against.

### Direct API for BOI: Requirements Gap

BOI workers rely heavily on built-in tools (Read, Write, Edit, Bash, Glob, Grep, etc.). A direct `messages.create` call doesn't get these — you'd need to implement tool routing yourself. The options:

1. **Claude Agent SDK** — provides tool loop management (see Section 2)
2. **Managed Agents API** — Anthropic hosts the agent loop (see Section 2)
3. **Custom tool executor** — implement Read/Write/Bash shims calling the API directly

---

## Section 2: Agent SDK and Persistent Sessions

### Claude Agent SDK

The Agent SDK (`@anthropic-ai/claude-agent-sdk` / Python equivalent) runs the Claude agent loop inside your own process with access to Claude Code's built-in tools.

**Characteristics:**
- Session state persisted as JSONL in `~/.claude/projects/` — sessions are resumable/forkable via `session_id`
- Full built-in tool support (Read, Write, Edit, Bash, etc.)
- Supports hooks, subagents, MCP servers

**Critical limitation for BOI:** Each `query()` call currently spawns a new subprocess with ~12s overhead. This is a known open issue (no daemon/hot-process reuse). Per-call latency is identical to `claude -p` — the SDK doesn't solve the spawning problem.

**Verdict:** Not a viable cold start fix in current form. Session persistence is useful for long-running workflows, but per-call overhead makes it unsuitable for BOI's high-frequency task pattern.

### Managed Agents API (Anthropic-hosted)

A separate REST API (`platform.claude.com/docs/en/managed-agents`) where Anthropic runs the agent loop.

| Feature | Managed Agents | BOI CLI today |
|---|---|---|
| Agent loop runs in | Anthropic infra | Worker subprocess |
| Session state | Anthropic server | None (fresh spawn) |
| Tool execution | Managed sandbox | BOI worker host |
| Cold start | None (no spawn) | 5–18s |
| Access to local files | No | Yes |

**Key gap:** Managed Agents run in Anthropic's sandbox, not on the BOI host machine. BOI workers need to read local spec files, run shell commands in the project directory, and access the local filesystem. Managed Agents cannot do this.

**Verdict:** Not applicable for BOI workers that need local filesystem access. Viable for hosted verification tasks (read-only judgment calls) but not for execute/decompose phases.

### Warm Session Pool

Pre-spawn a pool of `claude -p --bare` processes before tasks arrive, then route task prompts to a waiting process via stdin/stdout.

**Architecture:**
```
Daemon starts → spawns N warm workers → each waits on stdin
Task arrives  → daemon picks idle warm worker → writes prompt to stdin
Worker replies → daemon reads stdout → routes result → marks worker idle
```

**Estimated cold start:** ~0ms (process already warm, TTFT only ~183ms with `--bare`)

**Implementation risks:**
- Claude CLI may not support bidirectional stdin/stdout in `--bare` mode reliably
- Session contamination between tasks (context must be fully explicit per task)
- Need to detect crashed workers and respawn
- Concurrency limit: pool size = max parallelism

**Verdict:** Highest potential upside (near-zero cold start), but requires investigation into CLI's stdin/stdout interface in `--bare` mode. The `--output-format stream-json` flag BOI already uses is compatible. Worth prototyping.

---

## Section 3: OpenRouter Models

OpenRouter provides a unified OpenAI-compatible API (`/v1/chat/completions`) for all models below. No subprocess spawning — pure HTTP calls with ~1–3s TTFT.

### Model Comparison Table

| Model | OR ID | Input $/1M | Output $/1M | TTFT | Output Speed | Context | Tool Use |
|---|---|---|---|---|---|---|---|
| **Claude Sonnet 4.6** (baseline) | `anthropic/claude-sonnet-4.6` | $3.00 | $15.00 | ~1.0–1.5s | ~44 t/s | 1M | Yes |
| **Gemini 2.5 Flash** | `google/gemini-2.5-flash` | $0.30 | $2.50 | ~0.5–0.7s | ~200–221 t/s | 1M | Yes |
| **Gemini 2.5 Flash Lite** | `google/gemini-2.5-flash-lite` | $0.10 | $0.40 | ~0.5s est. | ~200 t/s est. | 1M | Yes |
| **Gemini 2.5 Pro** | `google/gemini-2.5-pro` | $1.25 | $10.00 | ~21–25s | ~130 t/s | 1M | Yes |
| **DeepSeek V3.2** | `deepseek/deepseek-v3.2` | $0.25 | $0.38 | ~0.8–1.9s | ~35–199 t/s | 131K | Yes |
| **DeepSeek V3 0324** | `deepseek/deepseek-chat-v3-0324` | $0.20 | $0.77 | ~1.0s est. | — | 163K | Yes |
| **DeepSeek R1 0528** | `deepseek/deepseek-r1-0528` | $0.50 | $2.15 | ~0.6–1.8s | ~35–182 t/s | 163K | No |
| **Qwen3 Coder Flash** | `qwen/qwen3-coder-flash` | $0.20 | $0.98 | — | — | 1M | Yes |
| **Qwen3 Coder 480B** | `qwen/qwen3-coder` | $0.22 | $1.80 | ~2.4s est. | ~96 t/s | 262K | Yes |
| **Qwen3 Coder Plus** | `qwen/qwen3-coder-plus` | $0.65 | $3.25 | — | — | 1M | Yes |
| **Grok 3** | `x-ai/grok-3` | $3.00 | $15.00 | ~1.65s | ~72 t/s | 131K | Yes |
| **Grok 3 Mini** | `x-ai/grok-3-mini` | $0.30 | $0.50 | ~0.54s | ~190 t/s | 131K | Yes |

### Notes

**Gemini 2.5 Pro:** The 21–25s TTFT is not network latency — it's the model's internal extended thinking (reasoning tokens before first output). For interactive agentic loops, this makes it worse than Claude Sonnet 4.6 despite higher quality ceiling. Reserve for complex single-shot planning steps only.

**DeepSeek V3.2:** Latency is strongly provider-dependent. Routing via Google Vertex (which OpenRouter can do automatically) yields ~0.76s TTFT / 199 t/s. Native DeepSeek endpoint gives ~1.88s / 34 t/s. OpenRouter's provider routing parameter `provider.order` controls this.

**DeepSeek R1:** Reasoning model — tool use is technically supported but not recommended for rapid tool-call loops. The chain-of-thought format doesn't compose well with agentic back-and-forth.

**Qwen3 Coder variants (thinking mode):** When using Qwen3 models, specify `thinking_mode: false` (or instruct variant) for tool-use tasks. Thinking mode adds 5–20s overhead similar to Gemini Pro.

### Cost Comparison

For a typical BOI worker task (10K input tokens, 1K output tokens):

| Model | Input cost | Output cost | Total | vs Sonnet |
|---|---|---|---|---|
| Claude Sonnet 4.6 | $0.030 | $0.015 | **$0.045** | baseline |
| Gemini 2.5 Flash | $0.003 | $0.003 | **$0.006** | 7.5x cheaper |
| Gemini 2.5 Flash Lite | $0.001 | $0.0004 | **$0.0014** | 32x cheaper |
| DeepSeek V3.2 | $0.0025 | $0.00038 | **$0.0029** | 16x cheaper |
| Qwen3 Coder Flash | $0.002 | $0.00098 | **$0.003** | 15x cheaper |
| Grok 3 | $0.030 | $0.015 | **$0.045** | same |

### OpenRouter Tool Use

All models in the table above (except DeepSeek R1) support tool calling via OpenRouter's unified `/v1/chat/completions` endpoint using the standard OpenAI tools format. OpenRouter normalizes this across providers. The BOI worker's tool definitions (Read, Write, Bash, etc.) would need to be passed explicitly in the API call — unlike the Claude CLI, there are no built-in tools.

---

## Section 4: Recommended Models by Tier

### Tier 1: Full Agentic Work (needs tool use + code generation)

**Primary:** Claude Sonnet 4.6 (direct API or `--bare`)
**Budget:** Gemini 2.5 Flash via OpenRouter
**Rationale:** Both have strong tool use and <1.5s TTFT. Gemini 2.5 Flash is 7.5x cheaper with 30% faster TTFT.

### Tier 2: Judgment / Review Tasks (needs reasoning, minimal tool use)

**Primary:** Gemini 2.5 Flash or DeepSeek V3.2 via OpenRouter
**Quality ceiling:** Gemini 2.5 Pro (for complex plans — accept the 21s TTFT as it's a one-shot call)
**Rationale:** Judgment tasks (critic, plan-critique, code-review) make few tool calls and are latency-tolerant. Cost savings are large.

### Tier 3: Simple / Mechanical Tasks (classify, format, extract)

**Primary:** Gemini 2.5 Flash Lite or Grok 3 Mini
**Rationale:** 32x cheaper than Sonnet, ~0.5s TTFT. Decompose and evaluate phases can be mechanical enough to use lighter models.

---

## Section 5: Invocation Architecture Recommendation

For BOI to achieve minimal cold start across phases:

```
                    ┌─ execute ──────► Claude Sonnet 4.6, direct API
                    │                  (needs built-in tools; use SDK or custom shims)
BOI Daemon          │
     │              ├─ task-verify ──► Claude Sonnet 4.6 --bare OR Gemini 2.5 Flash
     └─ Routes ─────│                  (needs command execution; --bare fastest)
                    │
                    ├─ critic ────────► Gemini 2.5 Flash via OpenRouter
                    │                  (judgment only; 7.5x cheaper, faster TTFT)
                    │
                    ├─ plan-critique ► Gemini 2.5 Pro (accept 21s for quality)
                    │                  OR Sonnet 4.6 --bare
                    │
                    ├─ code-review ──► DeepSeek V3.2 via OpenRouter
                    │                  (16x cheaper; strong code reasoning)
                    │
                    ├─ decompose ────► Gemini 2.5 Flash via OpenRouter
                    │                  (structured output; lightweight)
                    │
                    └─ evaluate ─────► Gemini 2.5 Flash via OpenRouter
                                       (assessment; no tool use needed)
```

**Phase-to-model mapping is in the next section (t-3 output).**

---

## Sources

- OpenRouter Models API (`openrouter.ai/api/v1/models`), queried 2026-04-29
- Artificial Analysis speed/TTFT leaderboard (`artificialanalysis.ai`)
- BenchLM speed leaderboard (`benchlm.ai/llm-speed`)
- DeepInfra DeepSeek V3.2 benchmarks
- Anthropic Agent SDK docs (`code.claude.com/docs/en/agent-sdk/overview`)
- Anthropic Managed Agents docs (`platform.claude.com/docs/en/managed-agents/overview`)
- OpenRouter tool-calling model collection (`openrouter.ai/collections/tool-calling-models`)
- Benchmark data: `docs/llm-cold-start-benchmarks.md` (this repo)

---

## Section 6: Phase-to-Model Mapping

> Derived from benchmark data (`llm-cold-start-benchmarks.md`) and OpenRouter research (Section 3). All cold start estimates assume `--bare` for Claude CLI phases and direct HTTP for OpenRouter phases. Default CLI adds ~5.2s to every figure.

### Phase Mappings

**execute:** Claude Sonnet 4.6 via direct API or `claude --bare`
- **Reasoning:** Requires full tool suite (Read, Write, Bash, Edit, Glob). Highest complexity work — Sonnet quality is load-bearing. Direct API eliminates subprocess spawn overhead while preserving tool routing via the Agent SDK or custom shims.
- **Cold start:** ~183ms (direct API) vs ~5,200ms (current default CLI) — **−96%**
- **Cost/task:** $0.045 (10K in / 1K out). No reduction — quality and tool capability justify Sonnet pricing for primary execution work.

**task-verify:** Claude Sonnet 4.6 via `claude --bare`
- **Reasoning:** Must run shell commands and read filesystem output — requires local tool access. `--bare` skips hooks/plugins while retaining full tool access. Correctness matters here; Sonnet quality is appropriate.
- **Cold start:** ~183ms vs ~5,200ms current — **−96%**
- **Cost/task:** $0.045. No reduction — verification failures are expensive; this is not a place to cut quality.

**critic:** Gemini 2.5 Flash via OpenRouter (`google/gemini-2.5-flash`)
- **Reasoning:** Pure judgment, no tool calls required. Read-only review of text/code. Flash has adequate reasoning quality for rating/critique tasks at a fraction of Sonnet's cost.
- **Cold start:** ~500ms (HTTP, no subprocess) vs ~5,200ms — **−90%**
- **Cost/task:** $0.006 vs $0.045 — **7.5× cheaper**

**plan-critique:** Gemini 2.5 Pro via OpenRouter (`google/gemini-2.5-pro`) — with Sonnet fallback
- **Reasoning:** Plan critique requires the deepest reasoning of all judgment phases. Gemini 2.5 Pro's quality ceiling justifies its cost for this single-shot call. Accept the 21–25s internal reasoning TTFT since this is not a tight loop. If latency is unacceptable for the pipeline, fall back to Sonnet 4.6 `--bare` (~183ms).
- **Cold start:** ~21,000–25,000ms (internal reasoning tokens, not network). No subprocess overhead, but TTFT is the model itself.
- **Cost/task:** $0.013 (10K in / 1K out at $1.25/$10.00 per 1M). 3.5× cheaper than Sonnet despite higher quality.

**code-review:** DeepSeek V3.2 via OpenRouter (`deepseek/deepseek-v3.2`, routed via Google Vertex)
- **Reasoning:** Strong code comprehension, 16× cheaper than Sonnet. No tool use needed for review-only output. Pin `provider.order: ["Google Vertex"]` for consistent 0.76s TTFT; without pinning, native DeepSeek endpoint yields 1.9s.
- **Cold start:** ~760ms (HTTP via Vertex) vs ~5,200ms — **−85%**
- **Cost/task:** $0.003 vs $0.045 — **16× cheaper**
- **Context limit:** 131K tokens. For multi-file reviews exceeding this, substitute Gemini 2.5 Flash (1M context, 7.5× cheaper than Sonnet).

**decompose:** Gemini 2.5 Flash via OpenRouter (`google/gemini-2.5-flash`)
- **Reasoning:** Structured JSON output from natural language specs. Mechanical task that benefits from fast TTFT and 1M context window. Flash quality is well above the threshold for task decomposition.
- **Cold start:** ~500ms vs ~5,200ms — **−90%**
- **Cost/task:** $0.006 vs $0.045 — **7.5× cheaper**

**evaluate:** Gemini 2.5 Flash via OpenRouter (`google/gemini-2.5-flash`)
- **Reasoning:** Assessing experiment results is read-only scoring/classification. No tool use, no code generation. Flash is ideal: fast TTFT, cheap, accurate at structured assessment tasks.
- **Cold start:** ~500ms vs ~5,200ms — **−90%**
- **Cost/task:** $0.006 vs $0.045 — **7.5× cheaper**

---

### Summary Table

| Phase | Model | Invocation | Cold Start | Cost/task | vs Current |
|-------|-------|------------|------------|-----------|-----------|
| execute | Claude Sonnet 4.6 | Direct API / `--bare` | ~183ms | $0.045 | −96% latency |
| task-verify | Claude Sonnet 4.6 | `--bare` | ~183ms | $0.045 | −96% latency |
| critic | Gemini 2.5 Flash | OpenRouter HTTP | ~500ms | $0.006 | −90% latency, −87% cost |
| plan-critique | Gemini 2.5 Pro | OpenRouter HTTP | ~21,000ms* | $0.013 | −71% cost |
| code-review | DeepSeek V3.2 | OpenRouter HTTP (Vertex) | ~760ms | $0.003 | −85% latency, −93% cost |
| decompose | Gemini 2.5 Flash | OpenRouter HTTP | ~500ms | $0.006 | −90% latency, −87% cost |
| evaluate | Gemini 2.5 Flash | OpenRouter HTTP | ~500ms | $0.006 | −90% latency, −87% cost |

\*Gemini 2.5 Pro TTFT reflects internal reasoning tokens, not subprocess spawn. Sonnet 4.6 `--bare` (~183ms) is the recommended fallback when plan-critique latency is pipeline-critical.

---

### Implementation Priority

1. **`--bare` for execute + task-verify** — Immediate, zero new dependencies. Largest cold start wins for the two highest-frequency phases. Expected: 5.2s → 183ms per call.
2. **Gemini 2.5 Flash for critic, decompose, evaluate** — Medium priority. Requires OpenRouter API key and HTTP client in the BOI daemon. High cost and latency savings across the most numerous judgment phases.
3. **DeepSeek V3.2 for code-review** — Medium. Same infrastructure as #2; swap model ID + pin Google Vertex routing.
4. **Gemini 2.5 Pro for plan-critique** — Low priority. Single-shot, infrequent. Accept 21s TTFT or use Sonnet `--bare` fallback.
5. **Warm session pool** — Future. Pre-spawn `--bare` processes; eliminates the remaining 183ms floor entirely.

---

### Constraints & Caveats

- **OpenRouter phases have no built-in tools.** The BOI daemon must pass tool definitions in the API request and handle the tool-execution loop explicitly. This is non-trivial — scope it carefully before migrating execute or task-verify off the Claude CLI.
- **plan-critique TTFT is untunable.** Gemini 2.5 Pro's 21–25s is internal reasoning, not network delay. Streaming does not reduce perceived latency — the first token arrives after ~21s. Use Sonnet 4.6 `--bare` when plan-critique sits on the critical path.
- **DeepSeek V3.2 provider pinning.** Without `provider.order: ["Google Vertex"]`, OpenRouter may route to the native DeepSeek endpoint (1.9s TTFT, 34 t/s) vs Vertex (0.76s TTFT, 199 t/s). Pin explicitly.
- **DeepSeek V3.2 context limit.** 131K tokens covers single-file code review. For multi-file or large-repo reviews, substitute Gemini 2.5 Flash (1M context).
- **Qwen3 Coder thinking mode.** If Qwen3 models are evaluated later, disable thinking mode (`thinking_mode: false`) for all tool-use phases — thinking mode adds 5–20s overhead.
