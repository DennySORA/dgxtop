use std::collections::HashMap;

use crate::config::AppConfig;
use crate::domain::cpu::{CpuHistory, CpuStats};
use crate::domain::disk::{DiskHistory, DiskStats};
use crate::domain::gpu::{GpuHistory, GpuProcessStats, GpuStats, NvLinkStats};
use crate::domain::memory::{MemoryHistory, MemoryStats};
use crate::domain::network::{NetworkHistory, NetworkInterfaceStats};
use crate::domain::system::SystemInfo;
use crate::ui::input::InputMode;
use crate::ui::views::ActiveTab;

/// Sort column for the process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessSortColumn {
    #[default]
    GpuMemory,
    GpuUtil,
    CpuPercent,
    HostMemory,
    Pid,
}

impl ProcessSortColumn {
    pub fn next(self) -> Self {
        match self {
            Self::GpuMemory => Self::GpuUtil,
            Self::GpuUtil => Self::CpuPercent,
            Self::CpuPercent => Self::HostMemory,
            Self::HostMemory => Self::Pid,
            Self::Pid => Self::GpuMemory,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::GpuMemory => "GPU MEM",
            Self::GpuUtil => "GPU %",
            Self::CpuPercent => "CPU %",
            Self::HostMemory => "HOST MEM",
            Self::Pid => "PID",
        }
    }
}

/// Central application state holding all data and UI state.
pub struct AppState {
    // --- Data ---
    pub system_info: SystemInfo,
    pub cpu: Option<CpuStats>,
    pub memory: Option<MemoryStats>,
    pub gpus: Vec<GpuStats>,
    pub gpu_processes: Vec<GpuProcessStats>,
    pub nvlink: Vec<NvLinkStats>,
    pub disks: Vec<DiskStats>,
    pub networks: Vec<NetworkInterfaceStats>,

    // --- History ---
    pub cpu_history: CpuHistory,
    pub memory_history: MemoryHistory,
    pub gpu_histories: Vec<GpuHistory>,
    pub disk_histories: HashMap<String, DiskHistory>,
    pub network_histories: HashMap<String, NetworkHistory>,

    // --- UI State ---
    pub active_tab: ActiveTab,
    pub input_mode: InputMode,
    pub process_sort: ProcessSortColumn,
    pub process_sort_ascending: bool,
    pub process_selected_index: usize,
    pub process_filter: String,
    pub selected_gpu_index: usize,
    pub process_kill_confirm: Option<u32>,
    pub show_per_core: bool,

    // --- Config ---
    pub config: AppConfig,
    pub tick_count: u64,
}

impl AppState {
    pub fn new(config: AppConfig, system_info: SystemInfo) -> Self {
        let history_len = config.history_length;
        let gpu_count = system_info.gpu_count as usize;

        Self {
            system_info,
            cpu: None,
            memory: None,
            gpus: Vec::new(),
            gpu_processes: Vec::new(),
            nvlink: Vec::new(),
            disks: Vec::new(),
            networks: Vec::new(),

            cpu_history: CpuHistory::new(history_len),
            memory_history: MemoryHistory::new(history_len),
            gpu_histories: (0..gpu_count)
                .map(|_| GpuHistory::new(history_len))
                .collect(),
            disk_histories: HashMap::new(),
            network_histories: HashMap::new(),

            active_tab: ActiveTab::default(),
            input_mode: InputMode::default(),
            process_sort: ProcessSortColumn::default(),
            process_sort_ascending: false,
            process_selected_index: 0,
            process_filter: String::new(),
            selected_gpu_index: 0,
            process_kill_confirm: None,
            show_per_core: false,

            config,
            tick_count: 0,
        }
    }

    /// Update CPU data and record history.
    pub fn update_cpu(&mut self, stats: CpuStats) {
        self.cpu_history.record(&stats);
        self.cpu = Some(stats);
    }

    /// Update memory data and record history.
    pub fn update_memory(&mut self, stats: MemoryStats) {
        self.memory_history.record(&stats);
        self.memory = Some(stats);
    }

    /// Update GPU data and record history.
    pub fn update_gpus(&mut self, gpus: Vec<GpuStats>) {
        // Ensure we have enough history buffers
        while self.gpu_histories.len() < gpus.len() {
            self.gpu_histories
                .push(GpuHistory::new(self.config.history_length));
        }

        for (i, gpu) in gpus.iter().enumerate() {
            if i < self.gpu_histories.len() {
                self.gpu_histories[i].record(gpu);
            }
        }
        self.gpus = gpus;
    }

    /// Update GPU process data.
    pub fn update_gpu_processes(&mut self, processes: Vec<GpuProcessStats>) {
        self.gpu_processes = processes;
        self.sort_processes();
    }

    /// Update disk data and record history.
    pub fn update_disks(&mut self, disks: Vec<DiskStats>) {
        for disk in &disks {
            self.disk_histories
                .entry(disk.device_name.clone())
                .or_insert_with(|| DiskHistory::new(self.config.history_length))
                .record(disk);
        }
        self.disks = disks;
    }

    /// Update network data and record history.
    pub fn update_networks(&mut self, nets: Vec<NetworkInterfaceStats>) {
        for net in &nets {
            self.network_histories
                .entry(net.name.clone())
                .or_insert_with(|| NetworkHistory::new(self.config.history_length))
                .record(net);
        }
        self.networks = nets;
    }

    /// Sort processes by current sort column.
    pub fn sort_processes(&mut self) {
        let ascending = self.process_sort_ascending;
        self.gpu_processes.sort_by(|a, b| {
            let ord = match self.process_sort {
                ProcessSortColumn::GpuMemory => a.gpu_memory_bytes.cmp(&b.gpu_memory_bytes),
                ProcessSortColumn::GpuUtil => a
                    .gpu_utilization
                    .partial_cmp(&b.gpu_utilization)
                    .unwrap_or(std::cmp::Ordering::Equal),
                ProcessSortColumn::CpuPercent => a
                    .cpu_percent
                    .partial_cmp(&b.cpu_percent)
                    .unwrap_or(std::cmp::Ordering::Equal),
                ProcessSortColumn::HostMemory => a.host_memory_bytes.cmp(&b.host_memory_bytes),
                ProcessSortColumn::Pid => a.pid.cmp(&b.pid),
            };
            if ascending { ord } else { ord.reverse() }
        });
    }

    /// Get filtered process list.
    pub fn filtered_processes(&self) -> Vec<&GpuProcessStats> {
        if self.process_filter.is_empty() {
            self.gpu_processes.iter().collect()
        } else {
            let filter = self.process_filter.to_lowercase();
            self.gpu_processes
                .iter()
                .filter(|p| {
                    p.command.to_lowercase().contains(&filter)
                        || p.user.to_lowercase().contains(&filter)
                        || p.pid.to_string().contains(&filter)
                })
                .collect()
        }
    }

    /// Increment tick counter.
    pub fn tick(&mut self) {
        self.tick_count += 1;
    }

    /// Adjust the update interval.
    pub fn adjust_interval(&mut self, delta: f64) {
        self.config.update_interval_secs =
            (self.config.update_interval_secs + delta).clamp(0.1, 10.0);
    }
}
