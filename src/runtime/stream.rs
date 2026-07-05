//! Goose `stream-json` → [`BoiEvent`] mapping (Task 7.2).
//!
//! `goose run --output-format stream-json` emits **JSONL** — one JSON object
//! per line. [`StreamMapper::map`] takes one line and produces zero-or-more
//! [`BoiEvent`]s.
//!
//! ## The real Goose `stream-json` event set (spike §Q3 — load-bearing)
//!
//! The plan's original stream model named `tool_use` / `message_delta` /
//! `message_stop` — **none of those exist**. A source survey of `block/goose`
//! (`docs/research/goose-spike-2026-05-20.md` §Q3) found exactly four event
//! types, tagged on a `type` field:
//!
//! | `type`         | Carried payload                       | Maps to |
//! |----------------|---------------------------------------|---------|
//! | `message`      | a `Message` (LLM response / tool msg) | scan `content[]` → one `ToolInvoked` per `toolRequest`; assistant text is accumulated for verdict extraction |
//! | `notification` | MCP server log / progress             | dropped (a pure MCP log) |
//! | `error`        | an error `String`                     | terminal `Fail` + an `ErrorEncountered` |
//! | `complete`     | `total/input/output` token counts     | terminal `PhaseCompleted` carrying the tokens |
//!
//! Goose `stream-json` streams an assistant turn as MANY `message` events —
//! token-delta fragments that all share one `message.id` (verified against
//! Goose 1.34.1; the spike's original "whole messages, no deltas" reading was
//! wrong). The mapper ASSEMBLES all assistant text fragments across the whole
//! stream into one growing buffer. Beyond that it carries a `seen_complete`
//! flag (so [`crate::runtime::goose`] can detect a stream that ended without a
//! `complete` event).
//!
//! ## The verdict channel — accumulate-all + last-strict-parseable-object (review C-cr-3)
//!
//! The worker emits its `WorkerVerdict` as a JSON object somewhere in its
//! assistant output. The mapper accumulates ALL assistant text fragments
//! regardless of `message.id` — token-delta fragments concatenate, and text
//! from a later turn (a tool-call turn, a closing remark) is also retained.
//!
//! At `complete`, [`extract_verdict`] scans EVERY balanced top-level `{...}`
//! substring in the accumulated text and returns the LAST one that
//! STRICT-parses as a valid [`WorkerVerdict`]. This means:
//!
//! - A mid-task illustrative JSON object (an example payload, a quoted config)
//!   fails strict parse (`deny_unknown_fields`) and is skipped — preserving the
//!   C-cr-3 intent without the fragile "last-message" anchor.
//! - A verdict that appears before a trailing tool call or closing-remark
//!   message is recovered even though it is not in the final delta. Under
//!   Goose's delta + tool-call streaming, "the last assistant message" is not
//!   a reliable anchor: a tool call starts a new `message.id` and any
//!   subsequent closing remark would have clobbered the verdict under the old
//!   per-id model.
//!
//! ## Loose envelope, strict inner (Batch A review L1)
//!
//! [`GooseStreamEvent`] parses **loosely** — NO `deny_unknown_fields`, so a
//! provider-specific extra field never breaks the envelope. The
//! [`WorkerVerdict`] extracted from the worker's text content is deserialized
//! **strictly** ([`WorkerVerdict`] has `deny_unknown_fields`, Phase 1 Task 1.4).
//!
//! ## Per-error-kind policy — no silent swallow (review S8)
//!
//! Every `Err` arm has a stated destination; no line is ever `continue`-d
//! away:
//!
//! - [`StreamMapError::VerdictParse`] — the worker's payload is not a valid
//!   `WorkerVerdict` → [`crate::runtime::goose`] retries 2×.
//! - [`StreamMapError::AgentError`] — a non-overflow `error` line (an HTTP 503,
//!   a rate-limit, any transient provider failure) → [`crate::runtime::goose`]
//!   retries 2× (Task 7.2/7.3 + the Goose spike: "any other `error` line →
//!   retry 2×"). The terminal `Fail` + `ErrorEncountered` are synthesized by
//!   the goose runtime only AFTER the retry budget is exhausted — a transient
//!   error must NOT hard-fail the phase on the first occurrence (review
//!   C-cr-1).
//! - [`StreamMapError::ContextOverflow`] — a context-overflow `error` line →
//!   terminal `Fail{context_overflow}`, **no retry** (a retry just overflows).
//! - [`StreamMapError::Transport`] — a non-JSON line **after** the first valid
//!   JSON line → `error!` LOUD + terminal `Fail{stream_corrupt}`. Never
//!   skipped mid-stream — a skipped mid-stream line is a silent swallow (S8).
//!
//! ## Goose startup banner — pre-stream preamble skip
//!
//! Goose 1.34.1 prints a multi-line ASCII-art banner to stdout before the JSON
//! stream begins, even under `--output-format stream-json`. The mapper tolerates
//! this with a `stream_started` flag: **before** the first successfully-parsed
//! JSON line, a non-JSON line is treated as startup preamble and skipped
//! (`tracing::debug!`). Once `stream_started` is `true` (the first JSON line
//! parsed), a non-JSON line is genuine mid-stream corruption → `Transport`. The
//! S8 "no silent swallow" guarantee is unchanged for mid-stream lines.
//!
//! [`WorkerVerdict`]: crate::types::verdict::WorkerVerdict

use serde::Deserialize;

use crate::types::event::BoiEvent;
use crate::types::ids::{PhaseRunId, SpecId, TaskId};
use crate::types::verdict::WorkerVerdict;

