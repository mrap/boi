//! The OTLP/JSON file-exporter half of the Phase 8a OTel stack (Task 8a.1).
//!
//! [`init_tracing`] stands up the OpenTelemetry SDK pipeline and returns an
//! [`OtelGuard`] — the daemon holds the guard for its lifetime and drops it
//! explicitly at shutdown ([`OtelGuard`]'s `Drop` force-flushes, review S13).
//!
//! ## Canonical OTLP/JSON, by construction (review C5)
//!
//! The wire format is **canonical OTLP/JSON**: every line of a
//! `~/.boi/v2/traces/{date}/{trace_id}.jsonl` file is one
//! `ExportTraceServiceRequest` — `{"resourceSpans":[...]}`. It is *not*
//! hand-rolled. The `JsonFileExporter` (this module's `SpanExporter` impl) runs
//! the SDK `SpanData` through
//! `opentelemetry_proto`'s `group_spans_by_resource_and_scope` transform — the
//! identical transform `opentelemetry-otlp` uses — then `serde_json`-serializes
//! the resulting `ExportTraceServiceRequest`. `opentelemetry-proto`'s `serde`
//! derives carry `rename_all = "camelCase"`, so the field names come out
//! `resourceSpans` / `traceId` / `startTimeUnixNano` / `stringValue` — exactly
//! what DuckDB's `read_otlp_traces` (Phase 8c) parses. The shape is correct
//! because every encoding step is the OTel project's own code; BOI never writes
//! a brace.
//!
//! ## Why a `SimpleSpanProcessor`, not a `BatchSpanProcessor`
//!
//! Design §8 says "typically batch"; this file deliberately uses a
//! `SimpleSpanProcessor`. A
//! `BatchSpanProcessor` drains on a background `rt-tokio` task — and S13's
//! footgun is precisely a guard dropped after the tokio runtime is gone,
//! silently losing the final batch. `SimpleSpanProcessor` exports synchronously
//! the instant a span ends: a file append, no background task, no
//! runtime-liveness dependency. `OtelGuard::drop` then has nothing to lose —
//! `force_flush` is already a no-op and `shutdown` is well-defined whatever
//! state the runtime is in. (Plan 8a.1 deviation — documented in the commit.)

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use opentelemetry::trace::{TraceError, TracerProvider as _};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};
use opentelemetry_sdk::trace::{SimpleSpanProcessor, Tracer, TracerProvider};

/// The OpenTelemetry instrumentation-scope + `service.name` BOI emits under.
///
/// Shared by [`init_tracing`] (the `Resource` / scope) and Phase 8a.2's
/// [`OtelObserver`](crate::runtime::otel) (the tracer name) — one const so the
/// `service.name` a query filters on cannot drift from the one the SDK writes.
pub const SERVICE_NAME: &str = "boi";

