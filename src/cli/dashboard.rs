use crate::fmt::{elapsed_since, ensure_db_dir, time_ago, truncate, BOLD, CYAN, DIM, GREEN, RED, RESET, YELLOW};
use crate::queue;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::collections::HashMap;
use std::io::{self, Write};
use std::time::{Duration, Instant};

const BAR_WIDTH: usize = 8;
const BAR_VIS: usize = BAR_WIDTH + 2;

fn compact_bar(completed: i64, total: i64) -> String {
    let filled = if total == 0 {
        0
    } else {
        (((completed as f64) / (total as f64)) * (BAR_WIDTH as f64)).round() as usize
    };
    let filled = filled.min(BAR_WIDTH);
    let empty = BAR_WIDTH - filled;
    format!(
        "[{}{}{}{}{}]",
        YELLOW,
        "█".repeat(filled),
        DIM,
        "░".repeat(empty),
        RESET
    )
}

fn parse_deps(depends: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(depends).unwrap_or_default()
}

fn task_elapsed_str(started_at: Option<&str>, completed_at: Option<&str>, status: &str) -> String {
    match status {
        "DONE" | "FAILED" | "SKIPPED" => {
            if let (Some(start), Some(end)) = (started_at, completed_at) {
                if let (Ok(s), Ok(e)) = (
                    chrono::DateTime::parse_from_rfc3339(start),
                    chrono::DateTime::parse_from_rfc3339(end),
                ) {
                    let secs = e.signed_duration_since(s).num_seconds().max(0);
                    return if secs < 60 {
                        format!("{}s", secs)
                    } else {
                        format!("{}m", secs / 60)
                    };
                }
            }
            String::new()
        }
        "RUNNING" => started_at.map(elapsed_since).unwrap_or_default(),
        _ => String::new(),
    }
}

struct DashState {
    running: Vec<queue::SpecRecord>,
    queued: Vec<queue::SpecRecord>,
    finished: Vec<queue::SpecRecord>,
    current_tasks: HashMap<String, (String, String)>,
    workers_total: usize,
    workers_busy: usize,
    last_fetch: Instant,
    selected: usize,
    show_finished: bool,
    show_all: bool,
    search_mode: bool,
    search_query: String,
    detail_spec_id: Option<String>,
    detail_tasks: Vec<queue::TaskRecord>,
}

impl DashState {
    fn new() -> Self {
        DashState {
            running: Vec::new(),
            queued: Vec::new(),
            finished: Vec::new(),
            current_tasks: HashMap::new(),
            workers_total: 0,
            workers_busy: 0,
            last_fetch: Instant::now() - Duration::from_secs(10),
            selected: 0,
            show_finished: false,
            show_all: false,
            search_mode: false,
            search_query: String::new(),
            detail_spec_id: None,
            detail_tasks: Vec::new(),
        }
    }

