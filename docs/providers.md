# BOI Provider Architecture

BOI dispatches every LLM phase through a unified `Provider` trait. All runtime selection,
validation, cost tracking, and error handling flow through this single abstraction.

---

## The Provider Trait

```rust
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn validate_config(&self, phase: &PhaseConfig) -> Result<(), ProviderError>;
    fn invoke(&self, ctx: InvocationContext) -> Result<RuntimeOutput, ProviderError>;
    fn cost_estimate(&self, ctx: &InvocationContext) -> Option<Decimal>;
    fn actual_cost(&self, response: &RuntimeOutput) -> Option<Decimal>;
}
```

**Contract:**

| Method | Responsibility |
|--------|---------------|
| `name()` | Returns the registry key (e.g. `"claude"`, `"openrouter"`). Must be unique, lowercase, stable. |
| `capabilities()` | Returns a `Capabilities` struct advertising what this provider can do. |
| `validate_config()` | Checks env vars, credentials, and phase-level compatibility. Returns `ProviderError` on failure. Called at registration time, phase-load time, and pre-invocation. Must be cheap (no network calls). |
| `invoke()` | Runs the phase and returns `RuntimeOutput`. All native errors must be mapped to `ProviderError` at the boundary. |
| `cost_estimate()` | Best-effort pre-invocation cost estimate in USD. Return `None` if unknown. |
| `actual_cost()` | Extract actual cost from a completed `RuntimeOutput`. Return `None` if not available. |

### Capabilities

```rust
pub struct Capabilities {
    pub tool_use: bool,     // Can use Claude Code tools / exec environment
    pub streaming: bool,    // Supports token streaming
    pub vision: bool,       // Accepts image inputs
    pub thinking: bool,     // Extended reasoning / thinking mode
    pub max_tokens_in: u32, // Max context tokens
    pub max_tokens_out: u32,// Max output tokens
}
```

Capability gating is enforced by the phase system. A phase that requires `tool_use = true`
must not be dispatched to a provider where `tool_use = false`. This is Phase 2 territory;
Phase 1 makes capabilities visible — enforcement is in `validate_config`.

### InvocationContext

```rust
pub struct InvocationContext<'a> {
    pub phase: &'a PhaseConfig,
    pub prompt: &'a str,
    pub model: &'a str,      // Empty string → use provider default
    pub timeout: Duration,
    pub spec_id: Option<&'a str>,
    pub task_id: Option<&'a str>,
    pub worktree_path: &'a str,
}
```

### RuntimeOutput

```rust
pub struct RuntimeOutput {
    pub output: String,
    pub success: bool,
    pub startup_ms: u64,
    pub inference_ms: u64,
    pub total_ms: u64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub tool_call_count: i64,
}
```

Fields unavailable for a given provider should be `None` / `0`.

---

## Built-in Providers

### `claude` — ClaudeCLIProvider

| Capability | Value |
|-----------|-------|
| tool_use | ✓ |
| thinking | ✓ |
| streaming | ✗ |
| vision | ✗ |
| max_tokens_in | 200,000 |
| max_tokens_out | 8,096 |

- Binary path resolved from `CLAUDE_BIN` env var, then `claude` on PATH.
- Auth handled by the Claude CLI itself. `validate_config` always passes.
- Timeout mapped to `ProviderError::Timeout` when output is `"timeout"`.
- Cost parsed from Claude CLI's `stream-json` output events.

### `openrouter` — OpenRouterProvider

| Capability | Value |
|-----------|-------|
| tool_use | ✗ |
| thinking | ✗ |
| streaming | ✗ |
| vision | ✗ |
| max_tokens_in | 128,000 |
| max_tokens_out | 8,096 |

- Requires `OPENROUTER_API_KEY` environment variable.
- Auto-disabled at registration if the key is absent.
- Default model: `openai/gpt-4o`. Override with phase `model` field.
- Maps HTTP 401 → `AuthFailed`, 429 → `RateLimit`, non-2xx → `BadResponse`.
- Cost read from `usage.cost` in OpenRouter's response JSON.

### `codex` — CodexProvider