/// An [`init_tracing`] failure.
///
/// Loud by construction (SO S6): every variant carries the offending path or
/// the SDK error string. [`init_tracing`] is called once at daemon boot — a
/// failure here aborts boot rather than running the daemon blind.
#[derive(Debug, thiserror::Error)]
pub enum OtelError {
    /// The traces directory could not be created.
    #[error("could not create traces dir {path}: {source}")]
    TracesDir {
        /// The directory `init_tracing` tried to create.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A worker-OTel file could not be read (Phase 8a.3 hoover).
    #[error("could not read worker-otel file {path}: {source}")]
    ReadWorkerSpans {
        /// The worker-emitted OTel file the hoover tried to read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A worker-OTel file held a line that is not canonical OTLP/JSON.
    #[error("worker-otel file {path} line {line} is not canonical OTLP/JSON: {source}")]
    MalformedWorkerSpans {
        /// The worker-emitted OTel file.
        path: PathBuf,
        /// The 1-based line number that failed to parse.
        line: usize,
        /// The underlying deserialization error.
        source: serde_json::Error,
    },
    /// The re-parented worker spans could not be written back.
    #[error("could not write hoovered spans to {path}: {source}")]
    WriteWorkerSpans {
        /// The destination trace file.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// The SDK `SpanExporter` BOI wires: each batch is grouped by `trace_id` and
/// each group is appended as one `ExportTraceServiceRequest` JSONL line to
/// `<traces_dir>/<date>/<trace_id>.jsonl`.
///
/// `SpanExporter::export` takes `&mut self` and the SDK contract guarantees it
/// is never called concurrently for one instance — but the trait still demands
/// `Sync`, so the per-export state (`resource`) sits behind a `Mutex` that is
/// only ever uncontended. No `.await` is held across the lock.
struct JsonFileExporter {
    /// `<HOME>/.boi/v2/traces` (or a tempdir, in tests).
    traces_dir: PathBuf,
    /// The process `Resource` — set once by the SDK via `set_resource`, read on
    /// every `export` to build each `ResourceSpans`.
    resource: Mutex<ResourceAttributesWithSchema>,
}

impl std::fmt::Debug for JsonFileExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonFileExporter")
            .field("traces_dir", &self.traces_dir)
            .finish()
    }
}

impl JsonFileExporter {
    /// Append every `trace_id` group in `batch` to its per-trace JSONL file.
    ///
    /// Pulled out of `export` so the `&mut self`/`BoxFuture` trait shape does
    /// not entangle the actual I/O — this is plain synchronous code returning a
    /// `Result`, trivially unit-testable.
    fn write_batch(&self, batch: Vec<SpanData>) -> Result<(), TraceError> {
        if batch.is_empty() {
            return Ok(());
        }
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let day_dir = self.traces_dir.join(&date);
        std::fs::create_dir_all(&day_dir)
            .map_err(|e| TraceError::Other(Box::new(io_err(&day_dir, e))))?;

        // Group the batch by trace_id — one batch can carry spans from several
        // traces, and each trace gets its own file.
        let mut by_trace: std::collections::HashMap<String, Vec<SpanData>> =
            std::collections::HashMap::new();
        for span in batch {
            let trace_id = span.span_context.trace_id().to_string();
            by_trace.entry(trace_id).or_default().push(span);
        }

        let resource = self
            .resource
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (trace_id, spans) in by_trace {
            // The standard `opentelemetry-proto` transform — the same one
            // `opentelemetry-otlp` uses. Produces canonical OTLP `ResourceSpans`.
            let resource_spans = group_spans_by_resource_and_scope(spans, &resource);
            let request = ExportTraceServiceRequest { resource_spans };
            // `serde` with `rename_all = "camelCase"` → canonical OTLP/JSON.
            let line =
                serde_json::to_string(&request).map_err(|e| TraceError::Other(Box::new(e)))?;

            let file_path = day_dir.join(format!("{trace_id}.jsonl"));
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file_path)
                .map_err(|e| TraceError::Other(Box::new(io_err(&file_path, e))))?;
            writeln!(file, "{line}")
                .map_err(|e| TraceError::Other(Box::new(io_err(&file_path, e))))?;
        }
        Ok(())
    }
}

/// Wrap an I/O error with the path it concerns — `TraceError::Other` only takes
/// an opaque `Box<dyn Error>`, so the path has to be folded into the message.
fn io_err(path: &Path, source: std::io::Error) -> std::io::Error {
    std::io::Error::new(source.kind(), format!("{}: {source}", path.display()))
}

impl SpanExporter for JsonFileExporter {
    fn export(
        &mut self,
        batch: Vec<SpanData>,
    ) -> futures::future::BoxFuture<'static, ExportResult> {
        // `SimpleSpanProcessor` calls this on the span-end thread; the work is
        // a synchronous file append. Resolve immediately — no task spawned.
        let result = self.write_batch(batch);
        Box::pin(std::future::ready(result))
    }

    fn set_resource(&mut self, resource: &Resource) {
        let mut slot = self
            .resource
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = ResourceAttributesWithSchema::from(resource);
    }
}

