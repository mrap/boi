//! `DashState` — the dashboard's two-screen interaction state and the pure
//! navigation transitions over it. Kept free of `ratatui` / `crossterm` so it
//! is unit-testable without a terminal.

use std::collections::HashSet;

use crate::cli::dashboard::model::{DashNode, SortMode};

/// One discrete user intent, produced by `input::map_key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashAction {
    /// Move the selection up within the current level.
    Up,
    /// Move the selection down within the current level.
    Down,
    /// Drill into the selected node (or, in picker, open the selected spec).
    DrillIn,
    /// Back out one level (or, in spec at root, return to picker).
    BackOut,
    /// Toggle waterfall ⇄ duration sort.
    ToggleSort,
    /// Toggle the selected log line's expanded detail.
    ToggleExpand,
    /// Quit.
    Quit,
}

/// Which screen the dashboard is showing.
pub enum Screen {
    /// The spec picker — a list of specs.
    Picker(PickerScreen),
    /// A single spec's focused bar-tree view.
    Spec(Box<SpecScreen>),
}

/// Picker-screen state.
pub struct PickerScreen {
    /// The spec rows, already ordered by `picker::build_spec_list`.
    pub specs: Vec<crate::cli::dashboard::picker::SpecSummary>,
    /// Index of the highlighted row.
    pub selected: usize,
}

/// Focused-spec-screen state — the existing focused-view fields.
pub struct SpecScreen {
    /// Which spec this screen shows (drives the poll loop).
    pub spec_id: crate::types::ids::SpecId,
    /// The spec's human-readable title, sourced from
    /// `spec_versions.snapshot.title`. `None` when no snapshot has been
    /// written or the snapshot omits a title — the renderer shows only the ID
    /// in that case.
    pub title: Option<String>,
    /// The current tree.
    pub tree: DashNode,
    /// Drill path of child indices from the root.
    pub drill: Vec<usize>,
    /// Selected child within the drilled-into node.
    pub selected: usize,
    /// Expanded log-line indices within the focused node.
    pub expanded: HashSet<usize>,
}

impl SpecScreen {
    /// Create a new `SpecScreen` with an empty drill path pointing at the root.
    pub fn new(spec_id: crate::types::ids::SpecId, tree: DashNode) -> Self {
        SpecScreen {
            spec_id,
            title: None,
            tree,
            drill: Vec::new(),
            selected: 0,
            expanded: HashSet::new(),
        }
    }

    /// Create a new `SpecScreen` with a known title — used by callers that
    /// have already resolved the spec's `spec_versions.snapshot.title`.
    pub fn with_title(
        spec_id: crate::types::ids::SpecId,
        title: Option<String>,
        tree: DashNode,
    ) -> Self {
        SpecScreen {
            spec_id,
            title,
            tree,
            drill: Vec::new(),
            selected: 0,
            expanded: HashSet::new(),
        }
    }

    /// The node the drill path points at.
    pub fn focused(&self) -> &DashNode {
        let mut node = &self.tree;
        for &idx in &self.drill {
            node = &node.children[idx];
        }
        node
    }
}

/// The dashboard's full interaction state.
pub struct DashState {
    /// The active screen.
    pub screen: Screen,
    /// Sort mode for the focused spec view (the picker is always
    /// running-first / recent order).
    pub sort: SortMode,
    /// Set once the user asks to quit.
    pub quit: bool,
    /// Most recent poll-tick failure, shown in the header; cleared on success.
    pub last_poll_error: Option<String>,
}

impl DashState {
    /// A state opened on the picker screen with the given spec list.
    pub fn picker(specs: Vec<crate::cli::dashboard::picker::SpecSummary>) -> DashState {
        DashState {
            screen: Screen::Picker(PickerScreen { specs, selected: 0 }),
            sort: SortMode::Waterfall,
            quit: false,
            last_poll_error: None,
        }
    }

    /// A state opened directly on a spec's focused view.
    pub fn spec(spec_id: crate::types::ids::SpecId, tree: DashNode) -> DashState {
        DashState {
            screen: Screen::Spec(Box::new(SpecScreen {
                spec_id,
                title: None,
                tree,
                drill: Vec::new(),
                selected: 0,
                expanded: HashSet::new(),
            })),
            sort: SortMode::Waterfall,
            quit: false,
            last_poll_error: None,
        }
    }

