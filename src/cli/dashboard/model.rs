//! The `DashNode` tree — the dashboard's in-memory model of a spec.
//!
//! Three levels come from the `phase_runs` SQLite table (spec → task →
//! phase); the fourth (events inside a phase) is merged in from the trace
//! JSONL by [`merge_events`]. Every node carries a [`TimeSplit`].

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};

use crate::cli::dashboard::trace::{EventKind, LogEvent, PhaseKey};
use crate::repo::phase_runs::PhaseRunRow;
use crate::types::state::{SpecStatus, TaskState};

/// How a node's wall-clock time divides into think / do / idle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimeSplit {
    /// Milliseconds spent in LLM turns.
    pub think_ms: u64,
    /// Milliseconds spent in tool calls.
    pub do_ms: u64,
    /// Milliseconds spent queued / blocked / unaccounted.
    pub idle_ms: u64,
}

impl TimeSplit {
    /// Total milliseconds across all three buckets.
    pub fn total_ms(&self) -> u64 {
        self.think_ms + self.do_ms + self.idle_ms
    }
}

impl std::ops::Add for TimeSplit {
    type Output = TimeSplit;

    /// Sum two splits componentwise.
    fn add(self, other: TimeSplit) -> TimeSplit {
        TimeSplit {
            think_ms: self.think_ms + other.think_ms,
            do_ms: self.do_ms + other.do_ms,
            idle_ms: self.idle_ms + other.idle_ms,
        }
    }
}

/// Token counts for a node — per-phase from `phase_runs`, aggregated for
/// task and spec nodes. Leaf event nodes carry the default (zero): per-turn
/// token attribution is not available from trace data.
///
/// Per the 2026-06-01 directive ("strip dollars everywhere, keep tokens
/// everywhere") the per-node dollar field was removed; the struct now
/// carries plain `u64` token counts only.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Tokens {
    /// Input tokens consumed.
    pub tokens_in: u64,
    /// Output tokens produced.
    pub tokens_out: u64,
}

impl Tokens {
    /// Total tokens (in + out).
    pub fn total_tokens(&self) -> u64 {
        self.tokens_in + self.tokens_out
    }
}

impl std::ops::Add for Tokens {
    type Output = Tokens;

    fn add(self, other: Tokens) -> Tokens {
        Tokens {
            tokens_in: self.tokens_in + other.tokens_in,
            tokens_out: self.tokens_out + other.tokens_out,
        }
    }
}

/// What kind of thing a [`DashNode`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// The spec root.
    Spec,
    /// A task.
    Task,
    /// A phase run.
    Phase,
    /// An LLM turn (a leaf, merged from the trace).
    LlmTurn,
    /// A tool call (a leaf, merged from the trace).
    ToolCall,
}

/// One node of the dashboard tree.
#[derive(Debug, Clone)]
pub struct DashNode {
    /// What this node is.
    pub kind: NodeKind,
    /// Display label (task id, phase name, tool name, …).
    pub label: String,
    /// Short status word (`active` / `done` / `blocked` / `failed` / `—`).
    pub status: String,
    /// When the node started.
    pub started_at: DateTime<Utc>,
    /// When it finished — `None` while in progress.
    pub completed_at: Option<DateTime<Utc>>,
    /// Think/do/idle breakdown (own time for leaves; aggregated for branches).
    pub split: TimeSplit,
    /// Phase identity key used to correlate trace [`LogEvent`]s.
    /// `Some` only for `Phase` nodes; `None` for all others.
    pub phase_key: Option<PhaseKey>,
    /// Expanded-detail text shown when a leaf log line is expanded.
    /// Empty string for non-leaf nodes; set from [`LogEvent::detail`] for
    /// `LlmTurn` and `ToolCall` leaves.
    pub detail: String,
    /// Token counts. Per-phase from `phase_runs` for `Phase` nodes;
    /// summed from children for `Task`/`Spec`; zero for leaf event nodes.
    /// (Per the 2026-06-01 strip-$ directive no dollar field rides here;
    /// tokens stay as the spend-hint signal.)
    pub cost: Tokens,
    /// The task's author-supplied `ref` slug, when known. Populated for
    /// `Task` nodes whose `task_runtime` row carries a `ref`; `None`
    /// elsewhere. The renderer pairs this with `behavior` to produce a
    /// human-readable label shown next to the task ID.
    pub task_ref: Option<String>,
    /// The task's spec-authored `behavior` string, when known. Populated for
    /// `Task` nodes from the spec's snapshot (`spec_versions.snapshot.tasks`);
    /// `None` elsewhere. Used by the renderer as a fallback label when
    /// `task_ref` is `None`.
    pub behavior: Option<String>,
    /// Child nodes.
    pub children: Vec<DashNode>,
}

