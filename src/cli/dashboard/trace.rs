//! Parsing the OTLP trace JSONL into dashboard log events.
//!
//! Each `~/.boi/v2/traces/{date}/{trace_id}.jsonl` line is a canonical
//! OTLP/JSON `ExportTraceServiceRequest` (written by `runtime::otel_export`).
//! This module flattens the spans into a flat, time-ordered list of
//! [`LogEvent`]s and classifies each as think (LLM turn) or do (tool call).
//!
//! ## Span-name reality (confirmed against `runtime/otel.rs` and the fixture)
//!
//! The `OtelObserver` writes two kinds of spans:
//!   - `"invoke_workflow boi.spec"` — root span (skipped)
//!   - `"invoke_agent boi.worker"` — per-phase span (skipped; carries token counts
//!     on `gen_ai.usage.input_tokens` / `gen_ai.usage.output_tokens`)
//!
//! The worker subprocess (via the Phase 8a.3 hoover) re-parents its own spans
//! under the `invoke_agent` span.  Those child spans use:
//!   - `"chat <model>"` (e.g. `"chat claude-opus-4-7"`) — an LLM turn (Think);
//!     carries `gen_ai.request.model` (string). Token counts live on the parent
//!     `invoke_agent` span, not on individual `chat` spans.
//!   - `"execute_tool <tool>"` (e.g. `"execute_tool Bash"`) — a tool call (Do);
//!     carries `boi.tool` (tool name, string) from BOI-native spans and/or
//!     `gen_ai.tool.name` (string) from hoover-normalised worker spans.
//!
//! ## Correlation mechanism
//!
//! `boi.phase_run_id` is **not** stored as a span attribute.  Instead, each
//! `invoke_agent boi.worker` span carries the natural key that uniquely
//! identifies a phase run via the `UNIQUE(spec_id, task_id, phase,
//! phase_iteration)` constraint:
//!
//!   - `boi.task_id`   — present only for task phases, absent for spec-level
//!   - `boi.phase`     — phase name string
//!   - `boi.iteration` — iteration counter (i64, stored as OTLP `intValue`)
//!
//! `spec_id` is omitted from [`PhaseKey`] because a dashboard view is always
//! scoped to one spec.
//!
//! Tool/chat child spans carry **no** phase attributes themselves; they are
//! correlated by walking `parentSpanId` up to the nearest `invoke_agent
//! boi.worker` ancestor.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;

/// Whether an event is model think-time or tool do-time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// An LLM turn.
    Think,
    /// A tool call.
    Do,
}

/// Identifies the phase run a log event belongs to — the natural key of the
/// `phase_runs` `UNIQUE(spec_id, task_id, phase, phase_iteration)` constraint.
///
/// `spec_id` is omitted because a dashboard view is always scoped to one spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseKey {
    /// Present for task-level phases; `None` for spec-level phases.
    pub task_id: Option<String>,
    /// Phase name (e.g. `"execute"`).
    pub phase: String,
    /// Phase iteration counter.
    pub iteration: i64,
}

/// One flattened span from the trace — a line in the leaf log.
#[derive(Debug, Clone)]
pub struct LogEvent {
    /// Think or do.
    pub kind: EventKind,
    /// The phase run this event belongs to, resolved via parent-span walk.
    pub phase: PhaseKey,
    /// Display label (`chat claude-opus-4-7`, `execute_tool Bash`, …).
    pub label: String,
    /// Span start.
    pub started_at: DateTime<Utc>,
    /// Span end — `None` if the span has not closed.
    pub completed_at: Option<DateTime<Utc>>,
    /// Detail shown when the log line is expanded (args / output tail).
    pub detail: String,
}

impl LogEvent {
    /// Duration in milliseconds, using `now` for an open span.
    pub fn duration_ms(&self, now: DateTime<Utc>) -> u64 {
        let end = self.completed_at.unwrap_or(now);
        (end - self.started_at).num_milliseconds().max(0) as u64
    }
}