/// A loosely-parsed Goose `stream-json` envelope.
///
/// Tagged on `type` over the four REAL Goose variants. NOT
/// `deny_unknown_fields` — a provider-specific extra field is tolerated (the
/// loose-envelope half of the Batch A review L1 rule).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum GooseStreamEvent {
    /// An agent message — the LLM response, or a tool-result message.
    Message {
        /// The message payload.
        message: GooseMessage,
    },
    /// An MCP server log / progress notification — dropped by the mapper.
    Notification {
        /// The notification's originating extension. Retained for parity with
        /// Goose's shape; unused by the mapper.
        #[serde(default)]
        #[allow(dead_code)]
        extension_id: Option<String>,
    },
    /// An agent error — the whole turn failed.
    Error {
        /// The error string.
        error: String,
    },
    /// The terminal event — emitted once at the end of a run.
    Complete {
        /// Total tokens consumed (input + output). `None` when Goose could not
        /// source it from the session record (spike §Q4).
        #[serde(default)]
        total_tokens: Option<i64>,
        /// Input tokens consumed.
        #[serde(default)]
        input_tokens: Option<i64>,
        /// Output tokens produced.
        #[serde(default)]
        output_tokens: Option<i64>,
    },
}

/// A Goose `Message` — loosely parsed (spike §Q3, `conversation/message.rs`).
///
/// `content` and `role` are load-bearing. Other `Message` fields (`id`,
/// `created`, `metadata`) are tolerated and ignored.
#[derive(Debug, Clone, Deserialize)]
struct GooseMessage {
    /// The message role — `user` / `assistant` / `tool`.
    #[serde(default)]
    role: Option<String>,
    /// The content blocks. A turn's `toolRequest`s all ride here.
    #[serde(default)]
    content: Vec<GooseContent>,
}

/// One Goose `MessageContent` block — tagged on `type`, camelCase (spike §Q3).
///
/// `#[serde(other)]` on [`GooseContent::Other`] makes an unrecognized content
/// type (`thinking`, `image`, a future variant) a tolerated no-op rather than
/// a parse failure — the loose-envelope rule applied one level down.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum GooseContent {
    /// A tool call the assistant requested.
    ToolRequest {
        /// The tool-call detail.
        #[serde(rename = "toolCall")]
        tool_call: GooseToolCall,
    },
    /// A tool result. Carried for completeness; the mapper does not emit a
    /// dedicated event for it (a `verify_run` result already surfaces as a
    /// `VerifyChecked` via the MCP handler path).
    ToolResponse {},
    /// Plain text — the worker's `WorkerVerdict` payload rides here.
    Text {
        /// The text body.
        text: String,
    },
    /// Any other content type (`thinking`, `image`, ...) — tolerated, ignored.
    #[serde(other)]
    Other,
}

/// A Goose `ToolRequest.toolCall` — an MCP `CallToolRequestParams` (spike §Q3).
///
/// Goose serializes `toolCall` through a custom `Result`-aware serde; in the
/// success case it is `{ name, arguments }`. Parsed loosely — `arguments` is a
/// free `serde_json::Value`, absent-tolerant.
#[derive(Debug, Clone, Deserialize)]
struct GooseToolCall {
    /// The tool name.
    #[serde(default)]
    name: Option<String>,
    /// The tool arguments.
    #[serde(default)]
    arguments: Option<serde_json::Value>,
}

/// A stream line could not be mapped.
///
/// Each variant has a stated destination in [`crate::runtime::goose`] (review
/// S8 — no `Err` arm is silently dropped). See the module doc.
///
/// `pub(crate)` — `runtime/`-internal, consumed only by `goose.rs` (Task 7.7).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum StreamMapError {
    /// The worker's payload is not a valid [`WorkerVerdict`] — the goose
    /// runtime retries (2×).
    #[error("worker verdict parse failed: {0}")]
    VerdictParse(String),
    /// The worker turn produced NO assistant text AND made NO tool calls — a
    /// fully empty completion. The provider returned nothing: `claude-code`
    /// emits a clean `complete` with no assistant message under rate-limiting
    /// (FIX-004, 2026-06-06 — confirmed from goose's session DB: failed plan
    /// runs had only the user prompt, no assistant row). Kept DISTINCT from
    /// [`VerdictParse`] (where the worker DID respond but the verdict was
    /// malformed) so the goose runtime can back off + surface the provider
    /// stderr (S6) rather than silently burning 3 immediate retries and
    /// blaming the worker for "no valid verdict".
    #[error("worker produced an empty completion (no assistant text, no tool calls)")]
    EmptyCompletion,
    /// A non-overflow agent `error` line — a transient provider failure (HTTP
    /// 503, a rate-limit, a turn-level error). The goose runtime retries (2×);
    /// only after the budget is exhausted does it synthesize the terminal
    /// `Fail` + `ErrorEncountered` (review C-cr-1 — a transient error must not
    /// hard-fail the phase on its first occurrence). Carries the agent's error
    /// string for the eventual `Fail` verdict.
    #[error("agent error: {0}")]
    AgentError(String),
    /// A context-overflow `error` line — a terminal `Fail`, NOT retried (a
    /// retry just overflows again).
    #[error("worker context overflowed")]
    ContextOverflow,
    /// A non-JSON / structurally-unparseable line — loud, terminal
    /// `Fail{stream_corrupt}`. Never skipped.
    #[error("stream transport error: {0}")]
    Transport(String),
}

/// The per-phase-run identity a [`StreamMapper`] needs to construct its
/// terminal `PhaseCompleted` event.
///
/// Bundled into one struct so [`StreamMapper::new`] keeps a tidy signature:
/// the mapper is otherwise near-stateless (just `seen_complete` +
/// `accumulated_text`), and this identity is fixed for the whole stream.
///
/// `pub(crate)` — `runtime/`-internal, consumed only by `goose.rs`.
#[derive(Debug, Clone)]
pub(crate) struct StreamIdentity {
    /// The spec this stream belongs to.
    pub(crate) spec_id: SpecId,
    /// The task this stream belongs to — `None` for a spec-level phase.
    pub(crate) task_id: Option<TaskId>,
    /// The phase run this stream is.
    pub(crate) phase_run_id: PhaseRunId,
    /// The phase name.
    pub(crate) phase: String,
}