| Capability | Value |
|-----------|-------|
| tool_use | ✓ |
| thinking | ✓ |
| streaming | ✗ |
| vision | ✗ |
| max_tokens_in | 128,000 |
| max_tokens_out | 8,096 |

- Requires `OPENAI_API_KEY` environment variable.
- Auto-disabled at registration if the key is absent.
- Binary path resolved from `CODEX_BIN` env var, then `codex` on PATH.
- Default model: `codex-mini-latest`.
- Uses `codex exec --output-last-message` for output capture.
- Cost not available (OpenAI Codex API doesn't return it in this flow).

### `deterministic` — DeterministicProvider

Handles `commit`, `merge`, and `cleanup` phases that use `completion_handler` builtins.
No LLM is invoked. `invoke()` returns `ProviderError::NotConfigured` — callers must
route deterministic phases through the builtin system, not through `Provider::invoke`.

---

## ProviderRegistry

All providers are looked up through the registry. The runner never branches on provider
name strings directly.

```rust
pub struct ProviderRegistry {
    providers: HashMap<String, Box<dyn Provider>>,
    disabled: HashMap<String, String>,  // name → reason
}
```

### Built-in registration order

1. `claude` — always active
2. `openrouter` — active if `OPENROUTER_API_KEY` is set, disabled otherwise
3. `codex` — active if `OPENAI_API_KEY` is set, disabled otherwise
4. `deterministic` — always active

### API

```rust
registry.get("openrouter")      // → Option<&dyn Provider>; None if disabled or unknown
registry.list()                 // → Vec<(&str, ProviderStatus)> sorted by name
registry.register(provider)     // validates at registration time; auto-disables on failure
registry.disable(name, reason)  // explicitly move a provider to the disabled map
registry.validate_phase(phase)  // check if a phase's runtime is available
registry.validate_phases(iter)  // bulk check; emits WARN for each misconfigured phase
```

---

## Validation Lifecycle

There are three validation checkpoints that make misconfigured providers surface loudly
rather than silently falling through to the wrong backend.

### 1. Registration time

`ProviderRegistry::register()` calls `validate_config` on the provider before inserting it.
If validation fails the provider goes into `disabled` instead of `providers`. A missing API
key at startup means `registry.get("openrouter")` returns `None` — the runner cannot even
reach the invocation path.

### 2. Phase TOML load time

After `PhaseRegistry::new()` loads all `*.phase.toml` files, call `validate_phases()`:

```rust
provider_registry.validate_phases(phase_registry.phases());
```

For each phase with a `runtime` field that names a disabled or missing provider, this
prints a loud warning to stderr at daemon startup:

```
WARN: phase 'spec-critique' wants runtime='openrouter' but provider openrouter
not configured: OPENROUTER_API_KEY not set. Phases using this runtime will
fail until configured. Add OPENROUTER_API_KEY to ~/.boi/.env.
```

This is the gate that would have surfaced the 2026-04-29 OpenRouter-runtime-drop bug
at startup instead of silently dispatching to Claude.

### 3. Pre-invocation

The runner calls `provider.validate_config(phase)` immediately before `provider.invoke()`.
This catches cases where credentials were present at startup but removed since (e.g.
env var cleared between daemon start and phase dispatch).

---

## ProviderError Taxonomy

All providers map their native errors to `ProviderError` at the boundary. Callers only
see this enum.

| Variant | When to use |
|---------|-------------|
| `NotConfigured { provider, reason }` | Provider missing from registry, or disabled (missing API key, binary not found) |
| `AuthFailed { provider, env_var }` | API key present but rejected (HTTP 401, invalid key) |
| `RateLimit { provider, retry_after_s }` | HTTP 429; include retry-after if the API provides it |
| `Timeout { secs }` | Phase exceeded its deadline |
| `BadResponse { provider, body_excerpt }` | HTTP error, malformed JSON, missing fields |
| `NetworkError(source)` | TCP/TLS failure, DNS failure |
| `CapabilityMissing { provider, required }` | Phase requires a capability the provider lacks |
| `BudgetExceeded { provider, period }` | Soft or hard budget cap triggered |
| `Other(source)` | Catch-all for errors that don't fit the above |

All variants implement `std::error::Error` via `thiserror`. Source chains are preserved
on `NetworkError` and `Other` for root-cause inspection.

---

## Telemetry

Unified events emitted for every phase execution:

| Event | When |
|-------|------|
| `boi.phase.invoked` | Immediately before branching to the provider |
| `boi.phase.completed` | On every exit path (success or failure) |

Both events include a `provider` field. `boi.phase.completed` includes `cost_usd`,
`input_tokens`, `output_tokens`, `cache_read_tokens`, `duration_ms`.

Provider-specific events (`boi.openrouter.spawn`, `boi.claude.spawn`) have been removed.
All observability goes through the unified events above.

---

## CLI Commands

### `boi providers list`

Prints registered and disabled providers. Use this to diagnose missing API keys.

```
$ boi providers list
Registered providers:
  claude [active]
  codex [disabled: auth failed for codex: env var OPENAI_API_KEY missing or invalid]
  deterministic [active]
  openrouter [disabled: provider openrouter not configured: OPENROUTER_API_KEY not set]
```

Output is sorted alphabetically. Status is derived from the live registry — it reflects
the current environment, not a cached state.

---

## Adding a New Provider

The Codex provider (`src/runtime/codex.rs`) is the reference implementation for an
API-key-gated provider with a CLI binary.

### Step 1: Create the file

`src/runtime/my_provider.rs`

```rust
use super::{Capabilities, InvocationContext, Provider, ProviderError, RuntimeOutput};
use crate::phases::PhaseConfig;
use rust_decimal::Decimal;

pub struct MyProvider {
    pub api_key: String,
}

impl Provider for MyProvider {
    fn name(&self) -> &str { "my-provider" }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_use: false,
            streaming: false,
            vision: false,
            thinking: false,
            max_tokens_in: 128_000,
            max_tokens_out: 8_096,
        }
    }

    fn validate_config(&self, _phase: &PhaseConfig) -> Result<(), ProviderError> {
        if self.api_key.is_empty() {
            return Err(ProviderError::NotConfigured {
                provider: "my-provider".into(),
                reason: "MY_PROVIDER_API_KEY not set".into(),
            });
        }
        Ok(())
    }

    fn invoke(&self, ctx: InvocationContext) -> Result<RuntimeOutput, ProviderError> {
        // ... call your API, map errors to ProviderError variants ...
        todo!()
    }

    fn cost_estimate(&self, _ctx: &InvocationContext) -> Option<Decimal> { None }

    fn actual_cost(&self, response: &RuntimeOutput) -> Option<Decimal> {
        response.cost_usd.and_then(|c| Decimal::try_from(c).ok())
    }
}
```

### Step 2: Register in ProviderRegistry::new()

In `src/runtime/mod.rs`, add to `ProviderRegistry::new()`:

```rust
let api_key = std::env::var("MY_PROVIDER_API_KEY").unwrap_or_default();
registry.register(Box::new(my_provider::MyProvider { api_key }));
```

Registration automatically validates. If the key is absent, the provider is auto-disabled
with the reason from `validate_config`. No other code changes needed.

### Step 3: Add the module

In `src/runtime/mod.rs`:

```rust
pub mod my_provider;
```

### Step 4: Add tests

At minimum, test:

- `name()` returns the expected string
- `validate_config` fails when the key is absent (`api_key: "".into()`)
- `validate_config` passes when the key is present
- `invoke` maps a known error to the correct `ProviderError` variant

Use a fake binary (see `codex_provider` tests) or mock HTTP (see `openrouter_provider`
tests) — no live API calls in tests.

### Step 5: Update `boi doctor` (optional)

If your provider depends on a CLI binary, add a check in `src/cli/doctor.rs`.

---

## Design Invariants

- `runner.rs` never branches on provider name strings. All dispatch goes through `registry.get()`.
- Adding a new provider requires zero changes to `runner.rs` or `worker.rs`.
- A disabled provider (`registry.get()` returns `None`) causes the runner to return
  `ProviderError::NotConfigured` — never a silent fallback to Claude.
- `validate_config` must be cheap. No network calls, no filesystem writes.
