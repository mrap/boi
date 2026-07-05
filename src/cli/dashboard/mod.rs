//! `boi dashboard` — the read-only spec-observability TUI.
//!
//! Layer 5 (`cli/`). Reads SQLite (`phase_runs`) for structure and tails the
//! OTLP trace JSONL for per-phase events. No daemon, no subprocess.
//!
//! Replaces the removed `boi status` (decision:
//! `boi-status-replaced-by-dashboard-2026-05-21`).

// Submodules are `pub` so their `pub` items are genuine `boi` lib API: this
// keeps the `unreachable_pub` lint quiet (the items are reachable) and lets
// the Task 14 integration test reach them via `boi::cli::dashboard::*`.
pub mod input;
pub mod model;
pub mod picker;
pub mod poll;
pub mod render;
pub mod snapshot;
pub mod state;
pub mod trace;

use std::io;

use chrono::Utc;
use crossterm::ExecutableCommand;
use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::cli::dashboard::model::{NodeKind, SortMode};
use crate::cli::dashboard::state::{DashAction, DashState, Screen, SpecScreen};
use crate::cli::read_error::ReadError;
use crate::types::ids::SpecId;

/// `boi dashboard [SPEC_ID]` entry point.
///
/// Detects whether stdout is a TTY: a TTY starts the interactive TUI; a
/// non-TTY (pipe / CI) prints a one-shot static snapshot instead.
pub async fn run(spec_id: Option<&str>) -> Result<(), ReadError> {
    if crossterm::tty::IsTty::is_tty(&std::io::stdout()) {
        run_tui(spec_id).await
    } else {
        snapshot::run(spec_id).await
    }
}

/// A RAII guard: restores the terminal on drop, even on panic.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        disable_raw_mode().ok();
        io::stdout().execute(LeaveAlternateScreen).ok();
    }
}