/// Maps Goose `stream-json` lines to [`BoiEvent`]s.
///
/// Stateful: beyond the fixed [`StreamIdentity`] it carries `seen_complete`
/// (so the runtime can detect a `complete`-less stream), `last_assistant_text`
/// (all assistant text across the whole stream — the [`WorkerVerdict`] is
/// extracted from it at `complete`), and `stream_started` (whether the first
/// JSON line has parsed — used to skip Goose's startup banner preamble).
///
/// [`map`] returns a `Vec` — one `message` event can yield several
/// `ToolInvoked`s (a turn can carry several `toolRequest` blocks).
///
/// `pub(crate)` — `runtime/`-internal, consumed only by `goose.rs` (Task 7.7).
///
/// [`map`]: StreamMapper::map
/// [`WorkerVerdict`]: crate::types::verdict::WorkerVerdict
pub(crate) struct StreamMapper {
    /// The fixed per-phase-run identity — used to construct every event.
    id: StreamIdentity,
    /// `true` once a `complete` event has been mapped. The goose runtime reads
    /// this to detect a stream that ended without a terminal event.
    seen_complete: bool,
    /// `true` once the first JSON line has been successfully parsed. Before
    /// this, a non-JSON line is Goose's startup banner preamble — skipped with
    /// `tracing::debug!`. After this, a non-JSON line is mid-stream corruption
    /// → `Transport` (review S8 — no silent swallow mid-stream).
    stream_started: bool,
    /// ALL accumulated assistant text across the stream. Token-delta fragments
    /// are appended as they arrive; text from later turns (a tool-call delta,
    /// a closing remark) is also retained. The [`WorkerVerdict`] is
    /// strict-parsed from this at `complete` — [`extract_verdict`] scans every
    /// `{...}` object and returns the last strict-parseable one (C-cr-3).
    /// `None` until the first assistant text fragment.
    last_assistant_text: Option<String>,
    /// `true` once ANY `message` event has been mapped (assistant OR tool role,
    /// any content). Used at `complete` to tell a fully-silent turn (NO message
    /// of any kind → the provider returned nothing → [`StreamMapError::
    /// EmptyCompletion`]) apart from a turn that DID produce output but ended
    /// without a parseable verdict → [`VerdictParse`]. A tool-role message or a
    /// `toolRequest` both count as activity (the worker engaged), so neither is
    /// an empty completion.
    saw_message: bool,
}

impl StreamMapper {
    /// A fresh mapper for one phase run's stream.
    pub(crate) fn new(id: StreamIdentity) -> Self {
        Self {
            id,
            seen_complete: false,
            stream_started: false,
            last_assistant_text: None,
            saw_message: false,
        }
    }

    /// Whether a `complete` event has been mapped.
    ///
    /// [`crate::runtime::goose`] checks this after the stream drains: a stream
    /// that ended without `complete` → synthesize `Fail{goose_crashed}`.
    pub(crate) fn seen_complete(&self) -> bool {
        self.seen_complete
    }

    /// Map one `stream-json` line to zero-or-more [`BoiEvent`]s.
    ///
    /// - `message` → one `ToolInvoked` per `toolRequest` in `content[]`;
    ///   assistant `text` content is accumulated (the verdict rides there).
    /// - `notification` → `[]` (a pure MCP log).
    /// - `error` → `Err`: a context-overflow error → `Err(ContextOverflow)`
    ///   (no retry); any other error → `Err(AgentError)` (the goose runtime
    ///   retries 2×, then synthesizes the terminal `Fail` + `ErrorEncountered`).
    /// - `complete` → a terminal `PhaseCompleted` carrying the token counts,
    ///   with the [`WorkerVerdict`] strict-parsed from `accumulated_text`.
    ///
    /// A non-JSON / unparseable line → `Err(Transport)` — never silently
    /// dropped.
    ///
    /// [`WorkerVerdict`]: crate::types::verdict::WorkerVerdict
    pub(crate) fn map(&mut self, line: &str) -> Result<Vec<BoiEvent>, StreamMapError> {
        let trimmed = line.trim();
        // A blank keep-alive line is not an error — Goose's JSONL is one event
        // per non-empty line; an empty line carries nothing.
        if trimmed.is_empty() {
            return Ok(vec![]);
        }

        // Loose envelope parse — a provider-specific extra field is tolerated.
        // A structurally-broken line after the stream has started is a loud
        // `Transport` error (review S8 — no silent swallow mid-stream).
        // Before the first JSON line, a non-JSON line is Goose's startup
        // banner preamble — skip it with a debug log (see module doc).
        let event: GooseStreamEvent = match serde_json::from_str(trimmed) {
            Ok(ev) => {
                self.stream_started = true;
                ev
            }
            Err(e) => {
                if !self.stream_started {
                    tracing::debug!(
                        line = %trimmed,
                        "skipping pre-stream non-JSON line (Goose startup banner preamble)",
                    );
                    return Ok(vec![]);
                }
                return Err(StreamMapError::Transport(format!(
                    "unparseable stream line: {e}"
                )));
            }
        };

        match event {
            GooseStreamEvent::Message { message } => Ok(self.map_message(message)),
            // A pure MCP log — nothing to surface.
            GooseStreamEvent::Notification { .. } => Ok(vec![]),
            GooseStreamEvent::Error { error } => self.map_error(&error),
            GooseStreamEvent::Complete {
                total_tokens,
                input_tokens,
                output_tokens,
            } => self.map_complete(total_tokens, input_tokens, output_tokens),
        }
    }

