# BOI Telemetry Schema

## phase_runs table (`~/.boi/boi-rust.db`)

Source: `src/queue.rs`

Records one row per phase execution. Populated by `record_phase_run()` in `src/worker.rs`.

### Core columns (v1.0)

| Column | Type | Description |
|--------|------|-------------|
| id | INTEGER PK | Auto-increment row ID |
| spec_id | TEXT NOT NULL | Spec identifier |
| task_id | TEXT | Task identifier (null for spec-level phases) |
| phase | TEXT NOT NULL | Phase name (execute, critic, task-verify, etc.) |
| level | TEXT NOT NULL | "spec" or "task" |
| outcome | TEXT NOT NULL | proceed, redo, pause, done, failed |
| duration_ms | INTEGER | Wall-clock elapsed for phase |
| cost_usd | REAL | Total cost in USD for this phase invocation |
| input_tokens | INTEGER | Input tokens consumed |
| output_tokens | INTEGER | Output tokens generated |
| started_at | TEXT NOT NULL | RFC3339 timestamp |
| completed_at | TEXT | RFC3339 timestamp |

### Experiment instrumentation columns (v1.3)

Added to support pipeline experimentation. See `projects/hex-autonomy/boi-experiments/2026-04-29-telemetry-gaps.md` for rationale.

| Column | Type | Gap | Description |
|--------|------|-----|-------------|
| model | TEXT | 8 | Model actually used (claude-sonnet-4-6, claude-opus-4-7, etc.) |
| runtime | TEXT | 8 | Runtime dispatch method (claude, openrouter, deterministic, verify) |
| pipeline_id | TEXT | 16 | Pipeline configuration fingerprint |
| attempt | INTEGER | 4 | Retry attempt number (1-based, default 1) |
| failure_mode | TEXT | 13 | Failure taxonomy: timeout, crash, rate_limit, context_overflow, validation_fail, unknown |
| cold_start_ms | INTEGER | 2 | Time from process spawn to first output byte |
| inference_ms | INTEGER | 2 | Time from first output byte to process exit |
| cache_read_tokens | INTEGER | 15 | Anthropic prompt cache read tokens |
| cache_creation_tokens | INTEGER | 15 | Anthropic prompt cache creation tokens |
| tool_call_count | INTEGER | 6 | Number of tool calls made during the phase |
| tool_calls_by_type | TEXT | 6 | JSON object mapping tool name to call count (e.g. `{"Read":5,"Edit":3}`) |
| ttft_ms | INTEGER | 3 | Time-to-first-token (same as cold_start_ms for CLI-spawned phases) |
| loop_iteration | INTEGER | 5 | Which iteration of the critique↔improve loop produced this phase run (1-indexed; 1 = first/only pass) |
| verify_exit_code | INTEGER | 10 | Exit code from the task's shell verify command (NULL for phases with no verify command; 0 = pass, non-zero = fail) |

### Data flow

```
spawn_claude() -> ClaudeResult
  Captures: startup_ms, inference_ms, cost_usd, input/output/cache tokens
  Source: parsed from Claude CLI --output-format stream-json events

ClaudePhaseRunner::run_phase_inner() -> PhaseMetrics
  Adds: model (from PhaseConfig), runtime (resolved), failure_mode (classified)

worker::record_phase_run()
  Adds: attempt (from retry loop), pipeline_id (NOT YET WIRED — all call sites pass None; see B1 in design-review gate)
  Writes: INSERT INTO phase_runs with all columns
```

### Token/cost data sources

The `cost_usd`, `input_tokens`, `output_tokens`, `cache_read_tokens`, and `cache_creation_tokens` fields are parsed from Claude CLI's `--output-format stream-json` output:

- **`assistant` events**: `message.usage.input_tokens`, `message.usage.output_tokens`, `message.usage.cache_read_input_tokens`, `message.usage.cache_creation_input_tokens`
- **`result` events**: `total_cost_usd` or `cost_usd`, plus `usage.*` fields

These fields are `NULL` when:
- The phase uses a non-Claude runtime (deterministic, verify)
- The Claude CLI version doesn't emit usage data in stream-json
- The phase was killed by timeout before output was parsed

