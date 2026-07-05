//! `crossterm` key events → [`DashAction`].

use crossterm::event::{KeyCode, KeyEvent};

use crate::cli::dashboard::state::DashAction;

/// Map a key event to an action, or `None` if the key is unbound.
pub fn map_key(key: KeyEvent) -> Option<DashAction> {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => Some(DashAction::Up),
        KeyCode::Down | KeyCode::Char('j') => Some(DashAction::Down),
        KeyCode::Enter => Some(DashAction::DrillIn),
        KeyCode::Esc => Some(DashAction::BackOut),
        KeyCode::Char('s') => Some(DashAction::ToggleSort),
        KeyCode::Char(' ') => Some(DashAction::ToggleExpand),
        KeyCode::Char('q') => Some(DashAction::Quit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn enter_drills_in_and_q_quits() {
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(map_key(enter), Some(DashAction::DrillIn));
        assert_eq!(map_key(q), Some(DashAction::Quit));
    }
}