    /// Map a `message` event: a `ToolInvoked` per `toolRequest`, and — for an
    /// assistant message with text content — append to the verdict accumulator.
    fn map_message(&mut self, message: GooseMessage) -> Vec<BoiEvent> {
        // ANY message event — assistant or tool role — is worker activity, so
        // the turn is not a fully-empty completion (FIX-004).
        self.saw_message = true;
        let is_assistant = message.role.as_deref() == Some("assistant");
        let mut events = Vec::new();
        for block in &message.content {
            match block {
                GooseContent::ToolRequest { tool_call } => {
                    events.push(self.tool_invoked(tool_call));
                }
                GooseContent::Text { text } => {
                    // Accumulate ALL assistant text regardless of message.id.
                    // Token-delta fragments concatenate; text from a later turn
                    // (a tool-call delta, a closing remark) is also retained.
                    // A tool-role / user-role text block is never the verdict.
                    if is_assistant && !text.is_empty() {
                        self.last_assistant_text
                            .get_or_insert_with(String::new)
                            .push_str(text);
                    }
                }
                GooseContent::ToolResponse {} | GooseContent::Other => {}
            }
        }
        events
    }

    /// Build a `ToolInvoked` from one `toolRequest`'s `toolCall`.
    fn tool_invoked(&self, call: &GooseToolCall) -> BoiEvent {
        let tool = call.name.clone().unwrap_or_else(|| "<unknown>".to_owned());
        let args_summary = call
            .arguments
            .as_ref()
            .map(summarize_json)
            .unwrap_or_else(|| "{}".to_owned());
        BoiEvent::ToolInvoked {
            spec_id: self.id.spec_id.clone(),
            task_id: self.id.task_id.clone(),
            tool,
            args_summary,
            // The stream `message` carries the request, not the result —
            // result detail arrives in a later `toolResponse`/`VerifyChecked`.
            result_summary: "(pending)".to_owned(),
        }
    }

    /// Map an `error` event. A context-overflow error is `Err(ContextOverflow)`
    /// (no retry); any other error is `Err(AgentError)` — a RETRYABLE failure
    /// (review C-cr-1). The goose runtime's 2-retry loop retries it; only after
    /// the budget is exhausted does it synthesize the terminal `Fail` +
    /// `ErrorEncountered`. The mapper no longer emits those events itself — a
    /// non-overflow `error` is no longer terminal-on-first-occurrence.
    fn map_error(&mut self, error: &str) -> Result<Vec<BoiEvent>, StreamMapError> {
        if is_context_overflow(error) {
            // A retry would just overflow again — bubble a no-retry error
            // (review S8). The goose runtime turns this into a terminal Fail.
            return Err(StreamMapError::ContextOverflow);
        }
        // A non-overflow error (HTTP 503, rate-limit, a transient turn failure)
        // ends THIS attempt but is RETRYABLE — the goose runtime retries it
        // (Task 7.2/7.3 + the Goose spike). The terminal `Fail` +
        // `ErrorEncountered` are synthesized by the runtime after the retry
        // budget is exhausted, not here (review C-cr-1).
        Err(StreamMapError::AgentError(error.to_owned()))
    }

    /// Map the terminal `complete` event: strict-parse the [`WorkerVerdict`]
    /// from the accumulated worker text, build a terminal `PhaseCompleted`.
    ///
    /// [`WorkerVerdict`]: crate::types::verdict::WorkerVerdict
    fn map_complete(
        &mut self,
        total: Option<i64>,
        input: Option<i64>,
        output: Option<i64>,
    ) -> Result<Vec<BoiEvent>, StreamMapError> {
        self.seen_complete = true;
        // FIX-004: a turn that produced NO assistant text AND made NO tool
        // calls is a fully empty completion — the provider returned nothing
        // (the `claude-code` rate-limit signature). Surface it as a distinct
        // `EmptyCompletion` so the runtime backs off + reports the provider
        // stderr, instead of mislabelling it "the worker emitted no valid
        // verdict" and retrying 3× back-to-back into the same rate-limit wall.
        let text = self.last_assistant_text.as_deref().unwrap_or("");
        if text.trim().is_empty() && !self.saw_message {
            return Err(StreamMapError::EmptyCompletion);
        }
        // The verdict — strict-parsed (deny_unknown_fields) out of the LAST
        // assistant message's text (review C-cr-3). A parse failure (the worker
        // DID respond / DID act but the verdict is malformed) is a
        // `VerdictParse` error; the goose runtime retries 2× (review S8).
        let verdict = extract_verdict(text)?;
        let (tokens_in, tokens_out) = token_split(total, input, output);
        Ok(vec![BoiEvent::PhaseCompleted {
            phase_run_id: self.id.phase_run_id.clone(),
            spec_id: self.id.spec_id.clone(),
            task_id: self.id.task_id.clone(),
            phase: self.id.phase.clone(),
            verdict,
            // Goose emits token counts only — per the 2026-06-01 directive
            // BOI no longer computes a per-phase dollar figure; tokens stay as
            // the spend-hint signal.
            tokens_in,
            tokens_out,
            duration_ms: 0,
        }])
    }
}

/// Strict-parse a [`WorkerVerdict`] from the accumulated assistant text.
///
/// Scans EVERY balanced top-level `{...}` substring in `text` and returns the
/// LAST one that strict-parses as a valid [`WorkerVerdict`]. A mid-task
/// illustrative JSON object (an example payload, a quoted config) fails strict
/// parse (`deny_unknown_fields`) and is skipped — preserving the C-cr-3 intent
/// without requiring a "last-message" anchor. An empty / object-free / all-
/// invalid-object payload → [`StreamMapError::VerdictParse`].
fn extract_verdict(text: &str) -> Result<WorkerVerdict, StreamMapError> {
    // Collect all balanced objects, then scan from the end to find the last
    // one that strict-parses as a WorkerVerdict.
    let candidates = all_json_objects(text);
    for candidate in candidates.iter().rev() {
        if let Ok(verdict) = serde_json::from_str::<WorkerVerdict>(candidate) {
            return Ok(verdict);
        }
    }
    // A VerdictParse failure is the #1 thing the operator needs the model's
    // actual text to debug — most often the model emitted markdown-fenced
    // JSON, an extra field, or trailing prose. Embed a tail of the accumulated
    // text + a candidate-object count so the verdict body in `phase_runs`
    // self-explains without needing to spelunk goose's session DB.
    Err(StreamMapError::VerdictParse(format!(
        "the worker emitted no JSON object carrying a WorkerVerdict \
         (text_len={}, candidate_objects={}, text_tail={:?})",
        text.len(),
        candidates.len(),
        tail_for_diag(text, 1200),
    )))
}