### Failure mode taxonomy

| Value | Meaning |
|-------|---------|
| timeout | Phase killed after exceeding deadline |
| crash | Process spawn error or unexpected termination |
| rate_limit | API rate limit (429) encountered |
| context_overflow | Context window exceeded |
| validation_fail | Phase returned Redo verdict (output rejected) |
| unknown | Failed for unclassified reason |
| NULL | Phase succeeded (outcome is proceed, done, or pause) |

---

## bench_results table

Source: `src/cli/bench.rs`

Records one row per bench run (spec × pipeline × run_number).

### Core columns (v1.0)

| Column | Type | Description |
|--------|------|-------------|
| run_id | TEXT | Bench run identifier (timestamp-based) |
| pipeline | TEXT | Pipeline name |
| spec_file | TEXT | Path to spec YAML |
| run_number | INTEGER | Run number within this battery |
| status | TEXT | Terminal status (completed, failed, timeout, etc.) |
| total_ms | INTEGER | Wall-clock for entire spec execution |
| tasks_total | INTEGER | Total task count |
| tasks_done | INTEGER | Successfully completed tasks |
| tasks_failed | INTEGER | Failed tasks |

### Cost/quality columns (v1.2)

| Column | Type | Gap | Description |
|--------|------|-----|-------------|
| total_cost_usd | REAL | 19 | Sum of cost_usd from phase_runs for this spec |
| total_input_tokens | INTEGER | 19 | Sum of input_tokens from phase_runs |
| total_output_tokens | INTEGER | 19 | Sum of output_tokens from phase_runs |
| tasks_skipped | INTEGER | 19 | Count of skipped tasks |

Cost columns are aggregated from `phase_runs` via `Queue::aggregate_spec_cost()` after spec completion.

---

## Telemetry DB — `PhaseInvocation` schema

Source: `src/telemetry.rs`

Every phase invocation emits two events:
- **`boi.phase.invoked`** — fired immediately after resolving the phase config and before branching to the runtime
- **`boi.phase.completed`** — fired on phase exit with all completion fields populated

Both events write to:
1. The `phase_runs` table in `~/.boi/boi-rust.db` (keyed by `invocation_id`)
2. The append-only JSONL audit log at `~/.hex/audit/boi-phase-runs.jsonl`
3. Daemon stderr (for live observability)

**Hard rule:** every field is either a real measured/observed value or explicitly `null`. Nothing is fabricated or copied from a TOML default if the actual call used something different.

### Invocation fields (populated at `boi.phase.invoked`)

| Field | Rust type | SQLite | Description |
|-------|-----------|--------|-------------|
| `invocation_id` | `String` | `TEXT PK` | Unique ID: `{timestamp_ms}-{random_hex}` |
| `spec_id` | `Option<String>` | `TEXT` | Spec identifier (null if unknown at emit time) |
| `task_id` | `Option<String>` | `TEXT` | Task identifier; null for spec-level phases |
| `phase_name` | `String` | `TEXT NOT NULL` | Phase name: execute, critic, spec-critique, plan-critique, etc. |
| `phase_level` | `String` | `TEXT` | "spec" or "task" |
| `mode` | `Option<String>` | `TEXT` | Execution mode: execute, challenge, discover, generate |
| `runtime` | `Option<String>` | `TEXT` | **Resolved** runtime (not TOML default): claude, openrouter, deterministic, verify |
| `model` | `Option<String>` | `TEXT` | **Resolved** model name actually passed to the runtime (e.g. `claude-sonnet-4-6`, `google/gemini-flash-1.5`) |
| `effort` | `Option<String>` | `TEXT` | Effort hint: low, medium, high |
| `thinking_enabled` | `Option<bool>` | `INTEGER` | Whether extended thinking was requested |
| `thinking_budget_tokens` | `Option<i64>` | `INTEGER` | Token budget for thinking (null if thinking not enabled) |
| `extended_thinking` | `Option<bool>` | `INTEGER` | Claude-specific extended thinking flag |
| `prompt_template_path` | `Option<String>` | `TEXT` | Path to the Handlebars/Tera template used for the prompt |
| `prompt_length_chars` | `Option<i64>` | `INTEGER` | Rendered prompt length in characters |
| `prompt_length_tokens` | `Option<i64>` | `INTEGER` | Estimated prompt token count (chars ÷ 4) |
| `timeout_secs` | `i64` | `INTEGER` | Configured timeout in seconds |
| `bare_flag` | `bool` | `INTEGER NOT NULL` | Whether `--bare` was passed to Claude CLI |
| `brain_dir` | `Option<String>` | `TEXT` | Brain/memory directory path passed to the runtime (null if none) |
| `api_key_env_used` | `Option<String>` | `TEXT` | Name of the env var read for auth (e.g. `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`) |
| `cli_args` | `Option<Vec<String>>` | `TEXT` (JSON array) | Full CLI args vector for Claude runtime; null for other runtimes |
| `http_endpoint` | `Option<String>` | `TEXT` | HTTP endpoint for OpenRouter; null for Claude runtime |
| `started_at` | `String` | `TEXT NOT NULL` | RFC3339 timestamp at phase start |
| `branch_sha` | `Option<String>` | `TEXT` | Git HEAD SHA of the worker's worktree at phase start |
| `host_os` | `Option<String>` | `TEXT` | OS name (e.g. `macos`, `linux`) |
| `host_arch` | `Option<String>` | `TEXT` | CPU architecture (e.g. `aarch64`, `x86_64`) |
| `daemon_version` | `Option<String>` | `TEXT` | BOI daemon version string |