    fn refresh(&mut self, db_str: &str) {
        ensure_db_dir(db_str);
        self.last_fetch = Instant::now();
        let q = match queue::Queue::open(db_str) {
            Ok(q) => q,
            Err(_) => return,
        };

        let specs = match q.status_all() {
            Ok(s) => s,
            Err(_) => return,
        };

        let workers = q.get_workers().unwrap_or_default();
        self.workers_total = workers.len();
        self.workers_busy = workers.iter().filter(|w| w.current_spec_id.is_some()).count();

        self.current_tasks.clear();
        for worker in &workers {
            if let (Some(spec_id), Some(task_id)) = (&worker.current_spec_id, &worker.current_task_id) {
                if let Ok(tasks) = q.get_tasks(spec_id) {
                    if let Some(task) = tasks.iter().find(|t| &t.id == task_id) {
                        self.current_tasks.insert(spec_id.clone(), (task_id.clone(), task.title.clone()));
                    }
                }
            }
        }

        let mut running = Vec::new();
        let mut queued = Vec::new();
        let mut finished = Vec::new();
        for s in specs {
            match s.status.as_str() {
                "running" => running.push(s),
                "queued" => queued.push(s),
                "completed" | "failed" | "cancelled" => finished.push(s),
                _ => {}
            }
        }
        finished.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));

        self.running = running;
        self.queued = queued;
        self.finished = finished;

        let vis = self.visible_count();
        if vis > 0 && self.selected >= vis {
            self.selected = vis - 1;
        }

        // Refresh task list if in detail view
        if let Some(spec_id) = self.detail_spec_id.clone() {
            if let Ok(tasks) = q.get_tasks(&spec_id) {
                self.detail_tasks = tasks;
            }
        }
    }

    fn visible_finished(&self) -> Vec<&queue::SpecRecord> {
        if self.show_all {
            return self.finished.iter().collect();
        }
        if self.show_finished {
            let cutoff = chrono::Utc::now() - chrono::Duration::hours(6);
            return self
                .finished
                .iter()
                .filter(|s| {
                    s.completed_at
                        .as_deref()
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map(|dt| dt.with_timezone(&chrono::Utc) >= cutoff)
                        .unwrap_or(false)
                })
                .collect();
        }
        vec![]
    }

    fn filter_match(&self, s: &queue::SpecRecord) -> bool {
        if self.search_query.is_empty() {
            return true;
        }
        let q = self.search_query.to_lowercase();
        s.title.to_lowercase().contains(&q) || s.id.to_lowercase().contains(&q)
    }

    fn visible_count(&self) -> usize {
        let r = self.running.iter().filter(|s| self.filter_match(s)).count();
        let q = self.queued.iter().filter(|s| self.filter_match(s)).count();
        let f = self
            .visible_finished()
            .iter()
            .filter(|s| self.filter_match(s))
            .count();
        r + q + f
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn move_down(&mut self) {
        let max = self.visible_count().saturating_sub(1);
        if self.selected < max {
            self.selected += 1;
        }
    }

    fn clamp_selection(&mut self) {
        let vis = self.visible_count();
        if vis > 0 && self.selected >= vis {
            self.selected = vis - 1;
        }
    }

    fn selected_spec_id(&self) -> Option<String> {
        let mut idx = 0;
        for s in self.running.iter().filter(|s| self.filter_match(s)) {
            if idx == self.selected {
                return Some(s.id.clone());
            }
            idx += 1;
        }
        for s in self.queued.iter().filter(|s| self.filter_match(s)) {
            if idx == self.selected {
                return Some(s.id.clone());
            }
            idx += 1;
        }
        let finished = self.visible_finished();
        for s in finished.iter().filter(|s| self.filter_match(s)) {
            if idx == self.selected {
                return Some(s.id.clone());
            }
            idx += 1;
        }
        None
    }

    fn detail_spec_record(&self) -> Option<&queue::SpecRecord> {
        let spec_id = self.detail_spec_id.as_deref()?;
        self.running
            .iter()
            .find(|s| s.id == spec_id)
            .or_else(|| self.queued.iter().find(|s| s.id == spec_id))
            .or_else(|| self.finished.iter().find(|s| s.id == spec_id))
    }

    fn enter_detail(&mut self, db_str: &str) {
        if let Some(spec_id) = self.selected_spec_id() {
            if let Ok(q) = queue::Queue::open(db_str) {
                if let Ok(tasks) = q.get_tasks(&spec_id) {
                    self.detail_tasks = tasks;
                    self.detail_spec_id = Some(spec_id);
                }
            }
        }
    }

    fn exit_detail(&mut self) {
        self.detail_spec_id = None;
        self.detail_tasks.clear();
    }
}