impl DashNode {
    /// Wall-clock duration in milliseconds. Uses `now` for in-progress nodes.
    pub fn duration_ms(&self, now: DateTime<Utc>) -> u64 {
        let end = self.completed_at.unwrap_or(now);
        (end - self.started_at).num_milliseconds().max(0) as u64
    }
}

/// Build the spec → task → phase tree from a spec's `phase_runs` rows.
///
/// Rows with `task_id = None` are spec-level phases and hang directly off the
/// spec root. The spec root's `started_at` is the earliest row start; its
/// `completed_at` is `None` if any row is still open, else the latest end.
pub fn build_tree(spec_id: &str, rows: &[PhaseRunRow]) -> DashNode {
    // Phases grouped by task_id (None => spec-level).
    let mut by_task: BTreeMap<Option<String>, Vec<DashNode>> = BTreeMap::new();

    for row in rows {
        let phase = DashNode {
            kind: NodeKind::Phase,
            label: format!("{} (iter {})", row.phase, row.phase_iteration),
            status: phase_status(row),
            started_at: row.started_at,
            completed_at: row.completed_at,
            split: TimeSplit::default(), // filled by merge_events (Task 5)
            phase_key: Some(PhaseKey {
                task_id: row.task_id.clone(),
                phase: row.phase.clone(),
                iteration: row.phase_iteration,
            }),
            detail: String::new(),
            cost: Tokens {
                tokens_in: row.tokens_in.unwrap_or(0).max(0) as u64,
                tokens_out: row.tokens_out.unwrap_or(0).max(0) as u64,
            },
            task_ref: None,
            behavior: None,
            children: Vec::new(),
        };
        by_task.entry(row.task_id.clone()).or_default().push(phase);
    }

    let mut children = Vec::new();
    for (task_id, phases) in by_task {
        match task_id {
            None => children.extend(phases), // spec-level phases
            Some(tid) => children.push(task_node(tid, phases)),
        }
    }

    let started_at = rows
        .iter()
        .map(|r| r.started_at)
        .min()
        .unwrap_or_else(Utc::now);
    let any_open = rows.iter().any(|r| r.completed_at.is_none());
    let completed_at = if any_open {
        None
    } else {
        rows.iter().filter_map(|r| r.completed_at).max()
    };

    let cost = children
        .iter()
        .fold(Tokens::default(), |acc, c| acc + c.cost);

    // A spec with ZERO phase_runs rows is queued (dispatch wrote the spec but
    // workspace_prepare hasn't been scheduled yet). Without this guard the
    // vacuous `any_open == false` reports `[done]` — terminal-by-vacuous-truth
    // — and waiters like the E2E `wait_for_spec` see the spec as settled
    // within milliseconds of dispatch. S6 — bias toward NOT lying about state.
    let status = if rows.is_empty() {
        "queued"
    } else if any_open {
        "running"
    } else {
        "done"
    };

    DashNode {
        kind: NodeKind::Spec,
        label: spec_id.to_string(),
        status: status.to_string(),
        started_at,
        completed_at,
        split: TimeSplit::default(),
        phase_key: None,
        detail: String::new(),
        cost,
        task_ref: None,
        behavior: None,
        children,
    }
}

/// Wrap a task's phases in a `Task` node spanning their time range.
fn task_node(task_id: String, phases: Vec<DashNode>) -> DashNode {
    let started_at = phases
        .iter()
        .map(|p| p.started_at)
        .min()
        .unwrap_or_else(Utc::now);
    let any_open = phases.iter().any(|p| p.completed_at.is_none());
    let completed_at = if any_open {
        None
    } else {
        phases.iter().filter_map(|p| p.completed_at).max()
    };
    let cost = phases.iter().fold(Tokens::default(), |acc, p| acc + p.cost);
    DashNode {
        kind: NodeKind::Task,
        label: task_id,
        status: if any_open { "active" } else { "done" }.to_string(),
        started_at,
        completed_at,
        split: TimeSplit::default(),
        phase_key: None,
        detail: String::new(),
        cost,
        task_ref: None,
        behavior: None,
        children: phases,
    }
}