    /// Apply one within-screen navigation action. Screen transitions
    /// (Picker⏎→Spec, Spec⎋-at-root→Picker) are handled by `run_tui`, which
    /// needs DB I/O — `apply` is pure.
    pub fn apply(&mut self, action: DashAction) {
        match &mut self.screen {
            Screen::Picker(p) => match action {
                DashAction::Up => p.selected = p.selected.saturating_sub(1),
                DashAction::Down if p.selected + 1 < p.specs.len() => {
                    p.selected += 1;
                }
                DashAction::Quit => self.quit = true,
                // DrillIn (⏎) is a screen transition — handled by run_tui.
                _ => {}
            },
            Screen::Spec(s) => {
                let child_count = s.focused().children.len();
                match action {
                    DashAction::Up => s.selected = s.selected.saturating_sub(1),
                    DashAction::Down => {
                        if s.selected + 1 < child_count {
                            s.selected += 1;
                        }
                    }
                    DashAction::DrillIn => {
                        if s.selected < child_count {
                            s.drill.push(s.selected);
                            s.selected = 0;
                            s.expanded.clear();
                        }
                    }
                    DashAction::BackOut => {
                        // BackOut at the root is a screen transition (→Picker),
                        // handled by run_tui. Here we only pop within the tree.
                        if let Some(parent_idx) = s.drill.pop() {
                            s.selected = parent_idx;
                            s.expanded.clear();
                        }
                    }
                    DashAction::ToggleSort => self.sort = self.sort.toggled(),
                    DashAction::ToggleExpand => {
                        if !s.expanded.remove(&s.selected) {
                            s.expanded.insert(s.selected);
                        }
                    }
                    DashAction::Quit => self.quit = true,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::dashboard::model::build_tree;
    use crate::types::ids::SpecId;
    use chrono::{TimeZone, Utc};

    fn spec_id() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }

    fn three_phase_tree() -> DashNode {
        let mut spec = build_tree("S0000001", &[]);
        spec.children = vec![DashNode {
            kind: crate::cli::dashboard::model::NodeKind::Task,
            label: "T1".into(),
            status: "active".into(),
            started_at: Utc.timestamp_opt(0, 0).unwrap(),
            completed_at: None,
            split: Default::default(),
            phase_key: None,
            detail: String::new(),
            cost: Default::default(),
            task_ref: None,
            behavior: None,
            children: vec![],
        }];
        spec
    }

    #[test]
    fn drill_in_then_back_out_restores_selection() {
        let mut st = DashState::spec(spec_id(), three_phase_tree());
        st.apply(DashAction::DrillIn); // into T1
        if let Screen::Spec(ref s) = st.screen {
            assert_eq!(s.drill, vec![0]);
        } else {
            panic!("expected spec screen");
        }
        st.apply(DashAction::BackOut);
        if let Screen::Spec(ref s) = st.screen {
            assert_eq!(s.drill, Vec::<usize>::new());
            assert_eq!(s.selected, 0, "selection restored to the drilled node");
        } else {
            panic!("expected spec screen");
        }
    }

    #[test]
    fn down_is_clamped_to_child_count() {
        let mut st = DashState::spec(spec_id(), three_phase_tree());
        st.apply(DashAction::Down);
        st.apply(DashAction::Down);
        if let Screen::Spec(ref s) = st.screen {
            assert_eq!(s.selected, 0, "one child => selection cannot move");
        } else {
            panic!("expected spec screen");
        }
    }

    #[test]
    fn picker_down_is_clamped_to_spec_count() {
        use crate::cli::dashboard::picker::SpecSummary;
        let only = SpecSummary {
            spec_id: "S0000001a".into(),
            title: None,
            status: "running".into(),
            started_at: None,
            completed_at: None,
            phase_count: 0,
        };
        let mut st = DashState::picker(vec![only]);
        st.apply(DashAction::Down);
        if let Screen::Picker(p) = &st.screen {
            assert_eq!(p.selected, 0, "one row => selection cannot move");
        } else {
            panic!("expected picker screen");
        }
    }
}