fn render_detail(state: &DashState, stdout: &mut impl Write) {
    let _ = execute!(stdout, cursor::MoveTo(0, 0));
    let _ = execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
    );

    let width: usize = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(110);

    let mut out = String::new();

    let spec = match state.detail_spec_record() {
        Some(s) => s,
        None => {
            out.push_str(&format!(
                "{}spec not found — press esc to go back{}\r\n",
                DIM, RESET
            ));
            let _ = write!(stdout, "{}", out);
            let _ = stdout.flush();
            return;
        }
    };

    // ── Header ────────────────────────────────────────────────────────────────
    let total = spec.total_tasks.unwrap_or(0);
    let done_count = spec.completed_tasks;
    let right_str = format!("{}/{} tasks  mode:{}", done_count, total, spec.mode);
    let right_vis = right_str.len();
    let id_vis = spec.id.len() + 2; // id + 2 spaces
    let title_budget = width.saturating_sub(id_vis + right_vis + 2);
    let title_str = truncate(&spec.title, title_budget);
    let title_vis = title_str.chars().count();
    let gap = width.saturating_sub(id_vis + title_vis + right_vis);

    out.push_str(&format!(
        "{}{}{}  {}{}{}{}{}  {}{}{}\r\n",
        BOLD,
        spec.id,
        RESET,
        title_str,
        " ".repeat(gap),
        DIM,
        right_str,
        RESET,
        "",
        "",
        ""
    ));

    // ── Divider ───────────────────────────────────────────────────────────────
    out.push_str(&format!("{}{}{}\r\n", DIM, "─".repeat(width.min(60)), RESET));

    // ── Tasks ─────────────────────────────────────────────────────────────────
    use std::collections::HashSet;
    let done_ids: HashSet<&str> = state
        .detail_tasks
        .iter()
        .filter(|t| matches!(t.status.as_str(), "DONE" | "SKIPPED"))
        .map(|t| t.id.as_str())
        .collect();

    if state.detail_tasks.is_empty() {
        out.push_str(&format!("{}  no tasks{}\r\n", DIM, RESET));
    }

    for task in &state.detail_tasks {
        let icon_str = match task.status.as_str() {
            "DONE" | "SKIPPED" => format!("{}✓{}", GREEN, RESET),
            "FAILED" => format!("{}✗{}", RED, RESET),
            "RUNNING" => format!("{}▸{}", YELLOW, RESET),
            _ => format!("{}○{}", DIM, RESET),
        };

        let is_blocked = task.status == "PENDING"
            && parse_deps(&task.depends)
                .iter()
                .any(|d| !done_ids.contains(d.as_str()));

        let timing = task_elapsed_str(
            task.started_at.as_deref(),
            task.completed_at.as_deref(),
            &task.status,
        );

        let blocked_suffix = if is_blocked { "  (blocked)" } else { "" };
        let right_str = if timing.is_empty() {
            format!("{}{}", task.status, blocked_suffix)
        } else {
            format!("{}  {}{}", task.status, timing, blocked_suffix)
        };
        let right_vis = right_str.len();

        // prefix visible width: icon(1) + 2 spaces + id.len() + 2 spaces
        let prefix_vis = 1 + 2 + task.id.len() + 2;
        let title_budget = width.saturating_sub(prefix_vis + right_vis + 2);
        let task_title = truncate(&task.title, title_budget);
        let title_vis = task_title.chars().count();
        let spaces = width.saturating_sub(prefix_vis + title_vis + right_vis).max(2);

        out.push_str(&format!(
            "{}  {}{}  {}{}{}{}\r\n",
            icon_str,
            task.id,
            RESET,
            task_title,
            " ".repeat(spaces),
            DIM,
            right_str
        ));
        out.push_str(RESET);
    }

    out.push_str("\r\n");

    // ── Footer ────────────────────────────────────────────────────────────────
    out.push_str(&format!("{}esc:back  l:log{}\r\n", DIM, RESET));

    let _ = write!(stdout, "{}", out);
    let _ = stdout.flush();
}