/// Minimal OTLP/JSON shapes — only the fields the dashboard needs.
#[derive(Deserialize)]
struct ExportRequest {
    #[serde(rename = "resourceSpans", default)]
    resource_spans: Vec<ResourceSpans>,
}
#[derive(Deserialize)]
struct ResourceSpans {
    #[serde(rename = "scopeSpans", default)]
    scope_spans: Vec<ScopeSpans>,
}
#[derive(Deserialize)]
struct ScopeSpans {
    #[serde(default)]
    spans: Vec<Span>,
}
#[derive(Deserialize)]
struct Span {
    #[serde(default)]
    name: String,
    /// OTLP hex string identifying this span.
    #[serde(rename = "spanId", default)]
    span_id: String,
    /// OTLP hex string of the parent span; empty for root spans.
    #[serde(rename = "parentSpanId", default)]
    parent_span_id: String,
    #[serde(rename = "startTimeUnixNano", default)]
    start_nano: String,
    #[serde(rename = "endTimeUnixNano", default)]
    end_nano: String,
    #[serde(default)]
    attributes: Vec<KeyValue>,
}
#[derive(Deserialize)]
struct KeyValue {
    #[serde(default)]
    key: String,
    #[serde(default)]
    value: AnyValue,
}
#[derive(Deserialize, Default)]
struct AnyValue {
    /// String attributes (e.g. `boi.phase`, `boi.task_id`).
    #[serde(rename = "stringValue", default)]
    string_value: Option<String>,
    /// Integer attributes stored as decimal strings in OTLP/JSON
    /// (e.g. `boi.iteration`).
    #[serde(rename = "intValue", default)]
    int_value: Option<String>,
}

/// Parse a whole trace JSONL string into time-ordered [`LogEvent`]s.
///
/// Lines that fail to parse are skipped — a half-written trailing line during
/// a live tail is expected, not an error.
pub fn parse_trace(jsonl: &str) -> Vec<LogEvent> {
    // Collect all spans from every well-formed line.
    let all_spans: Vec<Span> = jsonl
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<ExportRequest>(line).ok())
        .flat_map(|req| req.resource_spans)
        .flat_map(|rs| rs.scope_spans)
        .flat_map(|ss| ss.spans)
        .collect();

    // Build spanId → PhaseKey index from every `invoke_agent boi.worker` span.
    let phase_index: HashMap<String, PhaseKey> = all_spans
        .iter()
        .filter(|s| s.name == "invoke_agent boi.worker")
        .filter_map(|s| {
            let key = phase_key_from_span(s)?;
            Some((s.span_id.clone(), key))
        })
        .collect();

    // Build spanId → parentSpanId index for upward walks.
    let parent_index: HashMap<String, String> = all_spans
        .iter()
        .filter(|s| !s.span_id.is_empty() && !s.parent_span_id.is_empty())
        .map(|s| (s.span_id.clone(), s.parent_span_id.clone()))
        .collect();

    // Emit LogEvents for tool/chat spans only.
    let mut events: Vec<LogEvent> = all_spans
        .iter()
        .filter_map(|s| span_to_event(s, &phase_index, &parent_index))
        .collect();

    events.sort_by_key(|e| e.started_at);
    events
}

/// Extract a [`PhaseKey`] from an `invoke_agent boi.worker` span.
fn phase_key_from_span(span: &Span) -> Option<PhaseKey> {
    let phase = attr_string(&span.attributes, "boi.phase")?;
    let iteration = attr_i64(&span.attributes, "boi.iteration").unwrap_or(0);
    let task_id = attr_string(&span.attributes, "boi.task_id");
    Some(PhaseKey {
        task_id,
        phase,
        iteration,
    })
}

/// Resolve the [`PhaseKey`] for a span by walking up the parent chain until
/// an `invoke_agent boi.worker` entry is found in `phase_index`.
///
/// Returns `None` if no enclosing phase span exists.
fn resolve_phase_key<'a>(
    span_id: &str,
    phase_index: &'a HashMap<String, PhaseKey>,
    parent_index: &HashMap<String, String>,
) -> Option<&'a PhaseKey> {
    let mut current = span_id;
    // Bound the walk to avoid cycles in malformed data.
    for _ in 0..32 {
        if let Some(key) = phase_index.get(current) {
            return Some(key);
        }
        match parent_index.get(current) {
            Some(parent) => current = parent.as_str(),
            None => return None,
        }
    }
    None
}

