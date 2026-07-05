//! The worker-OTel hoover (Task 8a.3).
//!
//! Spawned workers emit their *own* OpenTelemetry: a `claude_code` worker run
//! with `CLAUDE_CODE_ENABLE_TELEMETRY=1` writes session/prompt spans; a Goose
//! worker emits `gen_ai.*` spans. Those land in a worker-local OTLP/JSON file
//! with a worker-local trace id and provider-specific span names.
//!
//! [`hoover_worker_spans`] *ingests* that file into BOI's trace: it
//!
//! 1. **re-parents** every worker span under BOI's `invoke_agent boi.worker`
//!    span — the worker trace's root spans get BOI's `invoke_agent` span as
//!    their parent, and every span is re-stamped with BOI's trace id, so the
//!    worker spans become the `chat`/`execute_tool` children §8's hierarchy
//!    expects; and
//! 2. **normalizes span names** to §8's GenAI-semconv names (`chat`,
//!    `execute_tool`) — re-parenting alone would leave provider-specific names
//!    (`claude_code.chat`, `tool.Bash`) that break the hierarchy (review item
//!    29).
//!
//! The re-stamped spans are appended to BOI's own
//! `<out_dir>/{date}/{boi_trace_id}.jsonl` file — the same canonical OTLP/JSON
//! the [`super::otel_export`] exporter writes, so Phase 8c's `read_otlp_traces`
//! sees one coherent trace.

use std::io::Write;
use std::path::Path;

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;

use super::otel_export::OtelError;

/// The identity of BOI's `invoke_agent boi.worker` span — the parent the
/// hoover re-parents worker spans onto.
///
/// `hoover_worker_spans` is a free function (kept in its own file per the S12
/// split); it cannot reach the [`OtelObserver`](super::otel::OtelObserver)'s
/// `phase_spans` registry, so the caller resolves the parent span's identity
/// — via [`OtelObserver::phase_span_ref`](super::otel::OtelObserver::phase_span_ref)
/// — and hands it in. (Plan-signature deviation — see the commit.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoiSpanRef {
    /// BOI's 16-byte trace id — every hoovered span is re-stamped with this.
    pub trace_id: [u8; 16],
    /// BOI's 8-byte `invoke_agent` span id — the new parent of the worker
    /// trace's root spans.
    pub span_id: [u8; 8],
}

/// The lowercase-hex form of BOI's trace id — the `<trace_id>.jsonl` filename.
fn trace_id_hex(trace_id: &[u8; 16]) -> String {
    trace_id.iter().map(|b| format!("{b:02x}")).collect()
}

/// Map a worker span to its §8 GenAI-semconv name.
///
/// The semconv standard carries the operation in the `gen_ai.operation.name`
/// attribute (`chat`, `execute_tool`, `text_completion`, …); that is consulted
/// first. Absent it, a name-prefix heuristic covers providers that name the
/// span instead of attributing it. A span that is neither an LLM call nor a
/// tool call keeps its name — re-parented but not renamed (the worker's own
/// structural spans, e.g. a session root).
fn normalized_name(span: &opentelemetry_proto::tonic::trace::v1::Span) -> Option<String> {
    // 1. The semconv attribute — authoritative.
    for kv in &span.attributes {
        if kv.key == "gen_ai.operation.name" {
            if let Some(op) = kv.value.as_ref().and_then(string_value) {
                return Some(match op.as_str() {
                    "chat" | "text_completion" | "generate_content" => "chat".to_owned(),
                    "execute_tool" => "execute_tool".to_owned(),
                    // An unrecognized GenAI operation — keep it verbatim
                    // rather than mislabel it.
                    other => other.to_owned(),
                });
            }
        }
    }
    // 2. Name-prefix heuristic for providers that do not attribute the op.
    let lower = span.name.to_ascii_lowercase();
    if lower.contains("chat") || lower.contains("completion") {
        return Some("chat".to_owned());
    }
    if lower.starts_with("tool.") || lower.contains("execute_tool") || lower.contains("tool_call") {
        return Some("execute_tool".to_owned());
    }
    None
}