/// Row ordering for the bar-tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    /// Start-time order — stable, never reshuffles. The default.
    Waterfall,
    /// Longest-duration-first — the culprit-finder.
    Duration,
}

impl SortMode {
    /// The mode reached by pressing `s`.
    pub fn toggled(self) -> SortMode {
        match self {
            SortMode::Waterfall => SortMode::Duration,
            SortMode::Duration => SortMode::Waterfall,
        }
    }
}

/// Recursively sort every node's children by `mode`. `now` resolves the
/// duration of in-progress nodes for `Duration` sort.
pub fn sort_tree(node: &mut DashNode, mode: SortMode, now: DateTime<Utc>) {
    match mode {
        SortMode::Waterfall => node.children.sort_by_key(|c| c.started_at),
        SortMode::Duration => {
            node.children
                .sort_by_key(|c| std::cmp::Reverse(c.duration_ms(now)));
        }
    }
    for child in &mut node.children {
        sort_tree(child, mode, now);
    }
}

/// Attach trace events as leaf children of their phase nodes, then roll the
/// resulting think/do/idle split up through phases, tasks, and the spec.
///
/// `now` resolves in-progress durations. A phase's `idle_ms` is its wall-clock
/// duration minus the think+do its events account for (never negative).
pub fn merge_events(tree: &mut DashNode, events: &[LogEvent], now: DateTime<Utc>) {
    attach_and_split(tree, events, now);
}

/// Recursive worker for [`merge_events`].
fn attach_and_split(node: &mut DashNode, events: &[LogEvent], now: DateTime<Utc>) {
    if node.kind == NodeKind::Phase {
        let Some(phase_key) = node.phase_key.as_ref() else {
            return;
        };
        let mut split = TimeSplit::default();
        for ev in events.iter().filter(|e| &e.phase == phase_key) {
            let dur = ev.duration_ms(now);
            match ev.kind {
                EventKind::Think => split.think_ms += dur,
                EventKind::Do => split.do_ms += dur,
            }
            node.children.push(DashNode {
                kind: match ev.kind {
                    EventKind::Think => NodeKind::LlmTurn,
                    EventKind::Do => NodeKind::ToolCall,
                },
                label: ev.label.clone(),
                status: if ev.completed_at.is_some() {
                    "done"
                } else {
                    "active"
                }
                .to_string(),
                started_at: ev.started_at,
                completed_at: ev.completed_at,
                split: TimeSplit {
                    think_ms: if ev.kind == EventKind::Think { dur } else { 0 },
                    do_ms: if ev.kind == EventKind::Do { dur } else { 0 },
                    idle_ms: 0,
                },
                phase_key: None,
                detail: ev.detail.clone(),
                cost: Tokens::default(),
                task_ref: None,
                behavior: None,
                children: Vec::new(),
            });
        }
        let wall = node.duration_ms(now);
        split.idle_ms = wall.saturating_sub(split.think_ms + split.do_ms);
        node.split = split;
        return;
    }
    // Branch node: recurse, then sum children's splits.
    let mut total = TimeSplit::default();
    for child in &mut node.children {
        attach_and_split(child, events, now);
        total = total + child.split;
    }
    node.split = total;
}

