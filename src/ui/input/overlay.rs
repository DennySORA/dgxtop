use crossterm::event::{KeyCode, KeyEvent};

use crate::app::AppState;

use super::{InputAction, InputMode};

/// Handle keyboard input in Help overlay.
pub fn handle_help(key: KeyEvent, state: &mut AppState) -> InputAction {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::F(1) => {
            state.input_mode = InputMode::Normal;
            InputAction::Redraw
        }
        _ => InputAction::Continue,
    }
}

/// Handle keyboard input in Settings overlay.
pub fn handle_settings(key: KeyEvent, state: &mut AppState) -> InputAction {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            state.input_mode = InputMode::Normal;
            InputAction::Redraw
        }
        _ => InputAction::Continue,
    }
}