fn render_list(state: &DashState, stdout: &mut impl Write) {
    let _ = execute!(stdout, cursor::MoveTo(0, 0));
    let _ = execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
    );

    let width: usize = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(110);

    let mut out = String::new();
    let mut global_idx: usize = 0;

    // ── Header ────────────────────────────────────────────────────────────────
    let header_left = format!("{}{}BOI{}", BOLD, CYAN, RESET);
    let header_left_vis: usize = 3;

    let (header_right, header_right_vis) = if state.workers_total > 0 {
        let s = format!("{}/{} workers busy", state.workers_busy, state.workers_total);
        let vis = s.len();
        (format!("{}{}{}", DIM, s, RESET), vis)
    } else {
        (String::new(), 0)
    };

    let header_gap = width.saturating_sub(header_left_vis + header_right_vis);
    out.push_str(&format!(
        "{}{}{}\r\n\r\n",
        header_left,
        " ".repeat(header_gap),
        header_right
    ));

    // Filtered views
    let running_vis: Vec<&queue::SpecRecord> = state
        .running
        .iter()
        .filter(|s| state.filter_match(s))
        .collect();
    let queued_vis: Vec<&queue::SpecRecord> = state
        .queued
        .iter()
        .filter(|s| state.filter_match(s))
        .collect();
    let finished_vis: Vec<&queue::SpecRecord> = state
        .visible_finished()
        .into_iter()
        .filter(|s| state.filter_match(s))
        .collect();

    // ── Empty state ───────────────────────────────────────────────────────────
    if running_vis.is_empty() && queued_vis.is_empty() && finished_vis.is_empty() {
        if !state.search_query.is_empty() {
            out.push_str(&format!(
                "{}no matches for '{}{}{}'{}\r\n",
                DIM, RESET, state.search_query, DIM, RESET
            ));
        } else {
            out.push_str(&format!("{}queue is empty{}\r\n", DIM, RESET));
        }
    }

    // ── RUNNING section ───────────────────────────────────────────────────────
    if !running_vis.is_empty() {
        out.push_str(&format!("{}{}RUNNING{}\r\n", BOLD, YELLOW, RESET));
        for s in &running_vis {
            let is_selected = global_idx == state.selected;
            global_idx += 1;

            let total = s.total_tasks.unwrap_or(0);
            let done = s.completed_tasks;
            let elapsed = s
                .started_at
                .as_deref()
                .map(elapsed_since)
                .unwrap_or_else(|| "?".to_string());

            let fraction = format!("{}/{}", done, total);
            let bar = compact_bar(done, total);
            let right_vis = fraction.len() + 2 + BAR_VIS + 2 + elapsed.len();
            let right = format!("{}  {}  {}", fraction, bar, elapsed);

            // ▶ when selected, ▸ otherwise
            let cursor_char = if is_selected { "\u{25b6}" } else { "\u{25b8}" };

            let prefix_vis: usize = 9;
            let title_budget = width.saturating_sub(prefix_vis + right_vis + 2);
            let title_str = truncate(&s.title, title_budget);
            let title_vis = title_str.chars().count();
            let spaces = width.saturating_sub(prefix_vis + title_vis + right_vis);

            out.push_str(&format!(
                "{}{} {:<5}{}  {}{}{}\r\n",
                YELLOW,
                cursor_char,
                s.id,
                RESET,
                title_str,
                " ".repeat(spaces),
                right
            ));

            if let Some((task_id, task_title)) = state.current_tasks.get(&s.id) {
                let task_prefix_vis = 4 + task_id.chars().count() + 2;
                let status_str = "executing...";
                let task_title_budget =
                    width.saturating_sub(task_prefix_vis + status_str.len() + 4);
                let task_title_str = truncate(task_title, task_title_budget);
                let task_title_vis = task_title_str.chars().count();
                let task_spaces =
                    width.saturating_sub(task_prefix_vis + task_title_vis + status_str.len());

                out.push_str(&format!(
                    "{}  \u{2192} {}: {}{}{}{}  {}{}\r\n",
                    DIM,
                    task_id,
                    RESET,
                    task_title_str,
                    " ".repeat(task_spaces),
                    DIM,
                    status_str,
                    RESET
                ));
            }
        }
        out.push_str("\r\n");
    }

    // ── QUEUED section ────────────────────────────────────────────────────────
    if !queued_vis.is_empty() {
        out.push_str(&format!(
            "{}{}QUEUED ({}){}\r\n",
            BOLD,
            CYAN,
            queued_vis.len(),
            RESET
        ));
        for s in &queued_vis {
            let is_selected = global_idx == state.selected;
            global_idx += 1;

            // ▶ when selected, ◦ otherwise
            let cursor_char = if is_selected { "\u{25b6}" } else { "\u{25e6}" };

            let title_budget = width.saturating_sub(9);
            let title_str = truncate(&s.title, title_budget);
            out.push_str(&format!(
                "{}{} {:<5}{}  {}\r\n",
                CYAN, cursor_char, s.id, RESET, title_str
            ));
        }
        out.push_str("\r\n");
    }

    // ── FINISHED section ──────────────────────────────────────────────────────
    if !finished_vis.is_empty() {
        let label = if state.show_all { "ALL" } else { "FINISHED" };
        out.push_str(&format!(
            "{}{}{}  ({}){}\r\n",
            BOLD,
            DIM,
            label,
            finished_vis.len(),
            RESET
        ));
        for s in &finished_vis {
            let is_selected = global_idx == state.selected;
            global_idx += 1;

            let cursor_char = if is_selected { "\u{25b6}" } else { " " };

            let status_icon = match s.status.as_str() {
                "completed" => format!("{}✓{}", GREEN, RESET),
                "failed" => format!("{}✗{}", RED, RESET),
                _ => format!("{}⊘{}", DIM, RESET),
            };
            let elapsed = s
                .completed_at
                .as_deref()
                .map(time_ago)
                .unwrap_or_default();

            // status_icon is 1 visible char; elapsed varies
            let right_vis = 1 + 2 + elapsed.len();
            let right = format!("{}  {}", status_icon, elapsed);

            let prefix_vis: usize = 9;
            let title_budget = width.saturating_sub(prefix_vis + right_vis + 2);
            let title_str = truncate(&s.title, title_budget);
            let title_vis = title_str.chars().count();
            let spaces = width.saturating_sub(prefix_vis + title_vis + right_vis);

            out.push_str(&format!(
                "{}{} {:<5}{}  {}{}{}\r\n",
                DIM,
                cursor_char,
                s.id,
                RESET,
                title_str,
                " ".repeat(spaces),
                right
            ));
        }
        out.push_str("\r\n");
    }

    // ── Footer ────────────────────────────────────────────────────────────────
    if state.search_mode {
        out.push_str(&format!(
            "{}/ {}{}{}_{}  esc:clear  enter:apply{}\r\n",
            CYAN, RESET, state.search_query, CYAN, RESET, RESET
        ));
    } else {
        let hints = if state.show_all {
            "q:quit  j/k:nav  f:finished  a:hide all  /:search  enter:detail"
        } else if state.show_finished {
            "q:quit  j/k:nav  f:hide  a:all  /:search  enter:detail"
        } else {
            "q:quit  j/k:nav  f:finished  a:all  /:search  enter:detail"
        };
        out.push_str(&format!("{}{}{}\r\n", DIM, hints, RESET));
    }

    let _ = write!(stdout, "{}", out);
    let _ = stdout.flush();
}