/// Override task/spec node statuses with the authoritative `blocked` signal
/// from `task_runtime`.
///
/// The structural tree from `phase_runs` cannot see a blocked task: a blocked
/// task's phases are closed, so [`build_tree`] reports the task `done` and — if
/// it's the last open task — the whole spec `done`, hiding a *wedged* spec as
/// "all done" (the 2026-06-11 incident — a blocked spec rendered
/// indistinguishable from a finished one).
///
/// `states` maps `task_id` → the task's `task_runtime.state` string. Any task
/// whose state is `blocked` is forced to a loud `blocked` status, and a spec
/// with ANY blocked task is itself `blocked`. Blocked dominates `running`/`done`
/// so the dashboard never lies about a halted spec; `queued` (a spec with zero
/// phase rows, hence no task children) is left intact.
///
/// `spec_status` is the AUTHORITATIVE `spec_runtime.status`. The override is
/// applied ONLY when the spec is actually `running` — a TERMINAL spec
/// (`failed` / `canceled` / `completed`) may legitimately retain a `blocked`
/// task row as a forensic record, and forcing `[blocked]` onto it would imply
/// it is revivable via `boi unblock` when it is dead. For a terminal spec the
/// structural status is left untouched.
pub fn apply_task_states(
    tree: &mut DashNode,
    states: &HashMap<String, String>,
    spec_status: SpecStatus,
) {
    if tree.kind != NodeKind::Spec || spec_status != SpecStatus::Running {
        return;
    }
    let mut any_blocked = false;
    for task in &mut tree.children {
        if task.kind != NodeKind::Task {
            continue;
        }
        if states.get(&task.label).map(String::as_str) == Some(TaskState::Blocked.as_str()) {
            task.status = "blocked".to_string();
            any_blocked = true;
        }
    }
    if any_blocked {
        tree.status = "blocked".to_string();
    }
}

