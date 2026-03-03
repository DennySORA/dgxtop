use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::AppState;
use crate::ui::views::ActiveTab;

use super::{InputAction, InputMode};

/// Handle keyboard input in Normal mode.
pub fn handle(key: KeyEvent, state: &mut AppState) -> InputAction {
    match key.code {
        // Quit
        KeyCode::Char('q') | KeyCode::Char('Q') => InputAction::Quit,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => InputAction::Quit,

        // Tab navigation
        KeyCode::Tab => {
            state.active_tab = state.active_tab.next();
            InputAction::Redraw
        }
        KeyCode::BackTab => {
            state.active_tab = state.active_tab.prev();
            InputAction::Redraw
        }
        KeyCode::Char('1') => {
            state.active_tab = ActiveTab::Overview;
            InputAction::Redraw
        }
        KeyCode::Char('2') => {
            state.active_tab = ActiveTab::GpuDetail;
            InputAction::Redraw
        }
        KeyCode::Char('3') => {
            state.active_tab = ActiveTab::Processes;
            InputAction::Redraw
        }

        // Vim navigation (up/down for process list or GPU selection)
        KeyCode::Char('j') | KeyCode::Down => {
            navigate_down(state);
            InputAction::Redraw
        }
        KeyCode::Char('k') | KeyCode::Up => {
            navigate_up(state);
            InputAction::Redraw
        }

        // GPU selection in detail view
        KeyCode::Char('h') | KeyCode::Left => {
            if state.active_tab == ActiveTab::GpuDetail && state.selected_gpu_index > 0 {
                state.selected_gpu_index -= 1;
            }
            InputAction::Redraw
        }
        KeyCode::Char('l') | KeyCode::Right => {
            if state.active_tab == ActiveTab::GpuDetail {
                let max = state.gpus.len().saturating_sub(1);
                if state.selected_gpu_index < max {
                    state.selected_gpu_index += 1;
                }
            }
            InputAction::Redraw
        }

        // Sort mode
        KeyCode::Char('s') | KeyCode::Char('S') => {
            state.input_mode = InputMode::ProcessSort;
            InputAction::Redraw
        }

        // Kill mode
        KeyCode::Char('K') => {
            if !state.gpu_processes.is_empty() {
                state.input_mode = InputMode::ProcessKill;
                let procs = state.filtered_processes();
                if let Some(proc) = procs.get(state.process_selected_index) {
                    state.process_kill_confirm = Some(proc.pid);
                }
            }
            InputAction::Redraw
        }

        // Filter mode
        KeyCode::Char('/') => {
            state.input_mode = InputMode::ProcessFilter;
            InputAction::Redraw
        }

        // Help overlay
        KeyCode::Char('?') | KeyCode::F(1) => {
            state.input_mode = InputMode::Help;
            InputAction::Redraw
        }

        // Speed controls
        KeyCode::Char('+') | KeyCode::Char('=') => {
            state.adjust_interval(-0.1);
            InputAction::Redraw
        }
        KeyCode::Char('-') => {
            state.adjust_interval(0.1);
            InputAction::Redraw
        }

        // Toggle per-core view
        KeyCode::Char('e') => {
            state.show_per_core = !state.show_per_core;
            InputAction::Redraw
        }

        _ => InputAction::Continue,
    }
}

fn navigate_down(state: &mut AppState) {
    match state.active_tab {
        ActiveTab::Processes | ActiveTab::Overview => {
            let max = state.filtered_processes().len().saturating_sub(1);
            if state.process_selected_index < max {
                state.process_selected_index += 1;
            }
        }
        ActiveTab::GpuDetail => {
            let max = state.gpus.len().saturating_sub(1);
            if state.selected_gpu_index < max {
                state.selected_gpu_index += 1;
            }
        }
    }
}

fn navigate_up(state: &mut AppState) {
    match state.active_tab {
        ActiveTab::Processes | ActiveTab::Overview => {
            if state.process_selected_index > 0 {
                state.process_selected_index -= 1;
            }
        }
        ActiveTab::GpuDetail => {
            if state.selected_gpu_index > 0 {
                state.selected_gpu_index -= 1;
            }
        }
    }
}
