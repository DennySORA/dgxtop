pub mod normal;
pub mod overlay;
pub mod process;

use crossterm::event::KeyEvent;

use crate::app::AppState;

/// Dispatch keyboard input to the appropriate handler based on current input mode.
pub fn handle_key_event(key: KeyEvent, state: &mut AppState) -> InputAction {
    match state.input_mode {
        InputMode::Normal => normal::handle(key, state),
        InputMode::ProcessSort => process::handle_sort(key, state),
        InputMode::ProcessFilter => process::handle_filter(key, state),
        InputMode::ProcessKill => process::handle_kill(key, state),
        InputMode::Help => overlay::handle_help(key, state),
        InputMode::Settings => overlay::handle_settings(key, state),
    }
}

/// What the UI loop should do after handling a key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    Continue,
    Quit,
    Redraw,
}

/// Current input mode determines which key handler is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    #[default]
    Normal,
    ProcessSort,
    ProcessFilter,
    ProcessKill,
    Help,
    Settings,
}
