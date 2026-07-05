//! The `OtelObserver` — the Phase 4 `EmitObserver` adapter (Task 8a.2).
//!
//! [`OtelObserver`] is the wired emit-Phase-2 observer: every `BoiEvent` the
//! bus emits is forwarded here and turned into an OpenTelemetry span lifecycle
//! event or span event, on top of the canonical-OTLP/JSON file exporter
//! ([`super::otel_export`]).
//!
//! ## Span hierarchy (design §8)
//!
//! ```text
//! invoke_workflow boi.spec        — one per spec   (keyed by SpecId)
//! └── invoke_agent boi.worker     — one per phase  (keyed by PhaseRunId)
//! ```
//!
//! `SpecStarted` opens the root; `SpecCompleted`/`SpecFailed`/`SpecCanceled`
//! end it. `PhaseStarted` opens the `invoke_agent` child (re-parented under the
//! spec root); `PhaseCompleted` ends it. The selected observational variants
//! attach a span *event* — `DecisionMade` → `boi.decision_recorded`,
//! `ErrorEncountered` → `boi.error`, `VerifyChecked` → `boi.verify`,
//! `ToolInvoked` → `execute_tool`, `ReportReceived` → `boi.task_reported`.
//!
//! ## Best-effort (emit-Phase 2)
//!
//! `observe` returning `Err` never aborts the bus's `emit` — the bus logs it
//! `warn!` and proceeds (design §2 / Phase 4 emit-Phase 2). The only `Err` an
//! `OtelObserver` raises is a registry-lock poison, which means another thread
//! panicked mid-`observe`; surfacing it `warn!` is the loud-failure contract.
//!
//! ## Registry discipline
//!
//! `spec_spans`/`phase_spans` are `std::sync::Mutex` (not tokio's) — no `.await`
//! is held across either guard, so a blocking mutex is correct and cheaper.
//! Every `SpecId`/`PhaseRunId` inserted is **removed on its terminal event**
//! (`Spec{Completed,Failed,Canceled}` / `PhaseCompleted`): the registries never
//! leak. `_assert_send_sync` (below) holds the type to the bus's `Arc<dyn
//! EmitObserver>` requirement at compile time.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use opentelemetry::trace::{Span as _, SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{Span, Tracer};

use crate::service::{EmitObserver, ObserverError};
use crate::types::event::BoiEvent;
use crate::types::ids::{PhaseRunId, SpecId};

/// The failure-fingerprint span-attribute key.
///
/// **Shared with Phase 8c** (review item 30): Task 8a.2 sets this attribute on
/// the `boi.error` span event, and Phase 8c's `boi failures top` query
/// `GROUP BY`s the same column. One `pub const` so the emit key and the query
/// key cannot drift. Design §8 erratum G15 standardized on `boi.failure_*`
/// (the §8 span-event table had also called it `error.fingerprint`).
pub const FAILURE_FINGERPRINT_ATTR: &str = "boi.failure_fingerprint";

/// The OTel `EmitObserver` — `BoiEvent` → spans (design §8).
///
/// Construct with [`OtelObserver::new`], handing it a [`Tracer`] cloned from
/// the [`OtelGuard`](super::otel_export::OtelGuard) that
/// [`init_tracing`](super::otel_export::init_tracing) returns. The bus holds it
/// as one `Arc<dyn EmitObserver>` (Phase 4 / Phase 9 `boot`).
pub struct OtelObserver {
    /// The tracer that mints every span — cloned from the process `OtelGuard`.
    tracer: Tracer,
    /// Open `invoke_workflow boi.spec` root spans, by spec. Inserted on
    /// `SpecStarted`, removed on the spec's terminal event.
    spec_spans: Mutex<HashMap<SpecId, Span>>,
    /// Open `invoke_agent boi.worker` spans, by phase run. Inserted on
    /// `PhaseStarted`, removed on `PhaseCompleted`. The value carries the
    /// span's `spec_id` + `phase` so a span-event that knows only
    /// `(spec_id, phase)` — not a `phase_run_id` — can still resolve the phase
    /// span (G24.1 — see [`OtelObserver::add_error_event`]).
    phase_spans: Mutex<HashMap<PhaseRunId, PhaseSpan>>,
}

/// An open `invoke_agent boi.worker` span plus the identity needed to resolve
/// it from a span-event that carries `(spec_id, phase)` but no `phase_run_id`.
struct PhaseSpan {
    /// The OTel span itself.
    span: Span,
    /// The spec the phase run belongs to.
    spec_id: SpecId,
    /// The phase name (`execute`, `review`, …).
    phase: String,
}

impl std::fmt::Debug for OtelObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OtelObserver")
    }
}

// The bus stores observers as `Arc<dyn EmitObserver>` — `EmitObserver: Send +
// Sync`. A future non-`Send`/non-`Sync` field fails the build here, at the
// type, rather than at a distant `Arc::new` call site (mirrors `EventBus`'s
// `_assert_send_sync`, review S1).
const _: () = {
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _check() {
        _assert_send_sync::<OtelObserver>();
    }
};

