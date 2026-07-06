# `boi dashboard` TUI Design

**Date:** 2026-05-21 — design rationale for the shipped `boi dashboard` command.
**Supersedes:** an earlier flat-text `boi status --watch` view.

---

## 1. Problem

BOI is slow and opaque. When a spec takes 40 minutes there is no way to see
*where* the time went, or — while it runs — *what it is doing right now*. The
only existing view, `boi status --watch`, is a flat text screen that redraws on
a 2 s tick: no navigation, no time attribution, no live detail.

The data to answer both questions already exists. BOI v2's event bus emits
`PhaseStarted`/`PhaseCompleted` (with `duration_ms`), `ToolInvoked`,
`DecisionMade`, `VerifyChecked`, `ErrorEncountered`. The OTel observer turns
those into spans; the `phase_runs` table records phase timing; canonical
OTLP/JSON traces land at `~/.boi/v2/traces/{date}/{trace_id}.jsonl`. What is
missing is a **human-navigable view** of that data.

## 2. Goals

- **Live-first.** Watch a running spec progress in real time — which task /
  phase / tool is active right now, with elapsed time ticking.
- **Find the culprit.** Identify where a run spends its time at a glance,
  without reading numbers — the longest bar is the problem.
- **Finished runs too.** Open a completed run and get the same informative
  view from its trace files.
- **High signal, not busy.** One screen, one level of detail expanded at a
  time. Everything is available; only what you drilled into is shown.

## 3. Non-goals (deferred — YAGNI)

- **Cross-cutting aggregated rollup.** A flat ranked list that totals a
  repeated op across the whole run (e.g. `cargo test ×8 = 4m08s` scattered
  across 3 phases). The sorted tree shows time *by structure* and is blind to
  death-by-a-thousand-cuts costs. Accepted tradeoff for v1. Add the rollup
  later *only if* a real culprit is hit that the tree-sort cannot surface.
- **Per-turn token attribution in the leaf-log expand detail.** Phase-level
  token counts and USD cost are shown in the bar-tree rows and the leaf-log
  footer (aggregated per phase from `phase_runs`, rolled up to task and spec).
  What is deferred is *per-turn* attribution: `gen_ai.usage.*` token counts are
  recorded on the parent `invoke_agent boi.worker` phase span, not on
  individual `chat` child spans. Attributing tokens to a specific turn would
  require walking up to the phase span and pro-rating across turns — that is
  deferred for v1.
- A separate web UI. This is a terminal tool.
- **Full tool output tail in the expand detail.** `boi.tool.args_summary` is a
  pre-call synopsis; no output excerpt is stored in the span. Full output is in
  the worker log, not the trace file. Deferred — out of scope for the trace
  reader.
- Mutating runtime state from the dashboard (cancel/unblock/etc.). It is
  strictly read-only; recovery actions stay on `boi cancel` / `unblock` /
  `fail` / `resolve-conflict`.

## 4. The one screen

A single navigable **bar-tree**. Every node — spec → task → phase → event — is
one row with a duration bar. The bar is split think-vs-do where the node has
children of both kinds.

```
 boi  S0042a · implement-api                    running · 14m22s   [waterfall]

   T2 implement-api    active    ████████████████░░░░    8m40s
     P5 implement      active    ███████████░░░░░░░░░    6m18s
       llm turn 1      done      █░░░░░░░░░░░░░░░░░░░    2m22s
       cargo check     done      ░░░░░░░░░░░░░░░░░░░░       4s
       llm turn 3      active    ███████░░░░░░░░░░░░░    3m08s
       cargo test      done      ██░░░░░░░░░░░░░░░░░░      31s
     P3 plan           done      ██░░░░░░░░░░░░░░░░░░    1m05s
   T1 routing-core     done      ████░░░░░░░░░░░░░░░░    2m10s
   T3 wire-cli         blocked   ░░░░░░░░░░░░░░░░░░░░       —
```

### 4.1 Sort modes (toggle: `s`)

The header shows the active mode. Sort is applied **per level** — children are
ordered within their parent.

- **Waterfall** *(default)* — nodes in start-time order. Stable: never
  reshuffles as the run progresses; live tail reads top-to-bottom like the run
  unfolds. Bars are offset along a shared timeline so sequencing and overlap
  are visible.
- **Duration** — same nodes re-sorted longest-first, bars left-aligned. The
  culprit-finder: the fattest bar floats to the top of every level. Walking
  the top row down each level walks the critical path.

Same tree, same bars, same drill — only row order changes. Waterfall to
*follow* the run; duration to *blame* it.

### 4.2 Bars

- A bar's full width = the node's wall-clock duration, scaled to the widest
  sibling at its level.
