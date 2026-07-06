# Telemetry

What BOI records while it runs, where it lands, and how to query it. Three
consumers read this data: `boi dashboard` (live TUI), `boi log` (phase-run
history), and the `boi traces` / `boi failures` SQL surface (this page).

## What gets recorded

Every event the daemon's bus emits is forwarded to an OTel observer
(`src/runtime/otel.rs`) and turned into spans and span events:

```
invoke_workflow boi.spec        one span per spec   (opened by SpecStarted)
└── invoke_agent boi.worker     one span per phase  (opened by PhaseStarted)
```

Observational events attach span *events* to those spans:

| Bus event | Span event |
|---|---|
| `ToolInvoked` | `execute_tool` |
| `VerifyChecked` | `boi.verify` |
| `DecisionMade` | `boi.decision_recorded` |
| `ErrorEncountered` | `boi.error` |
| `ReportReceived` | `boi.task_reported` |

Observation is best-effort by design: a failing observer logs a warning and
never aborts the state transition it was watching.

Alongside the traces, every phase execution is a row in the `phase_runs`
table of `~/.boi/v2/boi.db` — inserted at `PhaseStarted`, completed with the
phase's verdict and synopsis, heartbeated every ~30 s while live
(`src/repo/phase_runs.rs`).

## Where it lands

| Path | What |
|---|---|
| `~/.boi/v2/traces/{date}/{trace_id}.jsonl` | OTel traces. Each line is one canonical OTLP/JSON `ExportTraceServiceRequest` — produced by the OTel project's own encoding, not hand-rolled (`src/runtime/otel_export.rs`) |
| `~/.boi/v2/boi.db` | SQLite: `phase_runs` plus the spec/task state tables |

Spans are exported synchronously the moment they end (a `SimpleSpanProcessor`
file append — no batching task), so the trace file is current even if the
daemon dies right after a phase completes.

## Querying

Both commands are read-only and need no daemon. They require the `duckdb`
build feature (on by default); a `--no-default-features` binary still parses
them but exits non-zero with a clear message.

```bash
boi failures top --last 7d --n 10   # top recurring failure fingerprints
boi traces query '<SQL>'            # arbitrary read-only SQL over the traces
```

`boi traces query` runs your SQL in a DuckDB session that has:

- the OTel JSONL readable via `read_otlp_traces('<traces glob>')` (the DuckDB
  `otlp` community extension), and
- `~/.boi/v2/boi.db` ATTACHed read-only, so trace data joins against
  `phase_runs` and the spec/task state tables.

Example — count span events by name across all traces (the same
`events_json` pattern `boi failures top` is built on, see
`src/runtime/duckdb.rs`):

```bash
boi traces query "
  SELECT json_extract_string(ev.unnest, '\$.name') AS event, COUNT(*) AS n
  FROM read_otlp_traces('$HOME/.boi/v2/traces/**/*.jsonl') AS t,
       UNNEST(from_json(t.events_json, '[\"json\"]')) AS ev
  WHERE t.events_json IS NOT NULL
  GROUP BY event ORDER BY n DESC"
```

For phase-level timing and history without SQL, `boi log <spec-id>` prints
the phase-run rows directly, and `boi dashboard` renders the same data as a
navigable bar-tree (see
[design/2026-05-21-boi-dashboard-tui-design.md](design/2026-05-21-boi-dashboard-tui-design.md)).

## See also

- [getting-started.md](getting-started.md) — where all `~/.boi/v2/` state lives
- [agents/debugging.md](agents/debugging.md) — the diagnostic command ladder
- `src/runtime/otel.rs`, `src/runtime/otel_export.rs`, `src/cli/traces.rs`,
  `src/runtime/duckdb.rs` — the implementing modules (each opens with a `//!`
  design doc; ground truth if this page goes stale)