/// A live OpenTelemetry tracing pipeline — held by the daemon for its lifetime.
///
/// Dropping the guard force-flushes any in-flight spans and shuts the provider
/// down (review S13). The daemon drops it **explicitly** at the end of its
/// shutdown sequence; relying on scope-exit drop ordering risks the provider
/// outliving — or being outlived by — the tokio runtime.
///
/// Construct via [`init_tracing`]; clone a [`tracer`](OtelGuard::tracer) for
/// the [`OtelObserver`](crate::runtime::otel).
pub struct OtelGuard {
    provider: TracerProvider,
}

impl std::fmt::Debug for OtelGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OtelGuard")
    }
}

impl OtelGuard {
    /// A [`Tracer`] on this provider — the [`OtelObserver`](crate::runtime::otel)
    /// holds one to open/close spans. Cheap to clone.
    pub fn tracer(&self) -> Tracer {
        self.provider.tracer(SERVICE_NAME)
    }
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        // Force-flush first (a no-op under `SimpleSpanProcessor`, but correct
        // if a future maintainer swaps in a batch processor), then shut down.
        for result in self.provider.force_flush() {
            if let Err(e) = result {
                tracing::warn!(error = %e, "OTel force-flush failed during shutdown");
            }
        }
        if let Err(e) = self.provider.shutdown() {
            tracing::warn!(error = %e, "OTel provider shutdown failed");
        }
    }
}