- Think vs. do split: the bar is two-toned. `llm` events count as **think**;
  tool-call events count as **do**; queued/blocked time counts as **idle**
  (rendered distinctly). A spec/task/phase bar aggregates its descendants'
  think/do/idle.
- A leaf event's bar is just its own duration.
- An active node shows a spinner glyph and a live-updating duration.
- Each tree row also shows the node's **USD cost** in a right-aligned column.
  Phase nodes carry their cost directly from `phase_runs.cost_usd`; task nodes
  sum their phases; the spec node sums its tasks. Leaf event nodes show no cost
  (per-turn attribution is deferred — see §3).

## 5. Navigation

One uniform rule. Granularity always equals drill depth.

| Key      | Action |
|----------|--------|
| `↑` `↓`  | Move selection within the current level |
| `⏎`      | Drill into the selected node (one level deeper) |
| `⎋`      | Back out one level |
| `s`      | Toggle sort: waterfall ⇄ duration |
| `q`      | Quit |

Drill path: **spec → task → phase → event → event detail**. At each step the
header breadcrumb updates (`S0042a › T2 implement-api › P5 implement`) and the
view shows only the selected subtree.

## 6. Leaf = live streaming log

Drilling into a single **phase** (or deeper) switches the right/lower region
from bars to a **streaming log** — the chronological event stream for that
node, auto-following the live tail.

```
 boi  S0042a › T2 implement-api › P5 implement        active · 6m18s

   ▸ llm   turn 1           think 2m22s    42k→1.1k tok
   ▸ bash  cargo check                           4.2s  ✓
   ▾ edit  src/api/handler.rs                     0.3s
       +24 −6   fn handle_route() rewritten
   ▸ llm   turn 3           think 3m08s    11k→1.4k tok
   ▾ bash  cargo test                             31s  ✗
       test api::route::splits ... FAILED
  ⠿ edit  src/api/handler.rs                    running
   ──────────────────────────────────────────────────────
   phase 6m18s   ·   think 5m41s (90%)   ·   tools 37s (10%)   ·   $1.50   ·   4.8k tok
```

- One terse line per event by default (`▸` collapsed).
- `⏎` on a line toggles its detail (`▾`). What the detail shows depends on the
  span type:
  - **Tool call (`execute_tool`)** — the tool name plus an args synopsis when
    available (`boi.tool` + `boi.tool.args_summary` attributes on BOI-native
    spans; `gen_ai.tool.name` on hoover-normalised worker spans). Example:
    `Bash: cargo test --lib`. A full output tail is **not** stored in the span;
    this is limited to the pre-call summary BOI records.
  - **LLM turn (`chat`)** — the model name (`gen_ai.request.model`). Example:
    `claude-opus-4-7`. Per-turn token counts are not shown here — they are
    recorded on the parent `invoke_agent` phase span, not on individual `chat`
    spans (per-turn attribution is deferred — see §3).
- One level expanded at a time keeps it from getting busy.
- The active line carries the spinner; the log auto-scrolls to the tail.
- The footer gives the phase's think/do split, USD cost, and total token count
  (input + output) — the at-a-glance "is the model the bottleneck, and what did
  it cost?" answer. Cost and tokens come from `phase_runs.cost_usd`,
  `tokens_in`, and `tokens_out` for the focused phase.

## 7. Data source

`boi dashboard` is a **read-only** command, consistent with `boi status` / `log`
/ `traces` / `spec show` — it reads SQLite + trace files directly and **does not
require the daemon**.

**Recommendation: poll SQLite + tail the trace JSONL.** Rejected alternative:
subscribe to the daemon event bus over the control socket.

| | Poll + tail *(chosen)* | Daemon bus subscription |
|---|---|---|
| Daemon changes | None | New read-side socket protocol + per-subscriber fan-out |
| Live + finished | One code path — finished is "tail hits EOF" | Live only; finished still needs a file reader |
| Daemon down | Still works (finished runs, late inspection) | Dead |
| Latency | Poll interval (~500 ms) + file flush | Sub-second |

Sub-second streaming is overkill for a human watching impatiently; ~500 ms is
imperceptibly live. Coupling the TUI to daemon liveness would also kill
finished-run viewing whenever the daemon is down. Mechanism:

- **Structure** (the bar-tree) — poll `phase_runs` + `task_runtime` + spec
  state from SQLite every ~500 ms (read-only connection, WAL mode). Rebuild the
  node tree; diff against the prior tree to avoid flicker.
- **Leaf log** — tail the active spec's `~/.boi/v2/traces/{date}/{trace_id}.jsonl`
  (append-only OTLP/JSON written by `otel_export`). Parse new lines into log
  events.