/// The interactive TUI event loop.
///
/// Opens the SQLite pool, enters the alternate screen behind a RAII guard,
/// then runs a `tokio::select!` loop over crossterm key events and a 500 ms
/// poll tick. When no `spec_id` is given, opens the picker screen first.
async fn run_tui(spec_id: Option<&str>) -> Result<(), ReadError> {
    let pool = poll::open_pool().await?;

    // Build the initial state: picker when no spec_id, focused spec otherwise.
    let mut state = if let Some(raw) = spec_id {
        let sid = SpecId::new(raw).map_err(|_| ReadError::BadId(raw.to_string()))?;
        let trace_path = poll::trace_path_for(&sid)?;
        let tree = poll::build_snapshot(&pool, &sid, &trace_path, SortMode::Waterfall).await?;
        let title = poll::fetch_spec_title(&pool, &sid).await;
        let mut s = DashState::spec(sid, tree);
        if let crate::cli::dashboard::state::Screen::Spec(ref mut spec) = s.screen {
            spec.title = title;
        }
        s
    } else {
        let specs = poll::build_spec_list(&pool).await?;
        DashState::picker(specs)
    };

    enable_raw_mode().map_err(io_err)?;
    // TermGuard restores the terminal on exit or panic — bound immediately after
    // enable_raw_mode so it covers EnterAlternateScreen and everything below.
    let _guard = TermGuard;
    io::stdout().execute(EnterAlternateScreen).map_err(io_err)?;
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout())).map_err(io_err)?;

    let mut keys = EventStream::new();
    let mut ticker = tokio::time::interval(poll::POLL_INTERVAL);

    loop {
        // Redraw.
        term.draw(|f| {
            let now = Utc::now();
            let area = f.area();
            match &state.screen {
                Screen::Spec(s) => {
                    let err = state.last_poll_error.as_deref();
                    if matches!(
                        s.focused().kind,
                        NodeKind::Phase | NodeKind::LlmTurn | NodeKind::ToolCall
                    ) {
                        render::draw_log(f, area, s, err, now);
                    } else {
                        render::draw_tree(f, area, s, state.sort, err, now);
                    }
                }
                Screen::Picker(p) => {
                    render::draw_overview(f, area, p, now);
                }
            }
        })
        .map_err(io_err)?;

        if state.quit {
            break;
        }

        tokio::select! {
            // Key events.
            maybe_ev = keys.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_ev {
                    if key.kind == KeyEventKind::Press {
                        if let Some(action) = input::map_key(key) {
                            // Detect screen transitions BEFORE apply — they need DB I/O.
                            let transition_handled =
                                handle_transition(&pool, &mut state, action).await;
                            if !transition_handled {
                                state.apply(action);
                            }
                        }
                    }
                }
            }
            // Poll tick: rebuild state for the active screen.
            _ = ticker.tick() => {
                match &state.screen {
                    Screen::Picker(_) => {
                        match poll::build_spec_list(&pool).await {
                            Ok(fresh) => {
                                state.last_poll_error = None;
                                if let Screen::Picker(p) = &mut state.screen {
                                    // Clamp selected to the new length.
                                    if !fresh.is_empty() && p.selected >= fresh.len() {
                                        p.selected = fresh.len() - 1;
                                    }
                                    p.specs = fresh;
                                }
                            }
                            Err(e) => {
                                state.last_poll_error = Some(format!("poll failed: {e}"));
                            }
                        }
                    }
                    Screen::Spec(s) => {
                        // Re-resolve the trace path each tick for the current spec.
                        let sid = s.spec_id.clone();
                        match poll::trace_path_for(&sid) {
                            Ok(trace_path) => {
                                match poll::build_snapshot(&pool, &sid, &trace_path, state.sort).await {
                                    Ok(fresh) => {
                                        state.last_poll_error = None;
                                        if let Screen::Spec(s) = &mut state.screen {
                                            clamp_spec_navigation(s, &fresh);
                                            s.tree = fresh;
                                        }
                                    }
                                    Err(e) => {
                                        state.last_poll_error = Some(format!("poll failed: {e}"));
                                    }
                                }
                            }
                            Err(e) => {
                                state.last_poll_error = Some(format!("trace resolve failed: {e}"));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Handle a screen transition if the action + current screen requires one.
///
/// Returns `true` if the transition was handled (caller must NOT call
/// `state.apply`); `false` if the action is normal within-screen navigation.
async fn handle_transition(
    pool: &sqlx::SqlitePool,
    state: &mut DashState,
    action: DashAction,
) -> bool {
    match action {
        DashAction::DrillIn => {
            if let Screen::Picker(p) = &state.screen {
                let selected = p.selected;
                if let Some(summary) = p.specs.get(selected) {
                    let raw = summary.spec_id.clone();
                    enter_spec(pool, state, &raw).await;
                }
                return true;
            }
            false
        }
        DashAction::BackOut => {
            if let Screen::Spec(s) = &state.screen {
                if s.drill.is_empty() {
                    back_to_picker(pool, state).await;
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Transition from Picker → Spec: load a snapshot for `raw_spec_id` and swap
/// the screen. On error, sets `last_poll_error` and stays on the picker.
async fn enter_spec(pool: &sqlx::SqlitePool, state: &mut DashState, raw_spec_id: &str) {
    let sid = match SpecId::new(raw_spec_id) {
        Ok(s) => s,
        Err(_) => {
            state.last_poll_error = Some(format!("invalid spec id: {raw_spec_id}"));
            return;
        }
    };
    let trace_path = match poll::trace_path_for(&sid) {
        Ok(p) => p,
        Err(e) => {
            state.last_poll_error = Some(format!("trace resolve failed: {e}"));
            return;
        }
    };
    match poll::build_snapshot(pool, &sid, &trace_path, state.sort).await {
        Ok(tree) => {
            state.last_poll_error = None;
            let title = poll::fetch_spec_title(pool, &sid).await;
            state.screen = Screen::Spec(Box::new(SpecScreen::with_title(sid, title, tree)));
        }
        Err(e) => {
            state.last_poll_error = Some(format!("load failed: {e}"));
        }
    }
}

/// Transition from Spec → Picker: reload the spec list and swap the screen.
/// On error, sets `last_poll_error` and stays on the spec screen.
async fn back_to_picker(pool: &sqlx::SqlitePool, state: &mut DashState) {
    match poll::build_spec_list(pool).await {
        Ok(specs) => {
            state.last_poll_error = None;
            state.screen =
                Screen::Picker(crate::cli::dashboard::state::PickerScreen { specs, selected: 0 });
        }
        Err(e) => {
            state.last_poll_error = Some(format!("reload failed: {e}"));
        }
    }
}

/// After a poll rebuild, clamp `drill` and `selected` on a `SpecScreen` so
/// they still point at real nodes (the tree may have grown or a node may have
/// closed).
fn clamp_spec_navigation(
    spec: &mut crate::cli::dashboard::state::SpecScreen,
    fresh: &crate::cli::dashboard::model::DashNode,
) {
    let mut node = fresh;
    let mut valid_depth = 0;
    for &idx in &spec.drill {
        if idx < node.children.len() {
            node = &node.children[idx];
            valid_depth += 1;
        } else {
            break;
        }
    }
    spec.drill.truncate(valid_depth);
    // Walk the new tree along the (clamped) drill path to get the focused node.
    let mut focused = fresh;
    for &idx in &spec.drill {
        focused = &focused.children[idx];
    }
    let count = focused.children.len();
    if count == 0 {
        spec.selected = 0;
    } else if spec.selected >= count {
        spec.selected = count - 1;
    }
}

/// Wrap an `io::Error` into the shared `ReadError`.
fn io_err(e: io::Error) -> ReadError {
    ReadError::BadId(format!("terminal I/O error: {e}"))
}
