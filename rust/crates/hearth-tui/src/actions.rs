//! Key bindings: a `crossterm` `KeyEvent` (plus the focused row) to a `TuiAction`.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hearth_core::state::ActualServiceState;

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
    /// "Kill whatever holds my port, then start" — only ever produced for an `externally-owned`
    /// row, and the runner still gates it behind a second keypress (the raw-mode TUI's version of
    /// the CLI's [y/N] prompt).
    Reclaim,
}

pub fn keyboard_action(key: KeyEvent, selected: Option<&Service>) -> Option<TuiAction> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(TuiAction::Quit);
    }
    let externally_owned = selected.map(|s| s.state) == Some(ActualServiceState::ExternallyOwned);
    match key.code {
        KeyCode::Char('q') => Some(TuiAction::Quit),
        KeyCode::Up | KeyCode::Char('k') => Some(TuiAction::Up),
        KeyCode::Down | KeyCode::Char('j') => Some(TuiAction::Down),
        KeyCode::Char('r') | KeyCode::Char('R') => Some(TuiAction::Restart),
        KeyCode::Char('x') => Some(TuiAction::Stop),
        KeyCode::Char('a') => Some(TuiAction::StartAll),
        KeyCode::Char('s') => Some(TuiAction::StopAll),
        KeyCode::Char('K') if externally_owned => Some(TuiAction::Reclaim),
        KeyCode::Enter | KeyCode::Char(' ') => {
            if externally_owned {
                // Stopping an externally-owned row fails loudly anyway — the only useful action
                // is the reclaim.
                return Some(TuiAction::Reclaim);
            }
            let starting = matches!(
                selected.map(|s| s.state),
                Some(
                    ActualServiceState::Stopped
                        | ActualServiceState::QueuedStart
                        | ActualServiceState::Succeeded
                )
            );
            Some(if starting {
                TuiAction::Start
            } else {
                TuiAction::Stop
            })
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
        let state = serde_json::from_value(serde_json::json!(state)).unwrap();
        Service {
            name: "metadata".to_string(),
            kind: None,
            state,
            generation: None,
            current_operation_id: None,
            error: None,
            readiness_detail: None,
        }
    }

    #[test]
    fn r_and_shift_r_rebuild_and_restart_only_the_focused_service() {
        let selected = service("ready");
        assert_eq!(
            keyboard_action(key('r'), Some(&selected)),
            Some(TuiAction::Restart)
        );
        assert_eq!(
            keyboard_action(key('R'), Some(&selected)),
            Some(TuiAction::Restart)
        );
    }

    #[test]
    fn enter_and_space_start_a_queued_service_rather_than_stopping_it() {
        let selected = service("queued-start");
        assert_eq!(
            keyboard_action(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Some(&selected)
            ),
            Some(TuiAction::Start)
        );
        assert_eq!(
            keyboard_action(key(' '), Some(&selected)),
            Some(TuiAction::Start)
        );
    }

    #[test]
    fn enter_starts_a_succeeded_command_again() {
        let selected = service("succeeded");
        assert_eq!(
            keyboard_action(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Some(&selected)
            ),
            Some(TuiAction::Start)
        );
        assert_eq!(
            keyboard_action(key(' '), Some(&selected)),
            Some(TuiAction::Start)
        );
    }

    #[test]
    fn x_stops_exactly_the_focused_service() {
        let selected = service("ready");
        assert_eq!(
            keyboard_action(key('x'), Some(&selected)),
            Some(TuiAction::Stop)
        );
        assert_eq!(keyboard_action(key('x'), None), Some(TuiAction::Stop));
    }

    /// An `externally-owned` row has no daemon-owned process to stop — Enter, Space and `K` all
    /// mean "reclaim my port" there, and only there.
    #[test]
    fn externally_owned_rows_map_their_actions_to_reclaim() {
        let selected = service("externally-owned");
        assert_eq!(
            keyboard_action(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Some(&selected)
            ),
            Some(TuiAction::Reclaim)
        );
        assert_eq!(
            keyboard_action(key(' '), Some(&selected)),
            Some(TuiAction::Reclaim)
        );
        assert_eq!(
            keyboard_action(key('K'), Some(&selected)),
            Some(TuiAction::Reclaim)
        );

        // Any other state: no reclaim — `K` is unmapped and Enter still toggles start/stop.
        let ready = service("ready");
        assert_eq!(keyboard_action(key('K'), Some(&ready)), None);
        assert_eq!(
            keyboard_action(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Some(&ready)
            ),
            Some(TuiAction::Stop)
        );
        let stopped = service("stopped");
        assert_eq!(
            keyboard_action(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Some(&stopped)
            ),
            Some(TuiAction::Start)
        );
    }
}