impl OtelObserver {
    /// Build an observer over `tracer` — clone it from the process
    /// [`OtelGuard`](super::otel_export::OtelGuard).
    pub fn new(tracer: Tracer) -> Self {
        OtelObserver {
            tracer,
            spec_spans: Mutex::new(HashMap::new()),
            phase_spans: Mutex::new(HashMap::new()),
        }
    }

    /// Lock a registry, recovering the map even if a prior `observe` panicked
    /// while holding it — a poisoned lock is logged by the caller, not fatal.
    fn lock<'a, K, V>(m: &'a Mutex<HashMap<K, V>>) -> std::sync::MutexGuard<'a, HashMap<K, V>> {
        m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Resolve the span a span-*event* attaches to: the phase span when a
    /// `PhaseRunId` is known, else the spec root span.
    ///
    /// Several observational `BoiEvent`s (`ErrorEncountered`, `ToolInvoked`,
    /// `VerifyChecked`, `ReportReceived`) carry only `spec_id`/`task_id`, not a
    /// `phase_run_id` — design §8 says "the active phase span" but there is no
    /// `(spec,task) → phase_run` lookup (a task has many phase runs). The spec
    /// root span is always resolvable and is the fallback target; the event
    /// still lands in the right spec's trace. `DecisionMade` does carry an
    /// (optional) `phase_run_id` and uses the phase span when present.
    ///
    /// `event_name` is the human label used only for the `warn!` when nothing
    /// resolves — a dropped span event is a quiet failure (G24.1 / C-rt-S4).
    fn add_event_to_span(
        &self,
        phase_run_id: Option<&PhaseRunId>,
        spec_id: &SpecId,
        name: &'static str,
        attributes: Vec<KeyValue>,
    ) {
        if let Some(prid) = phase_run_id {
            let mut phases = Self::lock(&self.phase_spans);
            if let Some(entry) = phases.get_mut(prid) {
                entry.span.add_event(name, attributes);
                return;
            }
        }
        let mut specs = Self::lock(&self.spec_spans);
        if let Some(span) = specs.get_mut(spec_id) {
            span.add_event(name, attributes);
            return;
        }
        // Neither span is open — the event predates `SpecStarted` or postdates
        // the spec's terminal event. Nothing to attach to. The DB remains the
        // source of truth (design §8); span events are forwards. A `warn!`
        // here would be noisy for the common observational variants (they can
        // legitimately race the spec root's close), so the loud-on-drop
        // contract is enforced specifically for `ErrorEncountered` — see
        // `add_error_event` (G24.1 / C-rt-S4).
        tracing::debug!(
            event = name, spec = %spec_id,
            "OTel span event dropped — no open span to attach it to",
        );
    }

    /// Attach an `ErrorEncountered`'s `boi.error` span event, resolving the
    /// **phase span by `(spec_id, phase)`** so the error lands on the
    /// `invoke_agent` span — not the spec root (G24.1 / C-rt-S4).
    ///
    /// `ErrorEncountered` carries `phase` (R1 added the field) but no
    /// `phase_run_id`; `phase_spans` is keyed by `PhaseRunId`, so the open
    /// phase span is found by scanning for the entry whose `(spec_id, phase)`
    /// matches. Falls back to the spec root span when no phase span is open
    /// (a spec-level error, or an error between phases), and — because a
    /// dropped *error* event is a genuine quiet failure — `warn!`s loudly if
    /// even the root span is gone.
    fn add_error_event(
        &self,
        spec_id: &SpecId,
        phase: &str,
        name: &'static str,
        attributes: Vec<KeyValue>,
    ) {
        // 1. The phase span, resolved by (spec_id, phase) — the G24.1 fix: a
        //    `boi.error` event lands on `invoke_agent`, not `invoke_workflow`.
        {
            let mut phases = Self::lock(&self.phase_spans);
            if let Some(entry) = phases
                .values_mut()
                .find(|e| &e.spec_id == spec_id && e.phase == phase)
            {
                entry.span.add_event(name, attributes);
                return;
            }
        }
        // 2. Fallback — the spec root span (a spec-level error, or one raised
        //    when no phase span is open).
        {
            let mut specs = Self::lock(&self.spec_spans);
            if let Some(span) = specs.get_mut(spec_id) {
                span.add_event(name, attributes);
                return;
            }
        }
        // 3. Nothing resolved — a DROPPED ERROR EVENT. That is a quiet failure
        //    (S6 / C-rt-S4): an error that never reaches a span is invisible
        //    to `boi failures top`. Loud `warn!`, never silent.
        tracing::warn!(
            spec = %spec_id, phase,
            "OTel `boi.error` span event DROPPED — no open phase or spec span \
             to attach it to; the error will not appear in trace-backed \
             failure queries",
        );
    }

    /// The [`BoiSpanRef`](super::otel_hoover::BoiSpanRef) of the open
    /// `invoke_agent` span for `phase_run_id` — the parent the
    /// [`hoover_worker_spans`](super::otel_hoover::hoover_worker_spans) hoover
    /// (Task 8a.3) re-parents a worker's spans onto.
    ///
    /// Returns `None` if no phase span is open for that id (the phase has not
    /// started, or already ended). The hoover function lives in its own file
    /// and cannot reach `phase_spans`; this accessor is the bridge — the caller
    /// resolves the ref here and passes it in.
    pub fn phase_span_ref(
        &self,
        phase_run_id: &PhaseRunId,
    ) -> Option<super::otel_hoover::BoiSpanRef> {
        let phases = Self::lock(&self.phase_spans);
        let entry = phases.get(phase_run_id)?;
        let ctx = entry.span.span_context();
        Some(super::otel_hoover::BoiSpanRef {
            trace_id: ctx.trace_id().to_bytes(),
            span_id: ctx.span_id().to_bytes(),
        })
    }
}