- **Finished runs** — identical readers; the tail simply reaches EOF and the
  spec's terminal status stops the poll loop.

**Risk:** if `otel_export` batches writes heavily, the live log lags by one
batch. Mitigation noted for the implementation plan — the active trace may need
a smaller batch or flush-on-phase-event. Not a blocker; structure timing comes
from SQLite regardless.

## 8. CLI surface

```
boi dashboard [SPEC_ID]
```

- **No `SPEC_ID`** — opens the spec-picker screen: a list of specs, running
  first then most-recently-completed, auto-refreshing every ~500 ms. `↑`/`↓`
  moves the highlighted row; `⏎` enters the selected spec's focused bar-tree
  view; `⎋` from the spec root returns to the picker; `q` quits.
- **`SPEC_ID` given** — opens directly on that spec's focused bar-tree view,
  bypassing the picker. `⎋` at the spec root returns to the picker.
- **Non-TTY fallback.** When stdout is not a TTY (pipe, CI), `boi dashboard`
  without a `SPEC_ID` prints a plain-text spec list (one line per spec:
  `spec_id  [status]  N phases  $cost`). With a `SPEC_ID` it prints the
  one-shot static tree snapshot — this preserves the scripting use case the
  removed `boi status` served.

### 8.1 Removing `boi status`

`boi status` is deleted, not kept in parallel:

- Delete `src/cli/status.rs` (including the `--watch` redraw loop).
- Remove `Command::Status` from `src/cli/mod.rs`.
- Update help text / docs referencing `boi status`.
- The non-TTY snapshot fallback (8) covers the lost scripting path.

## 9. Architecture

`boi dashboard` is layer-5 CLI code (`src/cli/dashboard/`). It imports the
`repo` layer for SQLite reads and a trace reader; it spawns no subprocess.

| Unit | Responsibility | Depends on |
|---|---|---|
| `cli/dashboard/mod.rs` | Subcommand entry; TTY detection; snapshot vs. TUI | `model`, `render`, `repo` |
| `cli/dashboard/model.rs` | `DashNode` tree type; build tree from `phase_runs` + events; think/do/idle aggregation; the two sort orderings | `repo`, `types` |
| `cli/dashboard/poll.rs` | ~500 ms SQLite poll + trace-JSONL tail; emits tree + log-event deltas | `repo`, trace reader |
| `cli/dashboard/render.rs` | `ratatui` widgets — bar-tree, breadcrumb header, leaf log; bar/think-do drawing | `ratatui`, `model` |
| `cli/dashboard/input.rs` | Keymap → navigation state machine (selection, drill depth, sort mode, line expansion) | — |
| `cli/dashboard/snapshot.rs` | Non-TTY static tree print | `model` |

**Tech:** `ratatui` + `crossterm` — the standard Rust TUI stack. New
non-default-feature-gated? No — the dashboard is a core read command; the deps
are pure-Rust and light. (Contrast `duckdb`, gated for its C++ build cost.)

**State model:** a single `DashState` — the `DashNode` tree, the selection path
(drill stack), sort mode, per-line expansion set, and the leaf log buffer. The
poll task sends tree/log deltas over a channel; the input task sends key
events; the render loop folds both into `DashState` and redraws.

## 10. Error handling

- A failed poll tick prints a non-fatal status line in the footer and the loop
  continues (matches `boi status --watch`'s existing non-fatal-tick behavior).
- A missing / unreadable trace file degrades the leaf log to "no trace data"
  but the structural tree (from SQLite) still renders — loud, not silent.
- Terminal restored on `Ctrl-C` / panic via a guard, so a crash never leaves
  the terminal in raw mode.
- An unknown / nonexistent `SPEC_ID` exits non-zero with a clear message.

## 11. Testing

- **`model.rs`** — unit tests: tree built from fixture `phase_runs` rows;
  think/do/idle aggregation; both sort orderings; live-update tree diffing.
- **`input.rs`** — unit tests: keymap state machine — drill in/out, selection
  bounds, sort toggle, line expansion.
- **`snapshot.rs`** — golden-file test of the non-TTY static render.
- **`render.rs`** — `ratatui`'s `TestBackend`: render `DashState` fixtures to a
  buffer, assert on cell content (bar widths, breadcrumb, spinner placement).
- **Integration** — run a fixture spec to completion, point the dashboard's
  readers at its DB + trace files, assert the final tree matches expected
  durations.

## 12. Open questions

None blocking. The `otel_export` flush-cadence risk (7) is the one item the
implementation plan must check early — if batching is coarse, the active trace
needs a tighter flush. Resolve during Phase 1 of the plan, not now.