/// Pull the `stringValue` out of an OTLP `AnyValue`, if it is one.
fn string_value(v: &opentelemetry_proto::tonic::common::v1::AnyValue) -> Option<String> {
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    match v.value.as_ref()? {
        Value::StringValue(s) => Some(s.clone()),
        _ => None,
    }
}

/// Ingest worker-emitted OTel spans: re-parent every span under BOI's
/// `invoke_agent` span (`parent`) and normalize provider-specific span names to
/// §8's GenAI-semconv `chat`/`execute_tool`.
///
/// `worker_spans` is a canonical OTLP/JSON file (one `ExportTraceServiceRequest`
/// per line) the worker subprocess emitted. The re-stamped spans are appended
/// to `<out_dir>/{date}/{boi_trace_id}.jsonl`. `phase_run_id` is recorded on
/// every hoovered span as a `boi.phase_run_id` attribute (correlation) and is
/// used in error context.
///
/// Best-effort, loud-on-failure (SO S6): an unreadable or malformed worker file
/// returns a descriptive [`OtelError`] — the caller logs it `warn!` (the
/// worker's own work already succeeded; lost telemetry must not fail a task).
pub fn hoover_worker_spans(
    phase_run_id: &crate::types::ids::PhaseRunId,
    parent: &BoiSpanRef,
    worker_spans: &Path,
    out_dir: &Path,
) -> Result<(), OtelError> {
    let raw =
        std::fs::read_to_string(worker_spans).map_err(|source| OtelError::ReadWorkerSpans {
            path: worker_spans.to_path_buf(),
            source,
        })?;

    let mut requests: Vec<ExportTraceServiceRequest> = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut request: ExportTraceServiceRequest =
            serde_json::from_str(line).map_err(|source| OtelError::MalformedWorkerSpans {
                path: worker_spans.to_path_buf(),
                line: idx + 1,
                source,
            })?;
        reparent_request(&mut request, phase_run_id, parent);
        requests.push(request);
    }

    if requests.is_empty() {
        return Ok(());
    }

    // Append the re-stamped spans to BOI's own trace file for this trace id.
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let day_dir = out_dir.join(&date);
    std::fs::create_dir_all(&day_dir).map_err(|source| OtelError::WriteWorkerSpans {
        path: day_dir.clone(),
        source,
    })?;
    let file_path = day_dir.join(format!("{}.jsonl", trace_id_hex(&parent.trace_id)));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_path)
        .map_err(|source| OtelError::WriteWorkerSpans {
            path: file_path.clone(),
            source,
        })?;
    for request in &requests {
        let line = serde_json::to_string(request).map_err(|source| {
            // A re-serialization failure of typed proto is not an I/O error,
            // but `OtelError`'s write variant is the closest carrier.
            OtelError::WriteWorkerSpans {
                path: file_path.clone(),
                source: std::io::Error::other(source),
            }
        })?;
        writeln!(file, "{line}").map_err(|source| OtelError::WriteWorkerSpans {
            path: file_path.clone(),
            source,
        })?;
    }
    Ok(())
}