#[async_trait]
impl EmitObserver for OtelObserver {
    async fn observe(&self, event: &BoiEvent) -> Result<(), ObserverError> {
        // The whole `match` is synchronous span manipulation under `std::sync`
        // mutexes — no `.await` inside. `observe` is `async` only to satisfy
        // the `dyn`-compatible `EmitObserver` port.
        //
        // NO `_` arm — every `BoiEvent` variant is explicit so adding a variant
        // forces a conscious decision here (review item 29). A no-op arm
        // (`PlanRevised`, the task-lifecycle five) is a *decision*, not a gap.
        match event {
            // ---- Spec lifecycle → the `invoke_workflow boi.spec` root span ----
            BoiEvent::SpecStarted { spec_id } => {
                let span = self
                    .tracer
                    .span_builder("invoke_workflow boi.spec")
                    .with_kind(SpanKind::Internal)
                    .with_attributes(vec![KeyValue::new(
                        "boi.spec_id",
                        spec_id.as_str().to_owned(),
                    )])
                    .start(&self.tracer);
                Self::lock(&self.spec_spans).insert(spec_id.clone(), span);
            }
            BoiEvent::SpecCompleted { spec_id } => {
                if let Some(mut span) = Self::lock(&self.spec_spans).remove(spec_id) {
                    span.set_status(Status::Ok);
                    span.end();
                }
            }
            BoiEvent::SpecFailed { spec_id, reason } => {
                if let Some(mut span) = Self::lock(&self.spec_spans).remove(spec_id) {
                    span.set_status(Status::error(format!("{reason:?}")));
                    span.end();
                }
            }
            BoiEvent::SpecCanceled { spec_id, reason } => {
                if let Some(mut span) = Self::lock(&self.spec_spans).remove(spec_id) {
                    span.set_attribute(KeyValue::new(
                        "boi.cancellation_reason",
                        format!("{reason:?}"),
                    ));
                    span.end();
                }
            }

            // ---- Phase lifecycle → the `invoke_agent boi.worker` child span ----
            BoiEvent::PhaseStarted {
                phase_run_id,
                spec_id,
                task_id,
                phase,
                provider,
                model,
                iteration,
            } => {
                // Re-parent under the spec root span when it is open, so the
                // phase span is a child in the §8 hierarchy. `with_remote_span_
                // context` builds a parent `Context` from the root's
                // `SpanContext` without needing a tokio task-local.
                let parent_cx = {
                    let specs = Self::lock(&self.spec_spans);
                    specs.get(spec_id).map(|root| {
                        Context::current().with_remote_span_context(root.span_context().clone())
                    })
                };
                let mut attributes = vec![
                    KeyValue::new("boi.spec_id", spec_id.as_str().to_owned()),
                    KeyValue::new("boi.phase", phase.clone()),
                    // `boi.iteration` is set (G14.2); `boi.attempt` is NOT —
                    // no `BoiEvent` carries a retry counter (G15).
                    KeyValue::new("boi.iteration", i64::from(*iteration)),
                    KeyValue::new("gen_ai.request.model", model.clone()),
                    KeyValue::new("gen_ai.system", provider.clone()),
                ];
                if let Some(tid) = task_id {
                    attributes.push(KeyValue::new("boi.task_id", tid.as_str().to_owned()));
                }
                let builder = self
                    .tracer
                    .span_builder("invoke_agent boi.worker")
                    .with_kind(SpanKind::Internal)
                    .with_attributes(attributes);
                let span = match parent_cx {
                    Some(cx) => self.tracer.build_with_context(builder, &cx),
                    None => builder.start(&self.tracer),
                };
                // Store the span keyed by `phase_run_id`, carrying `spec_id` +
                // `phase` so an `ErrorEncountered` (which knows only those two)
                // can resolve this phase span (G24.1 — `add_error_event`).
                Self::lock(&self.phase_spans).insert(
                    phase_run_id.clone(),
                    PhaseSpan {
                        span,
                        spec_id: spec_id.clone(),
                        phase: phase.clone(),
                    },
                );
            }
            BoiEvent::PhaseCompleted {
                phase_run_id,
                spec_id: _,
                task_id: _,
                phase: _,
                verdict,
                tokens_in,
                tokens_out,
                duration_ms,
            } => {
                if let Some(PhaseSpan { mut span, .. }) =
                    Self::lock(&self.phase_spans).remove(phase_run_id)
                {
                    // `gen_ai.usage.*` token counts are real — sourced from the
                    // worker stream (Goose's `complete` event / Claude Code's
                    // usage). Per the 2026-06-01 strip-$ directive BOI no
                    // longer attaches a per-phase dollar attribute; tokens
                    // stay as the spend-hint signal.
                    span.set_attribute(KeyValue::new(
                        "gen_ai.usage.input_tokens",
                        i64::try_from(*tokens_in).unwrap_or(i64::MAX),
                    ));
                    span.set_attribute(KeyValue::new(
                        "gen_ai.usage.output_tokens",
                        i64::try_from(*tokens_out).unwrap_or(i64::MAX),
                    ));
                    span.set_attribute(KeyValue::new(
                        "boi.phase.duration_ms",
                        i64::try_from(*duration_ms).unwrap_or(i64::MAX),
                    ));
                    span.set_attribute(KeyValue::new("boi.phase.verdict", verdict_label(verdict)));
                    match verdict_is_passing(verdict) {
                        true => span.set_status(Status::Ok),
                        false => span.set_status(Status::error(verdict.synopsis.clone())),
                    }
                    span.end();
                }
            }

            // ---- Observational variants → span events (design §8 table) ----
            BoiEvent::DecisionMade { decision } => {
                let alternatives = serde_json::to_string(&decision.alternatives)
                    .unwrap_or_else(|_| "[]".to_owned());
                let mut attrs = vec![
                    KeyValue::new("decision.id", decision.id.as_str().to_owned()),
                    KeyValue::new("decision.title", decision.title.clone()),
                    KeyValue::new("decision.summary", decision.summary.clone()),
                    KeyValue::new("decision.rationale", decision.rationale.clone()),
                    KeyValue::new("decision.alternatives", alternatives),
                ];
                if let Some(superseded) = &decision.supersedes {
                    attrs.push(KeyValue::new(
                        "decision.supersedes",
                        superseded.as_str().to_owned(),
                    ));
                }
                self.add_event_to_span(
                    decision.phase_run_id.as_ref(),
                    &decision.spec_id,
                    "boi.decision_recorded",
                    attrs,
                );
            }
            BoiEvent::ReportReceived {
                spec_id,
                task_id: _,
                kind,
                payload,
                blocking,
            } => {
                let attrs = vec![
                    KeyValue::new("report.kind", kind.clone()),
                    KeyValue::new("report.blocking", *blocking),
                    KeyValue::new("report.payload", payload.to_string()),
                ];
                self.add_event_to_span(None, spec_id, "boi.task_reported", attrs);
            }
            BoiEvent::VerifyChecked {
                spec_id,
                task_id: _,
                level,
                command,
                exit_code,
                stdout_excerpt,
            } => {
                let attrs = vec![
                    KeyValue::new("verify.level", level.clone()),
                    KeyValue::new("verify.command", command.clone()),
                    KeyValue::new("verify.exit_code", i64::from(*exit_code)),
                    KeyValue::new("verify.stdout_excerpt", stdout_excerpt.clone()),
                ];
                self.add_event_to_span(None, spec_id, "boi.verify", attrs);
            }
            BoiEvent::ToolInvoked {
                spec_id,
                task_id: _,
                tool,
                args_summary,
                result_summary,
            } => {
                // `execute_tool` is the §8 GenAI-semconv span-event name for a
                // tool call. It is recorded as a span *event* on the active
                // span (the worker subprocess's own OTel supplies the real
                // child `execute_tool` *spans* — the Phase 8a.3 hoover).
                let attrs = vec![
                    KeyValue::new("boi.tool", tool.clone()),
                    KeyValue::new("boi.tool.args_summary", args_summary.clone()),
                    KeyValue::new("boi.tool.result_summary", result_summary.clone()),
                ];
                self.add_event_to_span(None, spec_id, "execute_tool", attrs);
            }
            BoiEvent::ErrorEncountered {
                spec_id,
                task_id: _,
                phase,
                error,
                why,
                fix_proposed,
                fingerprint,
            } => {
                let attrs = vec![
                    // The shared const — the 8a emit key and the 8c GROUP BY
                    // key are one symbol (review item 30).
                    KeyValue::new(FAILURE_FINGERPRINT_ATTR, fingerprint.clone()),
                    // G24.1 — the phase the error occurred in. `boi failures
                    // top`'s PHASE column reads this `boi.phase` attribute off
                    // the `boi.error` span event; without it the column was
                    // empty against real traces.
                    KeyValue::new("boi.phase", phase.clone()),
                    KeyValue::new("error.first_line", error.clone()),
                    KeyValue::new("error.why", why.clone()),
                    KeyValue::new(
                        "error.fix_proposed",
                        fix_proposed.clone().unwrap_or_default(),
                    ),
                ];
                // G24.1 / C-rt-S4 — resolve the PHASE span by `(spec_id,
                // phase)` so `boi.error` lands on `invoke_agent`, not the spec
                // root; `warn!` loudly if it can attach to neither (a dropped
                // error event is a quiet failure).
                self.add_error_event(spec_id, phase, "boi.error", attrs);
            }

            // ---- Explicit no-ops — a decision, not a gap (review item 29) ----
            //
            // `PlanRevised` is a `spec_versions` table write; design §8 lists
            // no span surface for it. The five task-lifecycle transitions
            // (`Task{Started,Blocked,Unblocked,Passed,Canceled}`) drive the
            // `tasks` state machine — §8's hierarchy is spec→phase, with no
            // task-level span; the phase span already carries `boi.task_id`.
            BoiEvent::PlanRevised { .. }
            | BoiEvent::TaskStarted { .. }
            | BoiEvent::TaskBlocked { .. }
            | BoiEvent::TaskUnblocked { .. }
            | BoiEvent::TaskPassed { .. }
            | BoiEvent::TaskCanceled { .. } => {}
        }
        Ok(())
    }
}