/// One-word status for a phase row.
fn phase_status(row: &PhaseRunRow) -> String {
    if row.completed_at.is_none() {
        "active".to_string()
    } else {
        match row.worker_verdict() {
            Ok(Some(_)) => "done".to_string(),
            _ => "done".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    /// A `PhaseRunRow` fixture builder for tests.
    fn row(task: Option<&str>, phase: &str, start_s: i64, end_s: Option<i64>) -> PhaseRunRow {
        PhaseRunRow {
            id: format!("P{phase:0>8}"),
            spec_id: "S0000001".to_string(),
            task_id: task.map(str::to_string),
            phase: phase.to_string(),
            phase_iteration: 1,
            spec_version: 1,
            provider: "claude_code".to_string(),
            worker_id: None,
            files_touched: json!([]),
            synopsis: String::new(),
            verdict: None,
            last_heartbeat_at: None,
            started_at: Utc.timestamp_opt(start_s, 0).unwrap(),
            completed_at: end_s.map(|s| Utc.timestamp_opt(s, 0).unwrap()),
            tokens_in: None,
            tokens_out: None,
        }
    }

    #[test]
    fn duration_sort_puts_the_longest_child_first() {
        let rows = vec![
            row(Some("T1"), "short", 100, Some(110)), // 10s
            row(Some("T1"), "long", 200, Some(400)),  // 200s
        ];
        let mut tree = build_tree("S0000001", &rows);
        let now = Utc.timestamp_opt(400, 0).unwrap();

        sort_tree(&mut tree, SortMode::Duration, now);
        let phases = &tree.children[0].children;
        assert!(phases[0].label.starts_with("long"), "longest first");

        sort_tree(&mut tree, SortMode::Waterfall, now);
        let phases = &tree.children[0].children;
        assert!(phases[0].label.starts_with("short"), "earliest start first");
    }

    #[test]
    fn apply_task_states_marks_blocked_task_and_spec() {
        // T1's only phase is closed → build_tree calls the task `done` and, as
        // the sole task, the spec `done`. The blocked signal must override both
        // so a wedged spec is not shown as finished.
        let rows = vec![row(Some("T1"), "plan", 100, Some(160))];
        let mut tree = build_tree("S0000001", &rows);
        assert_eq!(tree.status, "done", "precondition: closed phases ⇒ done");

        let mut states = HashMap::new();
        states.insert("T1".to_string(), "blocked".to_string());
        apply_task_states(&mut tree, &states, SpecStatus::Running);

        assert_eq!(tree.children[0].status, "blocked", "task forced blocked");
        assert_eq!(tree.status, "blocked", "spec blocked dominates done");
    }

    #[test]
    fn apply_task_states_skips_a_terminal_spec_with_a_retained_blocked_task() {
        // A failed/canceled spec may keep a `blocked` task row as a forensic
        // record. Forcing `[blocked]` would imply it is revivable — it is dead.
        let rows = vec![row(Some("T1"), "plan", 100, Some(160))];
        let mut tree = build_tree("S0000001", &rows);
        assert_eq!(tree.status, "done", "structural status for closed phases");

        let mut states = HashMap::new();
        states.insert("T1".to_string(), "blocked".to_string());
        for terminal in [
            SpecStatus::Failed,
            SpecStatus::Canceled,
            SpecStatus::Completed,
        ] {
            let mut t = tree.clone();
            apply_task_states(&mut t, &states, terminal);
            assert_eq!(
                t.status, "done",
                "terminal spec keeps its structural status"
            );
            assert_eq!(
                t.children[0].status, "done",
                "terminal spec task not forced blocked"
            );
        }
        // Sanity: the running case still overrides.
        apply_task_states(&mut tree, &states, SpecStatus::Running);
        assert_eq!(
            tree.status, "blocked",
            "a running spec still surfaces blocked"
        );
    }

    #[test]
    fn apply_task_states_blocked_dominates_a_running_sibling() {
        let rows = vec![
            row(Some("T1"), "plan", 100, Some(160)), // closed
            row(Some("T2"), "implement", 160, None), // open ⇒ running
        ];
        let mut tree = build_tree("S0000001", &rows);
        assert_eq!(tree.status, "running");

        let mut states = HashMap::new();
        states.insert("T1".to_string(), "blocked".to_string());
        states.insert("T2".to_string(), "active".to_string());
        apply_task_states(&mut tree, &states, SpecStatus::Running);

        assert_eq!(tree.status, "blocked", "any blocked task ⇒ spec blocked");
    }

    #[test]
    fn apply_task_states_without_a_blocked_task_is_a_noop() {
        let rows = vec![row(Some("T1"), "implement", 160, None)];
        let mut tree = build_tree("S0000001", &rows);
        let before = tree.status.clone();

        let mut states = HashMap::new();
        states.insert("T1".to_string(), "active".to_string());
        apply_task_states(&mut tree, &states, SpecStatus::Running);

        assert_eq!(tree.status, before, "no blocked task ⇒ status unchanged");
    }

    #[test]
    fn build_tree_groups_phases_under_their_task() {
        let rows = vec![
            row(Some("T1"), "plan", 100, Some(160)),
            row(Some("T1"), "implement", 160, None),
        ];
        let tree = build_tree("S0000001", &rows);

        assert_eq!(tree.kind, NodeKind::Spec);
        assert_eq!(tree.children.len(), 1, "one task");
        let task = &tree.children[0];
        assert_eq!(task.kind, NodeKind::Task);
        assert_eq!(task.label, "T1");
        assert_eq!(task.children.len(), 2, "two phases");
        assert_eq!(task.status, "active", "an open phase keeps the task active");
        assert_eq!(tree.status, "running");
    }

    #[test]
    fn build_tree_rolls_phase_tokens_up_to_the_spec() {
        let mut r1 = row(Some("T1"), "plan", 100, Some(160));
        r1.tokens_in = Some(1_000);
        r1.tokens_out = Some(200);
        let mut r2 = row(Some("T1"), "implement", 160, Some(400));
        r2.tokens_in = Some(4_000);
        r2.tokens_out = Some(800);

        let tree = build_tree("S0000001a", &[r1, r2]);

        // Spec total = sum of both phases. (Per the 2026-06-01 strip-$
        // directive the per-node dollar field is gone — tokens stay.)
        assert_eq!(tree.cost.tokens_in, 5_000);
        assert_eq!(tree.cost.tokens_out, 1_000);
        // Task node rolls up the same (one task owns both phases).
        assert_eq!(tree.children[0].cost.tokens_in, 5_000);
    }

    #[test]
    fn merge_events_rolls_think_do_up_to_the_spec() {
        use crate::cli::dashboard::trace::{EventKind, LogEvent};
        let rows = vec![row(Some("T1"), "implement", 100, Some(200))]; // 100s wall
        let mut tree = build_tree("S0000001", &rows);
        let phase_key = tree.children[0].children[0]
            .phase_key
            .clone()
            .expect("phase node must have phase_key");
        let now = Utc.timestamp_opt(200, 0).unwrap();
        let events = vec![LogEvent {
            kind: EventKind::Think,
            phase: phase_key,
            label: "llm turn".to_string(),
            started_at: Utc.timestamp_opt(100, 0).unwrap(),
            completed_at: Some(Utc.timestamp_opt(160, 0).unwrap()), // 60s think
            detail: String::new(),
        }];
        merge_events(&mut tree, &events, now);

        assert_eq!(tree.split.think_ms, 60_000, "60s think rolled to spec");
        assert_eq!(
            tree.split.idle_ms, 40_000,
            "100s wall − 60s think = 40s idle"
        );
    }
}