/// The last `max_len` characters of `text` — for embedding in a diagnostic
/// error. Char-boundary-safe (UTF-8 aware).
fn tail_for_diag(text: &str, max_len: usize) -> &str {
    if text.len() <= max_len {
        return text;
    }
    // Walk back from the end to a char boundary at or after `text.len() - max_len`.
    let start = text.len() - max_len;
    let mut i = start;
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    &text[i..]
}

/// Collect all balanced top-level `{...}` JSON object substrings in `text`.
///
/// Brace-depth scan — string literals (and escaped quotes inside them) are
/// honored so a `{` inside a JSON string never miscounts.
fn all_json_objects(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut objects = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = balanced_object_end(bytes, i) {
                objects.push(&text[i..=end]);
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    objects
}

/// Find the last balanced top-level `{...}` JSON object substring in `text`.
///
/// A worker's final message is often prose + a JSON block; this isolates the
/// JSON. Brace-depth scan — string literals (and escaped quotes inside them)
/// are honored so a `{` inside a JSON string never miscounts.
#[cfg(test)]
fn last_json_object(text: &str) -> Option<&str> {
    all_json_objects(text).into_iter().last()
}

/// Given `bytes[start] == b'{'`, return the index of the matching `}`.
fn balanced_object_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Whether an `error` string describes a context / token-limit overflow.
///
/// A small phrase set — Goose surfaces provider context-limit errors as a
/// passthrough string; the common phrasings are matched case-insensitively.
fn is_context_overflow(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("context_length_exceeded")
        || lower.contains("maximum context")
        || lower.contains("too many tokens")
        || lower.contains("token limit")
        || lower.contains("prompt is too long")
}

/// Reduce the three optional `complete`-event token counts to a
/// `(tokens_in, tokens_out)` pair for `PhaseCompleted`.
///
/// Goose's `complete` event carries `total/input/output`, all `Option` (spike
/// §Q4 — they come from the session record and can be `None`). When only
/// `total` is present BOI cannot split it; `tokens_in` then takes the total
/// and `tokens_out` is 0 — an honest, non-fabricated split.
fn token_split(total: Option<i64>, input: Option<i64>, output: Option<i64>) -> (u64, u64) {
    let to_u64 = |v: Option<i64>| v.unwrap_or(0).max(0) as u64;
    match (input, output) {
        (Some(_), _) | (_, Some(_)) => (to_u64(input), to_u64(output)),
        // Only `total` (or nothing) — cannot split; attribute it all to input.
        (None, None) => (to_u64(total), 0),
    }
}

/// A bounded, single-line summary of a JSON value for an event payload.
fn summarize_json(value: &serde_json::Value) -> String {
    const MAX: usize = 200;
    let s = value.to_string();
    if s.len() <= MAX {
        return s;
    }
    let cut = s
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= MAX)
        .last()
        .unwrap_or(0);
    format!("{}…", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::verdict::VerdictOutcome;

    fn spec() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }

    fn mapper() -> StreamMapper {
        StreamMapper::new(StreamIdentity {
            spec_id: spec(),
            task_id: Some(task()),
            phase_run_id: PhaseRunId::new("P0000001a").unwrap(),
            phase: "execute".to_owned(),
        })
    }

    /// A `message` event whose `content[]` carries a `toolRequest` →
    /// `ToolInvoked`. Two `toolRequest`s in one message → two `ToolInvoked`s.
    #[test]
    fn test_l2_message_with_tool_requests_yields_tool_invoked_per_request() {
        let line = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "type": "toolRequest",
                        "toolCall": { "name": "verify_run", "arguments": { "command": "cargo test" } }
                    },
                    {
                        "type": "toolRequest",
                        "toolCall": { "name": "worktree_diff", "arguments": {} }
                    }
                ]
            }
        })
        .to_string();

        let events = mapper().map(&line).unwrap();
        assert_eq!(events.len(), 2, "one ToolInvoked per toolRequest block");
        let tools: Vec<&str> = events
            .iter()
            .map(|e| match e {
                BoiEvent::ToolInvoked { tool, .. } => tool.as_str(),
                other => unreachable!("expected ToolInvoked, got {other:?}"),
            })
            .collect();
        assert_eq!(tools, vec!["verify_run", "worktree_diff"]);
    }

    /// A `complete` event with a valid worker payload → `PhaseCompleted` with
    /// a `Passing` verdict, carrying the token counts.
    #[test]
    fn test_l2_complete_with_valid_verdict_yields_phase_completed_passing() {
        let mut m = mapper();
        // The worker's final assistant message carries the verdict JSON.
        let verdict_line = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "text",
                    "text": "Done. {\"synopsis\":\"implemented it\",\"outcome\":{\"type\":\"passing\",\"evidence\":{\"files_touched\":[],\"verifications\":[],\"summary\":\"ok\"}}}"
                }]
            }
        })
        .to_string();
        assert!(
            m.map(&verdict_line).unwrap().is_empty(),
            "text accumulates, emits nothing"
        );

        let complete = serde_json::json!({
            "type": "complete",
            "total_tokens": 1500,
            "input_tokens": 1200,
            "output_tokens": 300
        })
        .to_string();
        let events = m.map(&complete).unwrap();
        assert_eq!(events.len(), 1);
        let BoiEvent::PhaseCompleted {
            verdict,
            tokens_in,
            tokens_out,
            ..
        } = &events[0]
        else {
            unreachable!("complete must yield PhaseCompleted, got {:?}", events[0]);
        };
        assert!(matches!(verdict.outcome, VerdictOutcome::Passing { .. }));
        assert_eq!(*tokens_in, 1200);
        assert_eq!(*tokens_out, 300);
        assert!(m.seen_complete(), "the complete flag must be set");
    }

    /// Goose `stream-json` streams an assistant turn as MANY delta `message`
    /// events. The mapper must ASSEMBLE the text fragments — the verdict is
    /// split across them. Feeds the verdict as 3 fragments.
    #[test]
    fn test_l2_assembles_text_deltas_across_same_id_messages() {
        let mut m = mapper();
        let frags = [
            r#"{"synopsis":"did it","outcome":{"type":"passing","#,
            r#""evidence":{"files_touched":[],"verifications":[],"#,
            r#""summary":"ok"}}}"#,
        ];
        for frag in frags {
            let line = serde_json::json!({
                "type": "message",
                "message": {
                    "id": "gen-deltas-1",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": frag }]
                }
            })
            .to_string();
            assert!(
                m.map(&line).unwrap().is_empty(),
                "a text delta emits no events",
            );
        }
        let events = m
            .map(&serde_json::json!({ "type": "complete" }).to_string())
            .unwrap();
        assert_eq!(events.len(), 1, "complete yields one PhaseCompleted");
        let BoiEvent::PhaseCompleted { verdict, .. } = &events[0] else {
            unreachable!("expected PhaseCompleted, got {:?}", events[0]);
        };
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "the verdict assembled from same-id deltas must parse as Passing",
        );
    }

    /// An envelope-level extra provider field still parses (loose envelope).
    #[test]
    fn test_l2_envelope_extra_provider_field_still_parses() {
        let line = serde_json::json!({
            "type": "complete",
            "total_tokens": 100,
            "input_tokens": 80,
            "output_tokens": 20,
            "provider_specific_extra": "tolerated"
        })
        .to_string();
        let mut m = mapper();
        // The verdict is missing and no message arrived → EmptyCompletion
        // (FIX-004), but the ENVELOPE parsed fine (no Transport error) — that is
        // the loose-envelope guarantee this test pins. The key assertion is the
        // absence of a Transport/parse error on the extra field.
        let err = m.map(&line).unwrap_err();
        assert!(
            matches!(err, StreamMapError::EmptyCompletion),
            "an extra envelope field must not break parsing — got {err:?}",
        );
    }

    /// A malformed inner `WorkerVerdict` (extra field — `deny_unknown_fields`)
    /// → `VerdictParse`.
    #[test]
    fn test_l2_malformed_inner_verdict_yields_verdict_parse() {
        let mut m = mapper();
        let verdict_line = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "text",
                    "text": "{\"synopsis\":\"x\",\"outcome\":{\"type\":\"redo\",\"reason\":\"r\"},\"bogus_field\":1}"
                }]
            }
        })
        .to_string();
        m.map(&verdict_line).unwrap();
        let err = m
            .map(&serde_json::json!({ "type": "complete" }).to_string())
            .unwrap_err();
        assert!(
            matches!(err, StreamMapError::VerdictParse(_)),
            "an inner verdict with an extra field must fail strict parse, got {err:?}",
        );
    }

    /// FIX-004: a `complete` with NO assistant text AND no tool calls is a
    /// fully empty completion (the provider returned nothing — the rate-limit
    /// signature), classified as `EmptyCompletion`, NOT `VerdictParse`. The
    /// runtime backs off + surfaces the provider stderr instead of blaming the
    /// worker for an absent verdict.
    #[test]
    fn test_l2_complete_with_no_text_and_no_tools_yields_empty_completion() {
        let mut m = mapper();
        let err = m
            .map(&serde_json::json!({ "type": "complete" }).to_string())
            .unwrap_err();
        assert!(
            matches!(err, StreamMapError::EmptyCompletion),
            "an empty turn (no text, no tools) is an empty completion, got {err:?}",
        );
    }

    /// FIX-004 boundary: a turn that DID act — made a tool call — but ended
    /// without a parseable verdict is the worker's fault, not an empty provider
    /// turn → `VerdictParse` (NOT `EmptyCompletion`). This keeps the empty-
    /// completion class narrow: only a fully silent turn qualifies.
    #[test]
    fn test_l2_complete_after_tool_call_without_verdict_yields_verdict_parse() {
        let mut m = mapper();
        let tool_line = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "toolRequest",
                    "toolCall": { "name": "verify_run", "arguments": { "command": "true" } }
                }]
            }
        })
        .to_string();
        m.map(&tool_line).unwrap();
        let err = m
            .map(&serde_json::json!({ "type": "complete" }).to_string())
            .unwrap_err();
        assert!(
            matches!(err, StreamMapError::VerdictParse(_)),
            "a turn that acted but emitted no verdict is VerdictParse, got {err:?}",
        );
    }

    /// A context-overflow `error` line → `ContextOverflow` (no retry).
    #[test]
    fn test_l2_context_overflow_error_yields_context_overflow() {
        let line = serde_json::json!({
            "type": "error",
            "error": "This model's maximum context length is 200000 tokens"
        })
        .to_string();
        let err = mapper().map(&line).unwrap_err();
        assert_eq!(err, StreamMapError::ContextOverflow);
    }

    /// A non-overflow `error` line → `Err(AgentError)` — a RETRYABLE failure
    /// (review C-cr-1). The mapper does NOT synthesize a terminal `Fail` /
    /// `ErrorEncountered` on the first occurrence: a transient provider error
    /// (HTTP 503, a rate-limit) must be retried, not hard-failed. The terminal
    /// events are synthesized by the goose runtime only after retry exhaustion.
    #[test]
    fn test_l2_non_overflow_error_is_retryable_agent_error() {
        let line = serde_json::json!({
            "type": "error",
            "error": "provider returned HTTP 503"
        })
        .to_string();
        let mut m = mapper();
        let err = m.map(&line).unwrap_err();
        let StreamMapError::AgentError(detail) = &err else {
            unreachable!("a non-overflow error must be a retryable AgentError, got {err:?}");
        };
        assert!(
            detail.contains("HTTP 503"),
            "the AgentError must carry the agent's error string, got {detail}",
        );
        // The mapper did NOT mark the stream complete — an agent error is
        // retryable, not terminal-on-first-occurrence.
        assert!(
            !m.seen_complete(),
            "a retryable agent error must not set the complete flag",
        );
    }

    /// A non-JSON line AFTER the stream has started (mid-stream) → `Transport`
    /// (loud, NOT skipped — review S8). A pre-stream non-JSON line (banner) is
    /// silently skipped; see `test_l2_banner_preamble_skipped_before_stream`.
    #[test]
    fn test_l2_corrupt_non_json_line_yields_transport_error() {
        let mut m = mapper();
        // Start the stream with a valid notification line.
        let notification = serde_json::json!({
            "type": "notification",
            "extension_id": "boi"
        })
        .to_string();
        m.map(&notification).unwrap();
        // Now a non-JSON line is mid-stream — must be a loud Transport error.
        let err = m.map("this is not json at all").unwrap_err();
        assert!(
            matches!(err, StreamMapError::Transport(_)),
            "a mid-stream non-JSON line must be a loud Transport error, got {err:?}",
        );
    }

    /// Goose startup banner preamble lines before the first JSON line are
    /// silently skipped; JSON events after the banner are mapped normally.
    ///
    /// Goose 1.34.1 prints a multi-line ASCII-art banner to stdout before the
    /// JSON stream begins. The `stream_started` flag allows the mapper to skip
    /// those lines without weakening the mid-stream S8 guarantee.
    #[test]
    fn test_l2_banner_preamble_skipped_before_stream() {
        let mut m = mapper();

        // 3 banner lines (plain ASCII — Goose also uses ASCII box-drawing, but
        // any non-JSON preamble line is tolerated) — all skipped, no error.
        assert!(
            m.map("   +------------------------------------------+")
                .unwrap()
                .is_empty()
        );
        assert!(
            m.map("   |         G O O S E  v1.34.1               |")
                .unwrap()
                .is_empty()
        );
        assert!(
            m.map("   +------------------------------------------+")
                .unwrap()
                .is_empty()
        );

        // A valid notification line after the banner — stream_started becomes true,
        // mapped as a normal (empty) notification.
        let notif =
            serde_json::json!({ "type": "notification", "extension_id": "boi" }).to_string();
        assert!(
            m.map(&notif).unwrap().is_empty(),
            "notification yields nothing"
        );

        // A valid tool-message after that — maps to a ToolInvoked.
        let tool_msg = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "toolRequest",
                    "toolCall": { "name": "verify_run", "arguments": {} }
                }]
            }
        })
        .to_string();
        let events = m.map(&tool_msg).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], BoiEvent::ToolInvoked { .. }));
    }

    /// A non-JSON line AFTER the first valid JSON line (mid-stream) still
    /// yields `Transport` — the S8 "no silent swallow" guarantee is unchanged
    /// for mid-stream lines; only pre-stream banner lines are skipped.
    #[test]
    fn test_l2_non_json_after_stream_started_yields_transport() {
        let mut m = mapper();

        // Start the stream with a valid notification.
        let notif = serde_json::json!({ "type": "notification" }).to_string();
        m.map(&notif).unwrap();

        // Mid-stream non-JSON → Transport, not silently skipped.
        let err = m.map("some corrupt output mid-stream").unwrap_err();
        assert!(
            matches!(err, StreamMapError::Transport(_)),
            "a mid-stream non-JSON line after the stream started must be Transport, got {err:?}",
        );
    }

    /// A `notification` event maps to nothing — a pure MCP log.
    #[test]
    fn test_l2_notification_event_maps_to_nothing() {
        let line = serde_json::json!({
            "type": "notification",
            "extension_id": "boi",
            "log": { "message": "tool ran" }
        })
        .to_string();
        let events = mapper().map(&line).unwrap();
        assert!(events.is_empty(), "a notification carries nothing for BOI");
    }

    /// A blank line is a no-op, not a `Transport` error (JSONL keep-alive).
    #[test]
    fn test_l1_blank_line_is_a_noop() {
        assert!(mapper().map("   ").unwrap().is_empty());
    }

    /// `last_json_object` isolates the LAST `{...}` from prose + JSON, and a
    /// brace inside a JSON string never miscounts.
    #[test]
    fn test_l1_last_json_object_isolates_trailing_object() {
        let text = r#"Here is an example {"x":1} and the real one {"synopsis":"a {brace} inside","outcome":1}"#;
        let got = last_json_object(text).unwrap();
        assert_eq!(got, r#"{"synopsis":"a {brace} inside","outcome":1}"#);
    }

    /// `token_split`: only `total` present → all of it attributed to input.
    #[test]
    fn test_l1_token_split_total_only_attributes_to_input() {
        assert_eq!(token_split(Some(500), None, None), (500, 0));
        assert_eq!(token_split(Some(500), Some(400), Some(100)), (400, 100));
        assert_eq!(token_split(None, None, None), (0, 0));
    }

    /// C-cr-3 — the verdict is the last STRICT-PARSEABLE `{...}` object in the
    /// accumulated transcript, not an anchor to "the last assistant message".
    ///
    /// Under Goose's delta + tool-call streaming, "the last assistant message"
    /// is not a robust anchor. A worker can emit its `WorkerVerdict` and then
    /// do a tool call (new `message.id`), or add a closing prose remark — the
    /// per-id model would clobber the verdict text with the later message and
    /// yield `VerdictParse`. The strict-parse scan across ALL accumulated text
    /// is what now preserves the C-cr-3 intent: a mid-task illustrative JSON
    /// object fails `deny_unknown_fields` and is skipped; only a complete,
    /// valid `WorkerVerdict` is accepted.
    ///
    /// Scenario: an early assistant message contains a complete, valid `Passing`
    /// verdict JSON; the FINAL assistant message is plain prose with no JSON.
    /// The correct outcome is `Passing` (the verdict from the early message IS
    /// the real verdict and is the last valid `WorkerVerdict` in the text).
    #[test]
    fn test_l2_verdict_is_from_the_last_assistant_message_not_an_early_one() {
        let mut m = mapper();
        // Message 1 (assistant): a complete, valid `Passing` WorkerVerdict —
        // the actual verdict the worker produced.
        let early = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "text",
                    "text": "Here is the verdict: {\"synopsis\":\"early verdict\",\"outcome\":{\"type\":\"passing\",\"evidence\":{\"files_touched\":[],\"verifications\":[],\"summary\":\"ok\"}}}"
                }]
            }
        })
        .to_string();
        m.map(&early).unwrap();
        // Message 2 (assistant): a later closing remark — plain prose, NO JSON.
        // Under the old per-id model this would clobber last_assistant_text and
        // cause VerdictParse; the new accumulate-all model retains both messages.
        let final_msg = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "All done — the task is complete." }]
            }
        })
        .to_string();
        m.map(&final_msg).unwrap();

        // `complete` → strict-parse scan finds the early verdict (the only
        // valid WorkerVerdict in the accumulated text) → Passing.
        let events = m
            .map(&serde_json::json!({ "type": "complete" }).to_string())
            .unwrap();
        assert_eq!(events.len(), 1);
        let BoiEvent::PhaseCompleted { verdict, .. } = &events[0] else {
            unreachable!("expected PhaseCompleted, got {:?}", events[0]);
        };
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "C-cr-3: the early valid verdict must be recovered even though a \
             later closing-remark message has no JSON — got {verdict:?}",
        );
    }

    /// A worker that emits a valid `WorkerVerdict` and THEN a `toolRequest`
    /// message (and a trailing prose message) — `extract_verdict` still
    /// recovers the verdict.
    ///
    /// This is the scenario that broke `critique_plan` in the smoke test: the
    /// worker emitted its verdict, then did a tool call (new `message.id`), and
    /// any text in the subsequent assistant turn clobbered `last_assistant_text`
    /// under the old per-id model → `VerdictParse`. The accumulate-all model
    /// retains all text and the strict-parse scan finds the verdict.
    #[test]
    fn test_l2_verdict_survives_tool_call_and_closing_remark_after_verdict() {
        let mut m = mapper();

        // Step 1: the worker emits its verdict plus a tool call in the same
        // assistant message.
        let verdict_and_tool = serde_json::json!({
            "type": "message",
            "message": {
                "id": "turn-1",
                "role": "assistant",
                "content": [
                    {
                        "type": "text",
                        "text": "{\"synopsis\":\"done\",\"outcome\":{\"type\":\"passing\",\"evidence\":{\"files_touched\":[\"src/lib.rs\"],\"verifications\":[],\"summary\":\"ci passed\"}}}"
                    },
                    {
                        "type": "toolRequest",
                        "toolCall": { "name": "worktree_diff", "arguments": {} }
                    }
                ]
            }
        })
        .to_string();
        let tool_events = m.map(&verdict_and_tool).unwrap();
        assert_eq!(tool_events.len(), 1, "one ToolInvoked from the toolRequest");

        // Step 2: tool response (role=tool, no text contribution).
        let tool_resp = serde_json::json!({
            "type": "message",
            "message": {
                "role": "tool",
                "content": [{ "type": "toolResponse" }]
            }
        })
        .to_string();
        m.map(&tool_resp).unwrap();

        // Step 3: closing remark from the assistant (new id, plain prose, no JSON).
        let closing = serde_json::json!({
            "type": "message",
            "message": {
                "id": "turn-2",
                "role": "assistant",
                "content": [{ "type": "text", "text": "Looks good — diff confirms the change." }]
            }
        })
        .to_string();
        m.map(&closing).unwrap();

        // complete → the verdict from step 1 must survive the tool call + closing remark.
        let events = m
            .map(&serde_json::json!({ "type": "complete" }).to_string())
            .unwrap();
        assert_eq!(events.len(), 1);
        let BoiEvent::PhaseCompleted { verdict, .. } = &events[0] else {
            unreachable!("expected PhaseCompleted, got {:?}", events[0]);
        };
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "the verdict must survive a tool call + closing remark after it — got {verdict:?}",
        );
    }

    /// A tool-role `text` block is NOT accumulated as the worker verdict —
    /// only an assistant message carries the verdict.
    #[test]
    fn test_l2_only_assistant_text_accumulates_as_verdict() {
        let mut m = mapper();
        // A tool-role message with text content — must NOT become the verdict.
        let tool_msg = serde_json::json!({
            "type": "message",
            "message": {
                "role": "tool",
                "content": [{ "type": "text", "text": "{\"not\":\"a verdict\"}" }]
            }
        })
        .to_string();
        m.map(&tool_msg).unwrap();
        // complete with only that tool text accumulated → no verdict found.
        let err = m
            .map(&serde_json::json!({ "type": "complete" }).to_string())
            .unwrap_err();
        assert!(
            matches!(err, StreamMapError::VerdictParse(_)),
            "tool-role text must not be treated as the worker verdict",
        );
    }
}
