use std::panic;

use clap::Parser;

use dgxtop::app::AppState;
use dgxtop::collectors::cpu::CpuCollector;
use dgxtop::collectors::disk::DiskCollector;
use dgxtop::collectors::gpu::{GpuCollector, gather_system_info};
use dgxtop::collectors::gpu_process::GpuProcessCollector;
use dgxtop::collectors::memory::MemoryCollector;
use dgxtop::collectors::network::NetworkCollector;
use dgxtop::config::cli::CliArgs;
use dgxtop::config::persistence::load_config;
use dgxtop::ui::run_ui;

fn main() {
    // Ensure terminal is restored on panic
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
        ratatui::restore();
        default_hook(info);
    }));

    let args = CliArgs::parse();

    // Load config and apply CLI overrides
    let mut config = load_config();
    config.apply_cli(&args);

    // Initialize collectors
    let cpu_collector = CpuCollector::new();
    let memory_collector = MemoryCollector::new();
    let disk_collector = DiskCollector::new();
    let network_collector = NetworkCollector::new();

    // GPU collector (optional — gracefully handle missing GPUs)
    let gpu_collector = if config.gpu_enabled {
        match GpuCollector::try_new() {
            Ok(gc) => Some(gc),
            Err(e) => {
                eprintln!("GPU monitoring disabled: {e}");
                None
            }
        }
    } else {
        None
    };

    let gpu_process_collector = gpu_collector.as_ref().and_then(|gc| {
        let nvml = match nvml_wrapper::Nvml::init() {
            Ok(n) => n,
            Err(e) => {
                eprintln!("GPU process monitoring disabled (NVML re-init failed): {e}");
                return None;
            }
        };
        match GpuProcessCollector::new(&nvml, gc.device_count()) {
            Ok(gpc) => Some(gpc),
            Err(e) => {
                eprintln!("GPU process monitoring disabled: {e}");
                None
            }
        }
    });

    // Gather static system info
    let system_info = gather_system_info(gpu_collector.as_ref());

    // Build app state
    let state = AppState::new(config, system_info);

    // Run the UI
    if let Err(e) = run_ui(
        state,
        cpu_collector,
        memory_collector,
        disk_collector,
        network_collector,
        gpu_collector,
        gpu_process_collector,
    ) {
        ratatui::restore();
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