/// A short label for a verdict's outcome — the `boi.phase.verdict` attribute.
fn verdict_label(verdict: &crate::types::verdict::WorkerVerdict) -> &'static str {
    use crate::types::verdict::VerdictOutcome;
    match verdict.outcome {
        VerdictOutcome::Passing { .. } => "passing",
        VerdictOutcome::Redo { .. } => "redo",
        VerdictOutcome::Blocked { .. } => "blocked",
        VerdictOutcome::Fail { .. } => "fail",
        VerdictOutcome::Canceled => "canceled",
    }
}

/// Whether a verdict means the phase succeeded — drives the phase span status.
fn verdict_is_passing(verdict: &crate::types::verdict::WorkerVerdict) -> bool {
    matches!(
        verdict.outcome,
        crate::types::verdict::VerdictOutcome::Passing { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::otel_export::init_tracing;
    use crate::types::decision::DecisionRecord;
    use crate::types::ids::{DecisionId, TaskId};
    use crate::types::reasons::{BlockedReason, CancellationReason, FailureReason};
    use crate::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};
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
                std::env::temp_dir().join(format!("boi-otelobs-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    fn spec() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }
    fn phase_run() -> PhaseRunId {
        PhaseRunId::new("P0000001a").unwrap()
    }

    /// All spans of one trace, parsed back from the per-trace JSONL files.
    fn read_spans(traces_dir: &std::path::Path) -> Vec<serde_json::Value> {
        let mut spans = Vec::new();
        let Ok(date_dirs) = std::fs::read_dir(traces_dir) else {
            return spans;
        };
        for date_dir in date_dirs.flatten() {
            let Ok(files) = std::fs::read_dir(date_dir.path()) else {
                continue;
            };
            for f in files.flatten() {
                let content = std::fs::read_to_string(f.path()).expect("read trace file");
                for line in content.lines() {
                    let req: serde_json::Value =
                        serde_json::from_str(line).expect("canonical OTLP/JSON line");
                    for rs in req["resourceSpans"].as_array().into_iter().flatten() {
                        for ss in rs["scopeSpans"].as_array().into_iter().flatten() {
                            for span in ss["spans"].as_array().into_iter().flatten() {
                                spans.push(span.clone());
                            }
                        }
                    }
                }
            }
        }
        spans
    }

    fn phase_started() -> BoiEvent {
        BoiEvent::PhaseStarted {
            phase_run_id: phase_run(),
            spec_id: spec(),
            task_id: Some(task()),
            phase: "execute".into(),
            provider: "claude_code".into(),
            model: "claude-opus-4-7".into(),
            iteration: 3,
        }
    }

    fn phase_completed() -> BoiEvent {
        BoiEvent::PhaseCompleted {
            phase_run_id: phase_run(),
            spec_id: spec(),
            task_id: Some(task()),
            phase: "execute".into(),
            verdict: WorkerVerdict {
                synopsis: "did it".into(),
                outcome: VerdictOutcome::Passing {
                    evidence: Evidence::default(),
                },
            },
            tokens_in: 1200,
            tokens_out: 340,
            duration_ms: 5000,
        }
    }

    /// `SpecStarted` → `SpecCompleted` opens and closes exactly one
    /// `invoke_workflow boi.spec` span, with the spec id attribute.
    #[tokio::test]
    async fn test_l2_spec_lifecycle_opens_and_closes_one_workflow_span() {
        let tmp = TempDir::new("spec-life");
        let traces = tmp.path.join("traces");
        {
            let guard = init_tracing(&traces).expect("init_tracing");
            let obs = OtelObserver::new(guard.tracer());
            obs.observe(&BoiEvent::SpecStarted { spec_id: spec() })
                .await
                .expect("observe SpecStarted");
            // Registry holds the open root span.
            assert_eq!(OtelObserver::lock(&obs.spec_spans).len(), 1);
            obs.observe(&BoiEvent::SpecCompleted { spec_id: spec() })
                .await
                .expect("observe SpecCompleted");
            // Terminal event removed it — no leak.
            assert_eq!(OtelObserver::lock(&obs.spec_spans).len(), 0);
        }
        let spans = read_spans(&traces);
        assert_eq!(spans.len(), 1, "one workflow span");
        assert_eq!(spans[0]["name"], "invoke_workflow boi.spec");
        let attrs = &spans[0]["attributes"];
        let has_spec_id = attrs
            .as_array()
            .unwrap()
            .iter()
            .any(|kv| kv["key"] == "boi.spec_id" && kv["value"]["stringValue"] == "S0000001a");
        assert!(has_spec_id, "workflow span carries boi.spec_id");
    }

    /// `PhaseStarted` → `PhaseCompleted` writes one `invoke_agent boi.worker`
    /// span carrying `boi.iteration` (G14.2) and the token attributes;
    /// `boi.attempt` is absent (G15). (Per the 2026-06-01 strip-$
    /// directive the BOI-computed dollar attribute is gone — tokens stay.)
    #[tokio::test]
    async fn test_l2_phase_lifecycle_writes_agent_span_with_iteration_and_tokens() {
        let tmp = TempDir::new("phase-life");
        let traces = tmp.path.join("traces");
        {
            let guard = init_tracing(&traces).expect("init_tracing");
            let obs = OtelObserver::new(guard.tracer());
            obs.observe(&BoiEvent::SpecStarted { spec_id: spec() })
                .await
                .unwrap();
            obs.observe(&phase_started()).await.unwrap();
            assert_eq!(OtelObserver::lock(&obs.phase_spans).len(), 1);
            obs.observe(&phase_completed()).await.unwrap();
            assert_eq!(
                OtelObserver::lock(&obs.phase_spans).len(),
                0,
                "PhaseCompleted removed the phase span — no leak"
            );
            obs.observe(&BoiEvent::SpecCompleted { spec_id: spec() })
                .await
                .unwrap();
        }
        let spans = read_spans(&traces);
        let agent = spans
            .iter()
            .find(|s| s["name"] == "invoke_agent boi.worker")
            .expect("an invoke_agent span was written");
        let attrs = agent["attributes"].as_array().unwrap();
        let attr = |k: &str| attrs.iter().find(|kv| kv["key"] == k);
        // `boi.iteration` is set, from the G14.2 PhaseStarted field.
        assert_eq!(
            attr("boi.iteration").expect("boi.iteration set")["value"]["intValue"],
            "3"
        );
        // `boi.attempt` is NOT set (G15).
        assert!(
            attr("boi.attempt").is_none(),
            "boi.attempt is dropped (G15)"
        );
        // Token attributes from PhaseCompleted. (Per the 2026-06-01
        // strip-$ directive no BOI-computed dollar attribute is attached.)
        assert_eq!(
            attr("gen_ai.usage.input_tokens").expect("input tokens")["value"]["intValue"],
            "1200"
        );
        assert_eq!(
            attr("gen_ai.usage.output_tokens").expect("output tokens")["value"]["intValue"],
            "340"
        );
    }

    /// The `invoke_agent` span is re-parented under the `invoke_workflow` root
    /// span — same trace, root as parent.
    #[tokio::test]
    async fn test_l2_phase_span_is_child_of_spec_span() {
        let tmp = TempDir::new("hierarchy");
        let traces = tmp.path.join("traces");
        {
            let guard = init_tracing(&traces).expect("init_tracing");
            let obs = OtelObserver::new(guard.tracer());
            obs.observe(&BoiEvent::SpecStarted { spec_id: spec() })
                .await
                .unwrap();
            obs.observe(&phase_started()).await.unwrap();
            obs.observe(&phase_completed()).await.unwrap();
            obs.observe(&BoiEvent::SpecCompleted { spec_id: spec() })
                .await
                .unwrap();
        }
        let spans = read_spans(&traces);
        let root = spans
            .iter()
            .find(|s| s["name"] == "invoke_workflow boi.spec")
            .expect("root span");
        let agent = spans
            .iter()
            .find(|s| s["name"] == "invoke_agent boi.worker")
            .expect("agent span");
        // Same trace.
        assert_eq!(
            root["traceId"], agent["traceId"],
            "phase span shares the spec's trace"
        );
        // The agent span's parent is the root span.
        assert_eq!(
            agent["parentSpanId"], root["spanId"],
            "invoke_agent is a child of invoke_workflow"
        );
    }

    /// `DecisionMade` adds a `boi.decision_recorded` span event.
    #[tokio::test]
    async fn test_l2_decision_made_adds_decision_recorded_event() {
        let tmp = TempDir::new("decision");
        let traces = tmp.path.join("traces");
        {
            let guard = init_tracing(&traces).expect("init_tracing");
            let obs = OtelObserver::new(guard.tracer());
            obs.observe(&BoiEvent::SpecStarted { spec_id: spec() })
                .await
                .unwrap();
            obs.observe(&phase_started()).await.unwrap();
            // A `Runtime`-origin decision — the kind a worker files; it
            // carries a `phase_run_id` (`new_authored` rejects one).
            let decision = DecisionRecord::new_runtime(
                DecisionId::new("D0000001a").unwrap(),
                spec(),
                Some(phase_run()),
                "pick the exporter".into(),
                "use opentelemetry-proto".into(),
                "canonical by construction".into(),
                vec![],
                None,
                chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            )
            .unwrap();
            obs.observe(&BoiEvent::DecisionMade { decision })
                .await
                .expect("observe DecisionMade");
            obs.observe(&phase_completed()).await.unwrap();
            obs.observe(&BoiEvent::SpecCompleted { spec_id: spec() })
                .await
                .unwrap();
        }
        let spans = read_spans(&traces);
        let agent = spans
            .iter()
            .find(|s| s["name"] == "invoke_agent boi.worker")
            .expect("agent span");
        let events = agent["events"].as_array().expect("events array");
        assert!(
            events.iter().any(|e| e["name"] == "boi.decision_recorded"),
            "phase span carries a boi.decision_recorded event"
        );
    }

    /// `ErrorEncountered` adds a `boi.error` span event carrying the shared
    /// `FAILURE_FINGERPRINT_ATTR` key and (G24.1) the `boi.phase` attribute.
    #[tokio::test]
    async fn test_l2_error_encountered_carries_failure_fingerprint() {
        let tmp = TempDir::new("error");
        let traces = tmp.path.join("traces");
        {
            let guard = init_tracing(&traces).expect("init_tracing");
            let obs = OtelObserver::new(guard.tracer());
            obs.observe(&BoiEvent::SpecStarted { spec_id: spec() })
                .await
                .unwrap();
            obs.observe(&BoiEvent::ErrorEncountered {
                spec_id: spec(),
                task_id: Some(task()),
                phase: "execute".into(),
                error: "verify gate failed".into(),
                why: "PATH stripped".into(),
                fix_proposed: Some("export PATH".into()),
                fingerprint: "abc123".into(),
            })
            .await
            .expect("observe ErrorEncountered");
            obs.observe(&BoiEvent::SpecCompleted { spec_id: spec() })
                .await
                .unwrap();
        }
        let spans = read_spans(&traces);
        let root = spans
            .iter()
            .find(|s| s["name"] == "invoke_workflow boi.spec")
            .expect("root span");
        let events = root["events"].as_array().expect("events array");
        let err = events
            .iter()
            .find(|e| e["name"] == "boi.error")
            .expect("a boi.error span event");
        let attrs = err["attributes"].as_array().unwrap();
        let fp = attrs
            .iter()
            .find(|kv| kv["key"] == FAILURE_FINGERPRINT_ATTR)
            .expect("the failure-fingerprint attribute, keyed by the shared const");
        assert_eq!(fp["value"]["stringValue"], "abc123");
        // The const really is `boi.failure_fingerprint` (G15 standardization).
        assert_eq!(FAILURE_FINGERPRINT_ATTR, "boi.failure_fingerprint");
        // G24.1 — the `boi.error` event carries the phase, so `boi failures
        // top`'s PHASE column resolves against real traces.
        let phase = attrs
            .iter()
            .find(|kv| kv["key"] == "boi.phase")
            .expect("the boi.phase attribute (G24.1)");
        assert_eq!(phase["value"]["stringValue"], "execute");
    }

    /// Regression test for C-rt-S4 / G24.1 — an `ErrorEncountered` raised while
    /// a phase span is open lands on the **`invoke_agent` (phase) span**,
    /// resolved by `(spec_id, phase)` — NOT on the spec root span.
    ///
    /// The OLD `add_event_to_span(None, spec_id, "boi.error", …)` passed
    /// `None` for the phase run, so a `boi.error` event always fell back to the
    /// spec root span. R1 added the `phase` field to `ErrorEncountered`; the
    /// fix's `add_error_event` uses it to find the open phase span. This test
    /// opens a phase span (`PhaseStarted` for `execute`), raises an
    /// `ErrorEncountered` for that same phase, and asserts the `boi.error`
    /// event is on the agent span and NOT duplicated on the root span.
    #[tokio::test]
    async fn test_l2_error_encountered_lands_on_the_phase_span_when_one_is_open() {
        let tmp = TempDir::new("error-on-phase");
        let traces = tmp.path.join("traces");
        {
            let guard = init_tracing(&traces).expect("init_tracing");
            let obs = OtelObserver::new(guard.tracer());
            obs.observe(&BoiEvent::SpecStarted { spec_id: spec() })
                .await
                .unwrap();
            // Open the `execute` phase span.
            obs.observe(&phase_started()).await.unwrap();
            // An error in the `execute` phase — must resolve onto that phase
            // span by (spec_id, phase).
            obs.observe(&BoiEvent::ErrorEncountered {
                spec_id: spec(),
                task_id: Some(task()),
                phase: "execute".into(),
                error: "verify gate failed".into(),
                why: "PATH stripped".into(),
                fix_proposed: Some("export PATH".into()),
                fingerprint: "phase-scoped".into(),
            })
            .await
            .expect("observe ErrorEncountered");
            obs.observe(&phase_completed()).await.unwrap();
            obs.observe(&BoiEvent::SpecCompleted { spec_id: spec() })
                .await
                .unwrap();
        }
        let spans = read_spans(&traces);
        let agent = spans
            .iter()
            .find(|s| s["name"] == "invoke_agent boi.worker")
            .expect("agent span");
        let root = spans
            .iter()
            .find(|s| s["name"] == "invoke_workflow boi.spec")
            .expect("root span");
        // The `boi.error` event is on the AGENT (phase) span — the G24.1 fix.
        let agent_events = agent["events"].as_array().expect("agent events");
        assert!(
            agent_events.iter().any(|e| e["name"] == "boi.error"),
            "C-rt-S4 regression: the boi.error event must land on the \
             invoke_agent phase span (resolved by spec_id+phase), not the root",
        );
        // ...and NOT on the spec root span.
        let root_errors = root["events"]
            .as_array()
            .map(|evs| evs.iter().filter(|e| e["name"] == "boi.error").count())
            .unwrap_or(0);
        assert_eq!(
            root_errors, 0,
            "the boi.error event must NOT fall back to the spec root span \
             while a matching phase span is open",
        );
    }

    /// `ToolInvoked` adds an `execute_tool` span event.
    ///
    /// `ToolInvoked` carries no `phase_run_id` (only `spec_id`/`task_id`), so
    /// per `add_event_to_span` the event lands on the **spec root span** — the
    /// only span resolvable from a bare `spec_id`. The event still appears in
    /// the right trace; see the latent-defect note in the Phase 8a report.
    #[tokio::test]
    async fn test_l2_tool_invoked_adds_execute_tool_event() {
        let tmp = TempDir::new("tool");
        let traces = tmp.path.join("traces");
        {
            let guard = init_tracing(&traces).expect("init_tracing");
            let obs = OtelObserver::new(guard.tracer());
            obs.observe(&BoiEvent::SpecStarted { spec_id: spec() })
                .await
                .unwrap();
            obs.observe(&phase_started()).await.unwrap();
            obs.observe(&BoiEvent::ToolInvoked {
                spec_id: spec(),
                task_id: Some(task()),
                tool: "Bash".into(),
                args_summary: "{\"cmd\":\"cargo test\"}".into(),
                result_summary: "exit 0".into(),
            })
            .await
            .expect("observe ToolInvoked");
            obs.observe(&phase_completed()).await.unwrap();
            obs.observe(&BoiEvent::SpecCompleted { spec_id: spec() })
                .await
                .unwrap();
        }
        let spans = read_spans(&traces);
        // The event attaches to the spec root span (no `phase_run_id` on the
        // event to resolve a phase span).
        let root = spans
            .iter()
            .find(|s| s["name"] == "invoke_workflow boi.spec")
            .expect("root span");
        let events = root["events"].as_array().expect("events array");
        let tool_event = events
            .iter()
            .find(|e| e["name"] == "execute_tool")
            .expect("spec span carries an execute_tool event");
        let attrs = tool_event["attributes"].as_array().unwrap();
        assert!(
            attrs
                .iter()
                .any(|kv| kv["key"] == "boi.tool" && kv["value"]["stringValue"] == "Bash"),
            "execute_tool event carries the tool name"
        );
    }

    /// A `SpecFailed` ends the root span with an error status; the registry is
    /// drained (no leak) even on the failure path.
    #[tokio::test]
    async fn test_l2_spec_failed_ends_root_span_and_drains_registry() {
        let tmp = TempDir::new("spec-fail");
        let traces = tmp.path.join("traces");
        {
            let guard = init_tracing(&traces).expect("init_tracing");
            let obs = OtelObserver::new(guard.tracer());
            obs.observe(&BoiEvent::SpecStarted { spec_id: spec() })
                .await
                .unwrap();
            obs.observe(&BoiEvent::SpecFailed {
                spec_id: spec(),
                reason: FailureReason::DaemonCrash,
            })
            .await
            .expect("observe SpecFailed");
            assert_eq!(
                OtelObserver::lock(&obs.spec_spans).len(),
                0,
                "SpecFailed drained the registry"
            );
        }
        let spans = read_spans(&traces);
        let root = &spans[0];
        // OTLP status code 2 == ERROR.
        assert_eq!(root["status"]["code"], 2, "failed spec → ERROR status");
    }

    /// The task-lifecycle transitions and `PlanRevised` are explicit no-ops —
    /// they open no span and never error.
    #[tokio::test]
    async fn test_l2_task_lifecycle_and_plan_revised_are_noops() {
        let tmp = TempDir::new("noop");
        let traces = tmp.path.join("traces");
        {
            let guard = init_tracing(&traces).expect("init_tracing");
            let obs = OtelObserver::new(guard.tracer());
            for event in [
                BoiEvent::TaskStarted {
                    spec_id: spec(),
                    task_id: task(),
                },
                // C-cr-6 — `TaskBlocked` was missing from this no-op iteration;
                // it is an explicit no-op arm in `observe` like the other
                // task-lifecycle transitions and must be exercised here.
                BoiEvent::TaskBlocked {
                    spec_id: spec(),
                    task_id: task(),
                    reason: BlockedReason::AwaitingDeps { unmet_deps: vec![] },
                },
                BoiEvent::TaskUnblocked {
                    spec_id: spec(),
                    task_id: task(),
                },
                BoiEvent::TaskPassed {
                    spec_id: spec(),
                    task_id: task(),
                    evidence: Evidence::default(),
                },
                BoiEvent::TaskCanceled {
                    spec_id: spec(),
                    task_id: task(),
                    reason: CancellationReason::SpecCanceled,
                },
                BoiEvent::PlanRevised {
                    spec_id: spec(),
                    diff: serde_json::Value::Null,
                    trigger: "t".into(),
                    trigger_meta: serde_json::Value::Null,
                },
            ] {
                obs.observe(&event).await.expect("no-op observe is Ok");
            }
            assert_eq!(OtelObserver::lock(&obs.spec_spans).len(), 0);
            assert_eq!(OtelObserver::lock(&obs.phase_spans).len(), 0);
        }
        assert!(read_spans(&traces).is_empty(), "no-op events open no spans");
    }
}