### Completion fields (populated at `boi.phase.completed`)

| Field | Rust type | SQLite | Description |
|-------|-----------|--------|-------------|
| `completed_at` | `String` | `TEXT` | RFC3339 timestamp at phase exit |
| `duration_ms` | `i64` | `INTEGER` | Total wall-clock elapsed (started_at → completed_at) |
| `startup_ms` | `Option<i64>` | `INTEGER` | Time from process spawn to first output byte (cold-start cost) |
| `inference_ms` | `Option<i64>` | `INTEGER` | Time from first output to process exit (inference cost) |
| `input_tokens` | `Option<i64>` | `INTEGER` | Input tokens consumed (Claude only, from stream-json `usage`) |
| `output_tokens` | `Option<i64>` | `INTEGER` | Output tokens generated (Claude only) |
| `cache_read_tokens` | `Option<i64>` | `INTEGER` | Prompt cache read tokens (Claude only) |
| `cache_creation_tokens` | `Option<i64>` | `INTEGER` | Prompt cache creation tokens (Claude only) |
| `cost_usd` | `Option<f64>` | `REAL` | Total cost in USD (Claude only, from `result.total_cost_usd`) |
| `exit_status` | `String` | `TEXT` | Terminal outcome: success, timeout, nonzero, crashed |
| `exit_reason` | `Option<String>` | `TEXT` | JSON-encoded `FailureReason` if failed; null on success (see SA015) |

### Retry field

| Field | Rust type | SQLite | Description |
|-------|-----------|--------|-------------|
| `retry_index` | `Option<i64>` | `INTEGER` | Retry attempt number (0-indexed); null on first attempt |

### Exit status values

| Value | Meaning |
|-------|---------|
| `success` | Phase exited 0 and output was accepted |
| `timeout` | Phase killed after exceeding `timeout_secs` |
| `nonzero` | Phase exited with non-zero exit code |
| `crashed` | Process failed to spawn or terminated by signal |

The `exit_reason` field encodes the full `FailureReason` from spec SA015 (failure-visibility spec) as a JSON object when the phase fails.

### Example JSONL record (`boi.phase.invoked`)