/// Build the expand-detail string for an `execute_tool` span.
///
/// Sources (in priority order):
///  1. `boi.tool.args_summary` — a human-readable args synopsis written by the
///     BOI OTel observer when it records a `ToolInvoked` event (present on
///     BOI-native tool spans).
///  2. `boi.tool` — the tool name (also from BOI-native spans); used alone when
///     no args summary exists.
///  3. `gen_ai.tool.name` — the tool name on hoover-normalised worker spans.
///
/// Returns an empty string when none of these attributes are present.
fn detail_for_tool(attrs: &[KeyValue]) -> String {
    // Prefer an args summary when available.
    if let Some(args) = attr_string(attrs, "boi.tool.args_summary") {
        // Include the tool name too when we have it.
        if let Some(name) = attr_string(attrs, "boi.tool") {
            return format!("{name}: {args}");
        }
        return args;
    }
    // Fall back to the tool name alone.
    if let Some(name) = attr_string(attrs, "boi.tool") {
        return name;
    }
    // Hoover-normalised worker span carries `gen_ai.tool.name`.
    attr_string(attrs, "gen_ai.tool.name").unwrap_or_default()
}

/// Build the expand-detail string for a `chat` span.
///
/// Sources:
///  - `gen_ai.request.model` — the model name (present on both BOI-native and
///    hoover-normalised spans).
///
/// Token counts (`gen_ai.usage.input_tokens` / `gen_ai.usage.output_tokens`)
/// are recorded on the parent `invoke_agent boi.worker` span, not on individual
/// `chat` spans, so they are not available here.
///
/// Returns an empty string when no model attribute is present.
fn detail_for_chat(attrs: &[KeyValue]) -> String {
    attr_string(attrs, "gen_ai.request.model").unwrap_or_default()
}

/// Convert one span into a `LogEvent`, or `None` if it should be skipped.
///
/// Skipped spans: `invoke_workflow boi.spec`, `invoke_agent boi.worker`, and
/// any span that cannot be correlated to a phase (no `invoke_agent` ancestor).
///
/// ## Detail population
///
/// The `detail` field is built from whichever real span attributes are present:
///   - `execute_tool` spans: `boi.tool.args_summary` (+ `boi.tool` name prefix)
///     or `boi.tool` alone or `gen_ai.tool.name` — see [`detail_for_tool`].
///   - `chat` spans: `gen_ai.request.model` — see [`detail_for_chat`].
fn span_to_event(
    span: &Span,
    phase_index: &HashMap<String, PhaseKey>,
    parent_index: &HashMap<String, String>,
) -> Option<LogEvent> {
    let kind = if span.name.starts_with("execute_tool") {
        EventKind::Do
    } else if span.name.starts_with("chat ") {
        EventKind::Think
    } else {
        return None;
    };

    let phase_key = resolve_phase_key(&span.span_id, phase_index, parent_index)?;

    let detail = match kind {
        EventKind::Do => detail_for_tool(&span.attributes),
        EventKind::Think => detail_for_chat(&span.attributes),
    };

    Some(LogEvent {
        kind,
        phase: phase_key.clone(),
        label: span.name.clone(),
        started_at: nanos_to_dt(&span.start_nano)?,
        completed_at: nanos_to_dt(&span.end_nano),
        detail,
    })
}

/// Look up a string attribute by key.
fn attr_string(attrs: &[KeyValue], key: &str) -> Option<String> {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.string_value.clone())
        .filter(|s| !s.is_empty())
}

/// Look up an integer attribute by key.  OTLP/JSON encodes integers as decimal
/// strings under the `intValue` field.
fn attr_i64(attrs: &[KeyValue], key: &str) -> Option<i64> {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.int_value.as_deref())
        .and_then(|s| s.parse().ok())
}