/// Initialize the OTel pipeline: a standard `SimpleSpanProcessor` feeding the
/// canonical-OTLP/JSON file exporter, writing to
/// `<traces_dir>/{date}/{trace_id}.jsonl`. Called once at daemon boot.
///
/// `traces_dir` is created if absent. The returned [`OtelGuard`] owns the
/// provider — keep it alive for the daemon's lifetime and drop it last.
pub fn init_tracing(traces_dir: &Path) -> Result<OtelGuard, OtelError> {
    std::fs::create_dir_all(traces_dir).map_err(|source| OtelError::TracesDir {
        path: traces_dir.to_path_buf(),
        source,
    })?;

    let exporter = JsonFileExporter {
        traces_dir: traces_dir.to_path_buf(),
        resource: Mutex::new(ResourceAttributesWithSchema::default()),
    };

    let provider = TracerProvider::builder()
        .with_resource(Resource::new(vec![opentelemetry::KeyValue::new(
            "service.name",
            SERVICE_NAME,
        )]))
        .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter)))
        .build();

    Ok(OtelGuard { provider })
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{Span, Tracer as _};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory removed on drop — `std`-only, matching the
    /// `runtime/` test convention (`goose.rs`).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-otel-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    /// Read every JSONL file under the (single) date directory of `traces_dir`.
    fn read_all_trace_files(traces_dir: &Path) -> Vec<(PathBuf, String)> {
        let mut out = Vec::new();
        let Ok(date_dirs) = std::fs::read_dir(traces_dir) else {
            return out;
        };
        for date_dir in date_dirs.flatten() {
            let Ok(files) = std::fs::read_dir(date_dir.path()) else {
                continue;
            };
            for f in files.flatten() {
                let content = std::fs::read_to_string(f.path()).expect("read trace file");
                out.push((f.path(), content));
            }
        }
        out
    }

    /// `init_tracing` over a fresh tempdir; emitting one span; dropping the
    /// guard flushes; the JSONL is canonical OTLP/JSON shape-matching the
    /// committed `tests/fixtures/traces/sample_otlp.jsonl` fixture.
    #[test]
    fn test_l2_emit_one_span_writes_canonical_otlp_json() {
        let tmp = TempDir::new("emit-one");
        let traces_dir = tmp.path.join("traces");

        {
            let guard = init_tracing(&traces_dir).expect("init_tracing");
            let tracer = guard.tracer();
            let mut span = tracer.start("invoke_workflow boi.spec");
            span.set_attribute(opentelemetry::KeyValue::new("boi.spec_id", "S0000001a"));
            span.end();
            // Dropping `guard` here force-flushes + shuts down. With a
            // `SimpleSpanProcessor` the span is already on disk, but the drop
            // must not panic and the file must survive it.
        }

        let files = read_all_trace_files(&traces_dir);
        assert_eq!(files.len(), 1, "exactly one trace file written");
        let (path, content) = &files[0];
        assert!(
            path.extension().is_some_and(|e| e == "jsonl"),
            "trace file is .jsonl: {}",
            path.display()
        );

        // Each line is one canonical `ExportTraceServiceRequest`.
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1, "one span → one JSONL line");
        let req: ExportTraceServiceRequest =
            serde_json::from_str(lines[0]).expect("line is a canonical ExportTraceServiceRequest");
        assert_eq!(req.resource_spans.len(), 1, "one ResourceSpans");

        // Shape-compatibility against the committed cross-phase fixture: the
        // same `serde` round-trip the Phase 8c query tests rely on succeeds for
        // both the emitted line and every fixture line.
        let fixture = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/traces/sample_otlp.jsonl"),
        )
        .expect("sample_otlp.jsonl fixture present");
        for (i, fl) in fixture.lines().enumerate() {
            let fixture_req: ExportTraceServiceRequest = serde_json::from_str(fl)
                .unwrap_or_else(|e| panic!("fixture line {i} is canonical OTLP/JSON: {e}"));
            assert!(
                !fixture_req.resource_spans.is_empty(),
                "fixture line {i} carries resourceSpans"
            );
        }

        // The emitted span's JSON carries the canonical camelCase keys (i.e. it
        // is genuine OTLP/JSON, not the SDK's debug rendering).
        let raw: serde_json::Value = serde_json::from_str(lines[0]).expect("valid json");
        let span_json = &raw["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span_json["name"], "invoke_workflow boi.spec");
        assert!(
            span_json["startTimeUnixNano"].is_string(),
            "startTimeUnixNano is a string-encoded nano (OTLP/JSON convention)"
        );
        assert!(span_json["traceId"].is_string(), "traceId present");
    }

    /// One export batch carrying two distinct `trace_id`s lands in two separate
    /// per-trace files (the `<trace_id>.jsonl` routing).
    #[test]
    fn test_l2_distinct_traces_route_to_distinct_files() {
        let tmp = TempDir::new("two-traces");
        let traces_dir = tmp.path.join("traces");
        {
            let guard = init_tracing(&traces_dir).expect("init_tracing");
            let tracer = guard.tracer();
            // Two independent root spans → two trace ids.
            tracer.start("invoke_workflow boi.spec").end();
            tracer.start("invoke_workflow boi.spec").end();
        }
        let files = read_all_trace_files(&traces_dir);
        assert_eq!(files.len(), 2, "two traces → two files");
        for (path, content) in &files {
            let stem = path.file_stem().unwrap().to_string_lossy().to_string();
            let req: ExportTraceServiceRequest =
                serde_json::from_str(content.trim()).expect("canonical OTLP/JSON");
            let span = &req.resource_spans[0].scope_spans[0].spans[0];
            // The file is named for the trace it holds.
            let trace_hex: String = span.trace_id.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(trace_hex, stem, "file stem == its trace_id");
        }
    }

    /// `init_tracing` creates `traces_dir` when it does not yet exist.
    #[test]
    fn test_l2_init_tracing_creates_missing_dir() {
        let tmp = TempDir::new("mkdir");
        let traces_dir = tmp.path.join("does/not/exist/traces");
        assert!(!traces_dir.exists());
        let guard = init_tracing(&traces_dir).expect("init_tracing creates the dir");
        assert!(traces_dir.is_dir(), "traces_dir created");
        drop(guard);
    }

    /// Dropping the guard with an un-ended span still in flight does not panic —
    /// the force-flush/shutdown path is exercised on a non-empty provider.
    #[test]
    fn test_l2_guard_drop_force_flushes_without_panic() {
        let tmp = TempDir::new("drop-flush");
        let traces_dir = tmp.path.join("traces");
        let guard = init_tracing(&traces_dir).expect("init_tracing");
        let tracer = guard.tracer();
        let _live = tracer.start("invoke_agent boi.worker");
        // `_live` is dropped here (ends the span → flush), then `guard` drops
        // (force_flush + shutdown). Neither may panic.
        drop(_live);
        drop(guard);
        let files = read_all_trace_files(&traces_dir);
        assert_eq!(files.len(), 1, "the in-flight span flushed on drop");
    }
}