fn render(state: &DashState, stdout: &mut impl Write) {
    if state.detail_spec_id.is_some() {
        render_detail(state, stdout);
    } else {
        render_list(state, stdout);
    }
}

pub fn run_dashboard(db_path: &str) {
    let mut stdout = io::stdout();

    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
        orig_hook(info);
    }));

    enable_raw_mode().expect("failed to enable raw mode");
    execute!(stdout, EnterAlternateScreen, cursor::Hide).expect("failed to enter alternate screen");

    let mut state = DashState::new();
    state.refresh(db_path);
    render(&state, &mut stdout);

    let poll_interval = Duration::from_millis(200);
    let db_interval = Duration::from_secs(2);
    let mut last_db_poll = Instant::now();
    let mut log_spec_id: Option<String> = None;

    'main: loop {
        if last_db_poll.elapsed() >= db_interval {
            state.refresh(db_path);
            render(&state, &mut stdout);
            last_db_poll = Instant::now();
        }

        if event::poll(poll_interval).unwrap_or(false) {
            if let Ok(ev) = event::read() {
                let mut needs_render = false;

                match ev {
                    // Ctrl+C always quits
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('c'),
                        modifiers,
                        ..
                    }) if modifiers.contains(KeyModifiers::CONTROL) => {
                        break 'main;
                    }

                    // Quit (list mode only)
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('q'),
                        ..
                    }) if !state.search_mode && state.detail_spec_id.is_none() => {
                        break 'main;
                    }

                    // Esc: exit detail view, or exit search mode
                    Event::Key(KeyEvent {
                        code: KeyCode::Esc,
                        ..
                    }) => {
                        if state.detail_spec_id.is_some() {
                            state.exit_detail();
                        } else if state.search_mode {
                            state.search_mode = false;
                            state.search_query.clear();
                            state.clamp_selection();
                        }
                        needs_render = true;
                    }

                    // 'l': tail log for the spec shown in detail view
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('l'),
                        ..
                    }) if !state.search_mode && state.detail_spec_id.is_some() => {
                        log_spec_id = state.detail_spec_id.clone();
                        break 'main;
                    }

                    // Enter search mode
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('/'),
                        ..
                    }) if !state.search_mode && state.detail_spec_id.is_none() => {
                        state.search_mode = true;
                        needs_render = true;
                    }

                    // Apply search filter and exit search mode
                    Event::Key(KeyEvent {
                        code: KeyCode::Enter,
                        ..
                    }) if state.search_mode => {
                        state.search_mode = false;
                        needs_render = true;
                    }

                    // Enter: open detail view for selected spec
                    Event::Key(KeyEvent {
                        code: KeyCode::Enter,
                        ..
                    }) if !state.search_mode && state.detail_spec_id.is_none() => {
                        state.enter_detail(db_path);
                        needs_render = true;
                    }

                    // Backspace in search mode
                    Event::Key(KeyEvent {
                        code: KeyCode::Backspace,
                        ..
                    }) if state.search_mode => {
                        state.search_query.pop();
                        state.clamp_selection();
                        needs_render = true;
                    }

                    // Printable chars in search mode → append to query
                    Event::Key(KeyEvent {
                        code: KeyCode::Char(c),
                        modifiers,
                        ..
                    }) if state.search_mode && !modifiers.contains(KeyModifiers::CONTROL) => {
                        state.search_query.push(c);
                        state.clamp_selection();
                        needs_render = true;
                    }

                    // j / Down arrow: move selection down (list mode only)
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('j'),
                        ..
                    }) if !state.search_mode && state.detail_spec_id.is_none() => {
                        state.move_down();
                        needs_render = true;
                    }
                    Event::Key(KeyEvent {
                        code: KeyCode::Down,
                        ..
                    }) if !state.search_mode && state.detail_spec_id.is_none() => {
                        state.move_down();
                        needs_render = true;
                    }

                    // k / Up arrow: move selection up (list mode only)
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('k'),
                        ..
                    }) if !state.search_mode && state.detail_spec_id.is_none() => {
                        state.move_up();
                        needs_render = true;
                    }
                    Event::Key(KeyEvent {
                        code: KeyCode::Up,
                        ..
                    }) if !state.search_mode && state.detail_spec_id.is_none() => {
                        state.move_up();
                        needs_render = true;
                    }

                    // f: toggle finished (list mode only)
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('f'),
                        ..
                    }) if !state.search_mode && state.detail_spec_id.is_none() => {
                        if state.show_finished {
                            state.show_finished = false;
                        } else {
                            state.show_finished = true;
                            state.show_all = false;
                        }
                        state.clamp_selection();
                        needs_render = true;
                    }

                    // a: toggle show-all (list mode only)
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('a'),
                        ..
                    }) if !state.search_mode && state.detail_spec_id.is_none() => {
                        if state.show_all {
                            state.show_all = false;
                        } else {
                            state.show_all = true;
                            state.show_finished = false;
                        }
                        state.clamp_selection();
                        needs_render = true;
                    }

                    // r: force refresh (works in all modes)
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('r'),
                        ..
                    }) if !state.search_mode => {
                        state.refresh(db_path);
                        last_db_poll = Instant::now();
                        needs_render = true;
                    }

                    Event::Resize(_, _) => {
                        needs_render = true;
                    }

                    _ => {}
                }

                if needs_render {
                    render(&state, &mut stdout);
                }
            }
        }
    }

    execute!(stdout, LeaveAlternateScreen, cursor::Show)
        .expect("failed to leave alternate screen");
    disable_raw_mode().expect("failed to disable raw mode");

    if let Some(spec_id) = log_spec_id {
        let boi_bin =
            std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("boi"));
        let _ = std::process::Command::new(boi_bin)
            .args(["log", &spec_id, "--follow"])
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::{plan_layout, LayoutPlan, MIN_FINISHED};

    // height=60: all three sections have items well under budget
    #[test]
    fn tall_terminal_all_visible() {
        let p = plan_layout(60, 2, 0, 5, 30);
        assert_eq!(p.run_show, 2);
        assert_eq!(p.que_show, 5);
        assert_eq!(p.fin_show, 30);
        assert_eq!(p.run_hidden, 0);
        assert_eq!(p.que_hidden, 0);
        assert_eq!(p.fin_hidden, 0);
    }

    // height=20: all running visible, queued shrinks, finished >= MIN_FINISHED
    #[test]
    fn short_terminal_priority() {
        let p = plan_layout(20, 4, 0, 8, 30);
        assert_eq!(p.run_show, 4, "all running must be visible");
        assert!(p.fin_show >= MIN_FINISHED, "finished must show at least MIN_FINISHED");
        // queued is deprioritised and shrinks
        assert!(p.que_show < 8);
        // hidden counts must be consistent
        assert_eq!(p.run_hidden, 4 - p.run_show);
        assert_eq!(p.que_hidden, 8 - p.que_show);
        assert_eq!(p.fin_hidden, 30 - p.fin_show);
    }

    // height=10: very tight — running is trimmed, finished gets whatever fits
    #[test]
    fn tiny_terminal() {
        let p = plan_layout(10, 5, 0, 0, 20);
        // running must be trimmed
        assert!(p.run_hidden > 0, "some running items should be hidden");
        // queued was already 0
        assert_eq!(p.que_show, 0);
        assert_eq!(p.que_hidden, 0);
        // finished either gets MIN_FINISHED or fewer if truly no room
        assert!(p.fin_show + p.fin_hidden == 20);
    }

    // empty: all inputs zero → all outputs zero
    #[test]
    fn empty_state() {
        let p = plan_layout(40, 0, 0, 0, 0);
        assert_eq!(p, LayoutPlan { run_show: 0, que_show: 0, fin_show: 0, run_hidden: 0, que_hidden: 0, fin_hidden: 0 });
    }

    // only finished items — they fill nearly the whole content budget
    #[test]
    fn only_finished() {
        let p = plan_layout(30, 0, 0, 0, 50);
        // run and queued are empty
        assert_eq!(p.run_show, 0);
        assert_eq!(p.que_show, 0);
        // finished fills close to entire budget (content=27, overhead=2, budget=25, fin_show=24)
        assert!(p.fin_show >= 20, "finished should fill most of the screen, got {}", p.fin_show);
        assert_eq!(p.fin_hidden, 50 - p.fin_show);
    }

    // exact_fit: height=19, running=2, queued=3, finished=5 — no truncation
    #[test]
    fn exact_fit() {
        let p = plan_layout(19, 2, 0, 3, 5);
        assert_eq!(p.run_show, 2);
        assert_eq!(p.que_show, 3);
        assert_eq!(p.fin_show, 5);
        assert_eq!(p.run_hidden, 0);
        assert_eq!(p.que_hidden, 0);
        assert_eq!(p.fin_hidden, 0);
    }

    // resize_smaller: same data, smaller height → counts decrease monotonically
    #[test]
    fn resize_smaller() {
        let big = plan_layout(30, 2, 0, 5, 30);
        let small = plan_layout(10, 2, 0, 5, 30);
        assert!(small.run_show <= big.run_show, "run_show must not increase on shrink");
        assert!(small.que_show <= big.que_show, "que_show must not increase on shrink");
        assert!(small.fin_show <= big.fin_show, "fin_show must not increase on shrink");
    }
}