/// Re-stamp every span in one request: BOI's trace id, the re-parenting rule,
/// the normalized name, the `boi.phase_run_id` correlation attribute.
fn reparent_request(
    request: &mut ExportTraceServiceRequest,
    phase_run_id: &crate::types::ids::PhaseRunId,
    parent: &BoiSpanRef,
) {
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value::Value};

    for resource_spans in &mut request.resource_spans {
        for scope_spans in &mut resource_spans.scope_spans {
            for span in &mut scope_spans.spans {
                // (1) Re-stamp the trace id — the worker span joins BOI's trace.
                span.trace_id = parent.trace_id.to_vec();
                // (1b) Re-parent: a worker-trace ROOT span (empty/all-zero
                // parent) becomes a child of BOI's `invoke_agent` span. A
                // non-root worker span keeps its parent — the intra-worker
                // nesting is preserved, and that parent is itself now re-traced.
                if is_root_parent(&span.parent_span_id) {
                    span.parent_span_id = parent.span_id.to_vec();
                }
                // (2) Normalize the span name to a §8 GenAI-semconv name.
                if let Some(name) = normalized_name(span) {
                    span.name = name;
                }
                // Correlation: tie every hoovered span to the BOI phase run.
                span.attributes.push(KeyValue {
                    key: "boi.phase_run_id".to_owned(),
                    value: Some(AnyValue {
                        value: Some(Value::StringValue(phase_run_id.as_str().to_owned())),
                    }),
                });
            }
        }
    }
}

