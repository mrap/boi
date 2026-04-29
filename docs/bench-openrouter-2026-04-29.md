# OpenRouter Runtime — Smoke Bench (2026-04-29)

Recorded from: `OPENROUTER_API_KEY=<key> cargo test --test openrouter_smoke -- --nocapture`

## Test

File: `tests/openrouter_smoke.rs`

Sends `"Reply with exactly one word: hello"` to `gemini-flash`
(`google/gemini-2.0-flash-001`) with a 30 s timeout. Asserts:

- `text` is non-empty
- `input_tokens > 0`
- `output_tokens > 0`
- `duration_ms > 0`

## Results

> Update this section by running the smoke test with a live key:
> ```
> OPENROUTER_API_KEY=sk-... cargo test --test openrouter_smoke -- --nocapture 2>&1
> ```

```
model:          gemini-flash
prompt:         "Reply with exactly one word: hello"
response:       <pending live run>
input_tokens:   <pending>
output_tokens:  <pending>
cost_usd:       <pending>
duration_ms:    <pending>
wall_ms:        <pending>
```

## Context

Phase 2 of the BOI runtime architecture decision (2026-04-29). Non-tool phases
(spec-critique, plan-critique, critic, evaluate) will route through OpenRouter
instead of the Claude CLI, saving ~6 s per phase cold-start and 5–10× cost
using Haiku/Flash for judgment phases. This smoke test validates the
`OpenRouterRuntime` implementation end-to-end against the live API.