/// Parse an OTLP `*UnixNano` decimal string into a `DateTime<Utc>`.
fn nanos_to_dt(nanos: &str) -> Option<DateTime<Utc>> {
    let n: i64 = nanos.parse().ok()?;
    if n == 0 {
        return None;
    }
    Utc.timestamp_opt(n / 1_000_000_000, (n % 1_000_000_000) as u32)
        .single()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_trace_skips_malformed_trailing_line() {
        let fixture = include_str!("../../../tests/fixtures/traces/sample_otlp.jsonl");
        // A half-written trailing line must not panic or abort the parse.
        let corrupted = format!("{fixture}\n{{\"resourceSpans\": [");
        let events = parse_trace(&corrupted);
        // The fixture's well-formed lines still parse.
        assert_eq!(events.len(), parse_trace(fixture).len());
    }

    #[test]
    fn parse_trace_orders_events_by_start() {
        let fixture = include_str!("../../../tests/fixtures/traces/sample_otlp.jsonl");
        let events = parse_trace(fixture);
        for pair in events.windows(2) {
            assert!(pair[0].started_at <= pair[1].started_at);
        }
    }

    /// The fixture contains one `execute_tool Bash` span — confirm at least one
    /// `EventKind::Do` event is classified correctly.
    #[test]
    fn parse_trace_classifies_do_event() {
        let fixture = include_str!("../../../tests/fixtures/traces/sample_otlp.jsonl");
        let events = parse_trace(fixture);
        assert!(
            events.iter().any(|e| e.kind == EventKind::Do),
            "expected at least one Do event from the fixture's execute_tool span"
        );
    }

    /// An `invoke_agent boi.worker` parent span with child `execute_tool` and
    /// `chat` spans must resolve to the correct [`PhaseKey`].
    ///
    /// Uses an inline JSONL that mirrors the real OTLP structure so the test
    /// does not depend on fixture file contents changing.  Each JSONL line must
    /// be a single self-contained JSON object (JSONL = newline-delimited JSON).
    #[test]
    fn parse_trace_resolves_phase_key_from_parent_span() {
        // One request with three spans — all on a single JSONL line:
        //   1. invoke_agent boi.worker — carries boi.phase, boi.task_id, boi.iteration
        //   2. execute_tool Bash       — child of (1)
        //   3. chat claude-sonnet      — child of (1)
        let jsonl = concat!(
            r#"{"resourceSpans":[{"scopeSpans":[{"spans":["#,
            r#"{"name":"invoke_agent boi.worker","spanId":"aabbccdd00000001","parentSpanId":"aabbccdd00000000","startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000001000000000","attributes":[{"key":"boi.spec_id","value":{"stringValue":"STEST001"}},{"key":"boi.task_id","value":{"stringValue":"TTEST001"}},{"key":"boi.phase","value":{"stringValue":"verify"}},{"key":"boi.iteration","value":{"intValue":"3"}}]},"#,
            r#"{"name":"execute_tool Bash","spanId":"aabbccdd00000002","parentSpanId":"aabbccdd00000001","startTimeUnixNano":"1700000000100000000","endTimeUnixNano":"1700000000500000000","attributes":[{"key":"boi.tool","value":{"stringValue":"Bash"}}]},"#,
            r#"{"name":"chat claude-sonnet","spanId":"aabbccdd00000003","parentSpanId":"aabbccdd00000001","startTimeUnixNano":"1700000000600000000","endTimeUnixNano":"1700000000900000000","attributes":[]}"#,
            r#"]}]}]}"#,
        );

        let events = parse_trace(jsonl);

        assert_eq!(events.len(), 2, "two child spans → two log events");

        let expected_key = PhaseKey {
            task_id: Some("TTEST001".to_owned()),
            phase: "verify".to_owned(),
            iteration: 3,
        };

        for event in &events {
            assert_eq!(
                event.phase, expected_key,
                "event {:?} should resolve to PhaseKey {:?}",
                event.label, expected_key
            );
        }

        // Ordering: execute_tool starts before chat in this fixture.
        assert_eq!(events[0].kind, EventKind::Do);
        assert_eq!(events[1].kind, EventKind::Think);
    }

    /// A `chat` or `execute_tool` span with no `invoke_agent` ancestor must be
    /// silently dropped — it cannot be correlated to any phase.
    #[test]
    fn parse_trace_drops_orphan_spans() {
        let jsonl = r#"{"resourceSpans":[{"scopeSpans":[{"spans":[{"name":"execute_tool Bash","spanId":"deadbeef00000001","parentSpanId":"deadbeef00000000","startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000001000000000","attributes":[]}]}]}]}"#;

        let events = parse_trace(jsonl);
        assert!(
            events.is_empty(),
            "orphan tool span with no invoke_agent ancestor must be dropped"
        );
    }

    /// Verify the real fixture's tool/chat spans resolve to a non-empty PhaseKey.
    #[test]
    fn parse_trace_fixture_events_have_non_empty_phase_key() {
        let fixture = include_str!("../../../tests/fixtures/traces/sample_otlp.jsonl");
        let events = parse_trace(fixture);
        assert!(
            !events.is_empty(),
            "fixture must produce at least one log event"
        );
        for event in &events {
            assert!(
                !event.phase.phase.is_empty(),
                "event {:?} has empty phase name",
                event.label
            );
        }
    }

    /// `execute_tool` spans with `boi.tool.args_summary` produce a detail string
    /// of the form `"<tool>: <args_summary>"`.
    #[test]
    fn detail_for_do_event_uses_boi_tool_and_args_summary() {
        // A minimal self-contained JSONL with one invoke_agent parent and one
        // execute_tool child that carries both `boi.tool` and
        // `boi.tool.args_summary`.
        let jsonl = concat!(
            r#"{"resourceSpans":[{"scopeSpans":[{"spans":["#,
            r#"{"name":"invoke_agent boi.worker","spanId":"aa00000000000001","parentSpanId":"aa00000000000000","startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000002000000000","attributes":[{"key":"boi.phase","value":{"stringValue":"execute"}},{"key":"boi.iteration","value":{"intValue":"0"}}]},"#,
            r#"{"name":"execute_tool Bash","spanId":"aa00000000000002","parentSpanId":"aa00000000000001","startTimeUnixNano":"1700000000100000000","endTimeUnixNano":"1700000000500000000","attributes":[{"key":"boi.tool","value":{"stringValue":"Bash"}},{"key":"boi.tool.args_summary","value":{"stringValue":"cargo test --lib"}}]}"#,
            r#"]}]}]}"#,
        );

        let events = parse_trace(jsonl);
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.kind, EventKind::Do);
        assert_eq!(
            ev.detail, "Bash: cargo test --lib",
            "detail must be '<tool>: <args_summary>'"
        );
    }

    /// `execute_tool` spans with only `boi.tool` (no args summary) produce the
    /// tool name as the detail string.
    #[test]
    fn detail_for_do_event_falls_back_to_boi_tool_name() {
        let jsonl = concat!(
            r#"{"resourceSpans":[{"scopeSpans":[{"spans":["#,
            r#"{"name":"invoke_agent boi.worker","spanId":"bb00000000000001","parentSpanId":"bb00000000000000","startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000002000000000","attributes":[{"key":"boi.phase","value":{"stringValue":"execute"}},{"key":"boi.iteration","value":{"intValue":"0"}}]},"#,
            r#"{"name":"execute_tool Bash","spanId":"bb00000000000002","parentSpanId":"bb00000000000001","startTimeUnixNano":"1700000000100000000","endTimeUnixNano":"1700000000500000000","attributes":[{"key":"boi.tool","value":{"stringValue":"Bash"}}]}"#,
            r#"]}]}]}"#,
        );

        let events = parse_trace(jsonl);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].detail, "Bash");
    }

    /// Hoover-normalised worker `execute_tool` spans carry `gen_ai.tool.name`
    /// instead of `boi.tool`; that attribute must be used as the detail.
    #[test]
    fn detail_for_do_event_uses_gen_ai_tool_name_from_hoover_span() {
        let jsonl = concat!(
            r#"{"resourceSpans":[{"scopeSpans":[{"spans":["#,
            r#"{"name":"invoke_agent boi.worker","spanId":"cc00000000000001","parentSpanId":"cc00000000000000","startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000002000000000","attributes":[{"key":"boi.phase","value":{"stringValue":"execute"}},{"key":"boi.iteration","value":{"intValue":"0"}}]},"#,
            r#"{"name":"execute_tool Bash","spanId":"cc00000000000002","parentSpanId":"cc00000000000001","startTimeUnixNano":"1700000000100000000","endTimeUnixNano":"1700000000500000000","attributes":[{"key":"gen_ai.tool.name","value":{"stringValue":"Bash"}}]}"#,
            r#"]}]}]}"#,
        );

        let events = parse_trace(jsonl);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].detail, "Bash");
    }

    /// `chat` spans produce the model name as their detail string via
    /// `gen_ai.request.model`.
    #[test]
    fn detail_for_think_event_uses_gen_ai_request_model() {
        let jsonl = concat!(
            r#"{"resourceSpans":[{"scopeSpans":[{"spans":["#,
            r#"{"name":"invoke_agent boi.worker","spanId":"dd00000000000001","parentSpanId":"dd00000000000000","startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000002000000000","attributes":[{"key":"boi.phase","value":{"stringValue":"execute"}},{"key":"boi.iteration","value":{"intValue":"0"}}]},"#,
            r#"{"name":"chat claude-opus-4-7","spanId":"dd00000000000002","parentSpanId":"dd00000000000001","startTimeUnixNano":"1700000000100000000","endTimeUnixNano":"1700000000900000000","attributes":[{"key":"gen_ai.request.model","value":{"stringValue":"claude-opus-4-7"}}]}"#,
            r#"]}]}]}"#,
        );

        let events = parse_trace(jsonl);
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.kind, EventKind::Think);
        assert_eq!(ev.detail, "claude-opus-4-7");
    }

    /// `chat` spans with no `gen_ai.request.model` attribute produce an empty
    /// detail string — graceful degradation, not a panic.
    #[test]
    fn detail_for_think_event_is_empty_when_no_model_attr() {
        let jsonl = concat!(
            r#"{"resourceSpans":[{"scopeSpans":[{"spans":["#,
            r#"{"name":"invoke_agent boi.worker","spanId":"ee00000000000001","parentSpanId":"ee00000000000000","startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000002000000000","attributes":[{"key":"boi.phase","value":{"stringValue":"execute"}},{"key":"boi.iteration","value":{"intValue":"0"}}]},"#,
            r#"{"name":"chat claude-opus-4-7","spanId":"ee00000000000002","parentSpanId":"ee00000000000001","startTimeUnixNano":"1700000000100000000","endTimeUnixNano":"1700000000900000000","attributes":[]}"#,
            r#"]}]}]}"#,
        );

        let events = parse_trace(jsonl);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].detail, "");
    }

    /// Fixture sanity: the real `sample_otlp.jsonl` `execute_tool Bash` span
    /// carries `boi.tool` — confirm the detail is non-empty.
    #[test]
    fn fixture_execute_tool_span_has_non_empty_detail() {
        let fixture = include_str!("../../../tests/fixtures/traces/sample_otlp.jsonl");
        let events = parse_trace(fixture);
        let tool_events: Vec<_> = events.iter().filter(|e| e.kind == EventKind::Do).collect();
        assert!(
            !tool_events.is_empty(),
            "fixture must contain at least one Do event"
        );
        for ev in &tool_events {
            assert!(
                !ev.detail.is_empty(),
                "execute_tool event {:?} must have non-empty detail from boi.tool",
                ev.label
            );
        }
    }

    /// Fixture sanity: the real `sample_otlp.jsonl` `chat` span carries
    /// `gen_ai.request.model` — confirm the detail is non-empty.
    #[test]
    fn fixture_chat_span_has_non_empty_detail() {
        let fixture = include_str!("../../../tests/fixtures/traces/sample_otlp.jsonl");
        let events = parse_trace(fixture);
        let think_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::Think)
            .collect();
        assert!(
            !think_events.is_empty(),
            "fixture must contain at least one Think event"
        );
        for ev in &think_events {
            assert!(
                !ev.detail.is_empty(),
                "chat event {:?} must have non-empty detail from gen_ai.request.model",
                ev.label
            );
        }
    }
}