/// Whether an OTLP `parent_span_id` denotes a root span — empty, or the 8-byte
/// all-zero id OTLP/JSON uses for "no parent".
fn is_root_parent(parent_span_id: &[u8]) -> bool {
    parent_span_id.is_empty() || parent_span_id.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::PhaseRunId;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory removed on drop — the `runtime/` test convention.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-hoover-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    fn phase_run() -> PhaseRunId {
        PhaseRunId::new("P0000001a").unwrap()
    }

    /// A representative BOI `invoke_agent` parent.
    fn boi_parent() -> BoiSpanRef {
        BoiSpanRef {
            trace_id: [
                0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb,
                0xbb, 0xbb,
            ],
            span_id: [0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc],
        }
    }

    /// The committed worker-spans fixture path.
    fn worker_fixture() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/traces/worker_spans.jsonl")
    }

    /// Parse every span out of the (single) hoovered output file.
    fn read_hoovered(out_dir: &Path) -> Vec<serde_json::Value> {
        let mut spans = Vec::new();
        for date_dir in std::fs::read_dir(out_dir).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(date_dir.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let content = std::fs::read_to_string(f.path()).expect("read");
                for line in content.lines() {
                    let req: serde_json::Value = serde_json::from_str(line).expect("otlp/json");
                    for rs in req["resourceSpans"].as_array().into_iter().flatten() {
                        for ss in rs["scopeSpans"].as_array().into_iter().flatten() {
                            for s in ss["spans"].as_array().into_iter().flatten() {
                                spans.push(s.clone());
                            }
                        }
                    }
                }
            }
        }
        spans
    }

    /// After the hoover, the worker spans are children of BOI's `invoke_agent`
    /// span (matching `trace_id`) with normalized `chat`/`execute_tool` names.
    #[test]
    fn test_l2_hoover_reparents_and_normalizes_worker_spans() {
        let tmp = TempDir::new("reparent");
        let out_dir = tmp.path.join("traces");
        let parent = boi_parent();

        hoover_worker_spans(&phase_run(), &parent, &worker_fixture(), &out_dir)
            .expect("hoover the worker fixture");

        let spans = read_hoovered(&out_dir);
        assert_eq!(spans.len(), 3, "all three worker spans were hoovered");

        let boi_trace = trace_id_hex(&parent.trace_id);
        let boi_span = parent
            .span_id
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        for span in &spans {
            // (1) Every span now carries BOI's trace id.
            assert_eq!(
                span["traceId"].as_str().unwrap(),
                boi_trace,
                "span re-stamped with BOI's trace id"
            );
        }

        // (1b) The worker-trace ROOT span (`claude_code.session`, no parent in
        // the fixture) is re-parented onto BOI's `invoke_agent` span id.
        // Its name is not a chat/tool name, so it keeps it.
        let root = spans
            .iter()
            .find(|s| s["name"] == "claude_code.session")
            .expect("the worker session span survived, name unchanged");
        assert_eq!(
            root["parentSpanId"].as_str().unwrap(),
            boi_span,
            "worker root re-parented under BOI invoke_agent"
        );

        // (2) The worker LLM span normalized to `chat`.
        let chat = spans
            .iter()
            .find(|s| s["name"] == "chat")
            .expect("the worker chat span was renamed to `chat`");
        // A non-root worker span keeps its (worker-local) parent.
        assert_eq!(
            chat["parentSpanId"].as_str().unwrap(),
            "1111111111111111",
            "non-root worker span keeps its intra-worker parent"
        );

        // (2) The worker tool span normalized to `execute_tool`.
        assert!(
            spans.iter().any(|s| s["name"] == "execute_tool"),
            "the worker tool span was renamed to `execute_tool`"
        );
        // The provider-specific names are gone.
        assert!(
            !spans
                .iter()
                .any(|s| s["name"] == "claude_code.chat" || s["name"] == "tool.Bash"),
            "provider-specific span names normalized away"
        );
    }

    /// Every hoovered span carries the `boi.phase_run_id` correlation attribute.
    #[test]
    fn test_l2_hoover_tags_spans_with_phase_run_id() {
        let tmp = TempDir::new("correlate");
        let out_dir = tmp.path.join("traces");
        hoover_worker_spans(&phase_run(), &boi_parent(), &worker_fixture(), &out_dir)
            .expect("hoover");
        for span in read_hoovered(&out_dir) {
            let attrs = span["attributes"].as_array().unwrap();
            let prid = attrs
                .iter()
                .find(|kv| kv["key"] == "boi.phase_run_id")
                .expect("every hoovered span carries boi.phase_run_id");
            assert_eq!(prid["value"]["stringValue"], "P0000001a");
        }
    }

    /// The hoovered output is canonical OTLP/JSON — it round-trips through the
    /// typed `ExportTraceServiceRequest` (the Phase 8c `read_otlp_traces`
    /// contract).
    #[test]
    fn test_l2_hoovered_output_is_canonical_otlp_json() {
        let tmp = TempDir::new("canonical");
        let out_dir = tmp.path.join("traces");
        hoover_worker_spans(&phase_run(), &boi_parent(), &worker_fixture(), &out_dir)
            .expect("hoover");
        let mut lines = 0;
        for date_dir in std::fs::read_dir(&out_dir).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(date_dir.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                for line in std::fs::read_to_string(f.path()).expect("read").lines() {
                    let _req: ExportTraceServiceRequest = serde_json::from_str(line)
                        .expect("hoovered line is a canonical ExportTraceServiceRequest");
                    lines += 1;
                }
            }
        }
        assert!(lines > 0, "the hoover wrote canonical OTLP/JSON lines");
    }

    /// A missing worker-spans file is a loud, descriptive error (SO S6) — not a
    /// silent skip.
    #[test]
    fn test_l2_hoover_missing_worker_file_errors_loudly() {
        let tmp = TempDir::new("missing");
        let out_dir = tmp.path.join("traces");
        let absent = tmp.path.join("no-such-worker-spans.jsonl");
        let err = hoover_worker_spans(&phase_run(), &boi_parent(), &absent, &out_dir)
            .expect_err("a missing worker file must error");
        assert!(
            matches!(err, OtelError::ReadWorkerSpans { .. }),
            "error names the unreadable file: {err}"
        );
    }

    /// A malformed worker-spans line is a loud, line-numbered error.
    #[test]
    fn test_l2_hoover_malformed_worker_line_errors_loudly() {
        let tmp = TempDir::new("malformed");
        let out_dir = tmp.path.join("traces");
        let bad = tmp.path.join("bad.jsonl");
        std::fs::write(&bad, "{not valid otlp json}\n").expect("write bad fixture");
        let err = hoover_worker_spans(&phase_run(), &boi_parent(), &bad, &out_dir)
            .expect_err("a malformed worker line must error");
        match err {
            OtelError::MalformedWorkerSpans { line, .. } => {
                assert_eq!(line, 1, "the error names the offending line");
            }
            other => panic!("expected MalformedWorkerSpans, got {other}"),
        }
    }
}