```json
{
  "event": "boi.phase.invoked",
  "invocation_id": "1745980800000-3f2a1b4c8d9e0f1a",
  "spec_id": "S9CE3",
  "task_id": "TD42B",
  "phase_name": "execute",
  "phase_level": "task",
  "mode": "execute",
  "runtime": "claude",
  "model": "claude-sonnet-4-6",
  "effort": "medium",
  "thinking_enabled": false,
  "thinking_budget_tokens": null,
  "extended_thinking": null,
  "prompt_template_path": "~/.boi/prompts/execute.md",
  "prompt_length_chars": 12430,
  "prompt_length_tokens": 3107,
  "timeout_secs": 600,
  "bare_flag": true,
  "brain_dir": null,
  "api_key_env_used": "ANTHROPIC_API_KEY",
  "cli_args": ["claude", "-p", "--output-format", "stream-json", "--bare", "..."],
  "http_endpoint": null,
  "started_at": "2026-04-29T20:00:00Z",
  "branch_sha": "56ceccc",
  "host_os": "macos",
  "host_arch": "aarch64",
  "daemon_version": "1.1.0"
}
```

---

## Example queries

These queries run against `~/.boi/boi-rust.db`. Use `sqlite3 ~/.boi/boi-rust.db`.

### Show all phases that used a specific model

```sql
-- All phase invocations that used gemini-flash
SELECT phase_name, spec_id, task_id, started_at, duration_ms, cost_usd
FROM phase_runs
WHERE model LIKE '%gemini-flash%'
ORDER BY started_at DESC;
```

### Cost per phase per spec

```sql
-- Total cost broken down by spec and phase
SELECT spec_id, phase_name, runtime, model,
       COUNT(*) AS invocations,
       SUM(cost_usd) AS total_cost_usd,
       AVG(duration_ms) AS avg_duration_ms
FROM phase_runs
WHERE cost_usd IS NOT NULL
GROUP BY spec_id, phase_name
ORDER BY total_cost_usd DESC;
```

### Find phases where runtime config was wrong

```sql
-- Phases logged as "verify" that may be misrouted openrouter calls
-- (see diagnostics/2026-04-29-openrouter-not-firing.md)
SELECT invocation_id, spec_id, phase_name, runtime, model, exit_status, started_at
FROM phase_runs
WHERE runtime = 'verify' AND model IS NOT NULL
ORDER BY started_at DESC;
```

### Startup latency by model

```sql
-- Cold-start cost (process spawn → first token) per model
SELECT model, runtime,
       COUNT(*) AS samples,
       AVG(startup_ms) AS avg_startup_ms,
       MAX(startup_ms) AS max_startup_ms
FROM phase_runs
WHERE startup_ms IS NOT NULL
GROUP BY model, runtime
ORDER BY avg_startup_ms DESC;
```

### Cache efficiency per spec

```sql
-- Prompt cache hit rate: cache_read vs cache_creation tokens
SELECT spec_id, phase_name,
       SUM(cache_read_tokens) AS total_cache_hits,
       SUM(cache_creation_tokens) AS total_cache_writes,
       ROUND(100.0 * SUM(cache_read_tokens) /
             NULLIF(SUM(cache_read_tokens) + SUM(cache_creation_tokens), 0), 1) AS hit_rate_pct
FROM phase_runs
WHERE cache_read_tokens IS NOT NULL OR cache_creation_tokens IS NOT NULL
GROUP BY spec_id, phase_name
ORDER BY spec_id, phase_name;
```

### Recent failures with exit reason

```sql
-- All failed phases in the last 24 hours
SELECT invocation_id, spec_id, phase_name, runtime, exit_status, exit_reason, started_at
FROM phase_runs
WHERE exit_status != 'success'
  AND started_at >= datetime('now', '-1 day')
ORDER BY started_at DESC;
```

### Identify retry patterns

```sql
-- Phases that needed retries — sorted by retry count
SELECT spec_id, task_id, phase_name, MAX(retry_index) AS max_retries, COUNT(*) AS attempts
FROM phase_runs
WHERE retry_index IS NOT NULL AND retry_index > 0
GROUP BY spec_id, task_id, phase_name
ORDER BY max_retries DESC;
```

---

## Audit log

Location: `~/.hex/audit/boi-phase-runs.jsonl`

JSONL file with one line per `boi.phase.invoked` and `boi.phase.completed` event. Contains all fields from both tables. Used for post-hoc analysis when the DB is unavailable or for external tooling.

To tail live phase events during a run:

```bash
tail -f ~/.hex/audit/boi-phase-runs.jsonl | jq 'select(.event == "boi.phase.invoked") | {phase: .phase_name, runtime: .runtime, model: .model}'
```
