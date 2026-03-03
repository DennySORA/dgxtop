use crossterm::event::{KeyCode, KeyEvent};

use crate::app::AppState;

use super::{InputAction, InputMode};

/// Handle keyboard input in ProcessSort mode.
pub fn handle_sort(key: KeyEvent, state: &mut AppState) -> InputAction {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            state.input_mode = InputMode::Normal;
            InputAction::Redraw
        }
        // Cycle sort column
        KeyCode::Char('s') | KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            state.process_sort = state.process_sort.next();
            state.sort_processes();
            InputAction::Redraw
        }
        // Toggle ascending/descending
        KeyCode::Char('r') | KeyCode::Char('R') => {
            state.process_sort_ascending = !state.process_sort_ascending;
            state.sort_processes();
            InputAction::Redraw
        }
        KeyCode::Enter => {
            state.input_mode = InputMode::Normal;
            InputAction::Redraw
        }
        _ => InputAction::Continue,
    }
}

/// Handle keyboard input in ProcessFilter mode.
pub fn handle_filter(key: KeyEvent, state: &mut AppState) -> InputAction {
    match key.code {
        KeyCode::Esc => {
            state.process_filter.clear();
            state.input_mode = InputMode::Normal;
            InputAction::Redraw
        }
        KeyCode::Enter => {
            state.input_mode = InputMode::Normal;
            InputAction::Redraw
        }
        KeyCode::Backspace => {
            state.process_filter.pop();
            state.process_selected_index = 0;
            InputAction::Redraw
        }
        KeyCode::Char(c) => {
            state.process_filter.push(c);
            state.process_selected_index = 0;
            InputAction::Redraw
        }
        _ => InputAction::Continue,
    }
}

/// Handle keyboard input in ProcessKill mode.
pub fn handle_kill(key: KeyEvent, state: &mut AppState) -> InputAction {
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
            state.process_kill_confirm = None;
            state.input_mode = InputMode::Normal;
            InputAction::Redraw
        }
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            if let Some(pid) = state.process_kill_confirm.take() {
                kill_process(pid);
            }
            state.input_mode = InputMode::Normal;
            InputAction::Redraw
        }
        _ => InputAction::Continue,
    }
}

/// Send SIGTERM to a process. Only succeeds if the current user owns the process.
fn kill_process(pid: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
}
