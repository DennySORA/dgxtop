use std::io;
use std::time::Duration;

use crossterm::event::KeyEventKind;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};

use crate::app::event::AppEvent;
use crate::app::state::AppState;
use crate::app::worker::spawn_event_loop;
use crate::collectors::Collector;
use crate::collectors::cpu::CpuCollector;
use crate::collectors::disk::DiskCollector;
use crate::collectors::memory::MemoryCollector;
use crate::collectors::network::NetworkCollector;
use crate::ui::input::{InputAction, InputMode, handle_key_event};
use crate::ui::theme::Theme;
use crate::ui::views::ActiveTab;

/// Run the main UI loop. Takes ownership of collectors and state.
pub fn run_ui(
    mut state: AppState,
    mut cpu_collector: CpuCollector,
    mut memory_collector: MemoryCollector,
    mut disk_collector: DiskCollector,
    mut network_collector: NetworkCollector,
    mut gpu_collector: Option<crate::collectors::gpu::GpuCollector>,
    mut gpu_process_collector: Option<crate::collectors::gpu_process::GpuProcessCollector>,
) -> crate::error::Result<()> {
    let mut terminal = ratatui::init();
    crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture)?;

    let tick_rate = Duration::from_secs_f64(state.config.update_interval_secs);
    let (_stop_tx, event_rx) = spawn_event_loop(tick_rate);

    // Initial collection
    collect_all(
        &mut state,
        &mut cpu_collector,
        &mut memory_collector,
        &mut disk_collector,
        &mut network_collector,
        &mut gpu_collector,
        &mut gpu_process_collector,
    );

    let theme = Theme::from_name(&state.config.color_theme);

    loop {
        // Render
        terminal.draw(|frame| render_frame(frame, &state, &theme))?;

        // Wait for event
        match event_rx.recv() {
            Ok(AppEvent::Tick) => {
                state.tick();
                collect_all(
                    &mut state,
                    &mut cpu_collector,
                    &mut memory_collector,
                    &mut disk_collector,
                    &mut network_collector,
                    &mut gpu_collector,
                    &mut gpu_process_collector,
                );
            }
            Ok(AppEvent::Key(key)) => {
                // Only handle key press events (not release/repeat)
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match handle_key_event(key, &mut state) {
                    InputAction::Quit => break,
                    InputAction::Redraw | InputAction::Continue => {}
                }
            }
            Ok(AppEvent::Mouse(_mouse)) => {
                // Mouse support placeholder for future iteration
            }
            Ok(AppEvent::Resize(_, _)) => {
                // Terminal will auto-resize on next draw
            }
            Err(_) => break,
        }
    }

    // Cleanup
    crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture)?;
    ratatui::restore();

    Ok(())
}

fn collect_all(
    state: &mut AppState,
    cpu: &mut CpuCollector,
    memory: &mut MemoryCollector,
    disk: &mut DiskCollector,
    network: &mut NetworkCollector,
    gpu: &mut Option<crate::collectors::gpu::GpuCollector>,
    gpu_process: &mut Option<crate::collectors::gpu_process::GpuProcessCollector>,
) {
    if let Ok(cpu_stats) = cpu.collect() {
        state.update_cpu(cpu_stats);
    }
    if let Ok(mem_stats) = memory.collect() {
        state.update_memory(mem_stats);
    }
    if let Ok(disk_stats) = disk.collect() {
        state.update_disks(disk_stats);
    }
    if let Ok(net_stats) = network.collect() {
        state.update_networks(net_stats);
    }
    if let Some(gc) = gpu.as_mut() {
        if let Ok(gpu_stats) = gc.collect() {
            state.update_gpus(gpu_stats);
        }
        state.nvlink = gc.collect_nvlink();
    }
    if let Some(gpc) = gpu_process.as_mut()
        && let Ok(procs) = gpc.collect()
    {
        state.update_gpu_processes(procs);
    }
}

fn render_frame(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let [header_area, body_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Header
    super::views::header::render(frame, header_area, state, theme);

    // Body — render active tab
    match state.active_tab {
        ActiveTab::Overview => super::views::overview::render(frame, body_area, state, theme),
        ActiveTab::GpuDetail => {
            super::views::gpu_detail::render(frame, body_area, state, theme);
        }
        ActiveTab::Processes => {
            super::views::processes::render(frame, body_area, state, theme);
        }
    }

    // Footer
    super::views::footer::render(frame, footer_area, state, theme);

    // Overlays (rendered on top)
    match state.input_mode {
        InputMode::Help => {
            super::views::overlays::help::render(frame, frame.area(), theme);
        }
        InputMode::Settings => {
            super::views::overlays::settings::render(frame, frame.area(), state, theme);
        }
        _ => {}
    }
}
