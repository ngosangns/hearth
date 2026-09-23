//! Port of `src/tui/tui-actions.ts`. Deliberate deviation from a 1:1 port: the TS source matches
//! raw terminal byte sequences (`matchesKey(data, ...)`) because pi-tui parses input at a lower
//! level than this port's terminal library; `crossterm` already parses input into structured
//! `KeyEvent`s, so this takes one directly rather than re-deriving that parsing.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::Service;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiAction {
    Quit,
    Up,
    Down,
    StartAll,
    StopAll,
    Start,
    Stop,
    Restart,
}

pub fn keyboard_action(key: KeyEvent, selected: Option<&Service>) -> Option<TuiAction> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(TuiAction::Quit);
    }
    match key.code {
        KeyCode::Char('q') => Some(TuiAction::Quit),
        KeyCode::Up | KeyCode::Char('k') => Some(TuiAction::Up),
        KeyCode::Down | KeyCode::Char('j') => Some(TuiAction::Down),
        KeyCode::Char('r') | KeyCode::Char('R') => Some(TuiAction::Restart),
        KeyCode::Char('x') => Some(TuiAction::Stop),
        KeyCode::Char('a') => Some(TuiAction::StartAll),
        KeyCode::Char('s') => Some(TuiAction::StopAll),
        KeyCode::Enter | KeyCode::Char(' ') => {
            let starting = matches!(selected.map(|s| s.state.as_str()), Some("stopped") | Some("queued-start"));
            Some(if starting { TuiAction::Start } else { TuiAction::Stop })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Service;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn service(state: &str) -> Service {
        Service { name: "metadata".to_string(), kind: None, state: state.to_string(), generation: None, current_operation_id: None }
    }

    #[test]
    fn r_and_shift_r_rebuild_and_restart_only_the_focused_service() {
        let selected = service("ready");
        assert_eq!(keyboard_action(key('r'), Some(&selected)), Some(TuiAction::Restart));
        assert_eq!(keyboard_action(key('R'), Some(&selected)), Some(TuiAction::Restart));
    }

    #[test]
    fn enter_and_space_start_a_queued_service_rather_than_stopping_it() {
        let selected = service("queued-start");
        assert_eq!(keyboard_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), Some(&selected)), Some(TuiAction::Start));
        assert_eq!(keyboard_action(key(' '), Some(&selected)), Some(TuiAction::Start));
    }

    #[test]
    fn x_stops_exactly_the_focused_service() {
        let selected = service("ready");
        assert_eq!(keyboard_action(key('x'), Some(&selected)), Some(TuiAction::Stop));
        assert_eq!(keyboard_action(key('x'), None), Some(TuiAction::Stop));
    }
}
