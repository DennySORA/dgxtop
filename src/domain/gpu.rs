use serde::{Deserialize, Serialize};

use super::history::RingBuffer;

/// Statistics for a single GPU device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuStats {
    pub index: u32,
    pub name: String,
    pub utilization_gpu: f64,
    pub utilization_memory: f64,
    pub temperature: f64,
    pub power_draw_watts: f64,
    pub power_limit_watts: f64,
    pub fan_speed: Option<f64>,
    pub clock_graphics_mhz: f64,
    pub clock_max_graphics_mhz: f64,
    pub clock_memory_mhz: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub memory_free_bytes: u64,
    #[serde(default)]
    pub memory_is_shared: bool,
    pub pcie_tx_bytes_per_sec: Option<u64>,
    pub pcie_rx_bytes_per_sec: Option<u64>,
    pub ecc_errors_corrected: Option<u64>,
    pub ecc_errors_uncorrected: Option<u64>,
}

impl GpuStats {
    pub fn memory_usage_percent(&self) -> f64 {
        if self.memory_total_bytes == 0 {
            return 0.0;
        }
        (self.memory_used_bytes as f64 / self.memory_total_bytes as f64) * 100.0
    }

    pub fn power_usage_percent(&self) -> f64 {
        if self.power_limit_watts <= 0.0 {
            return 0.0;
        }
        (self.power_draw_watts / self.power_limit_watts) * 100.0
    }

    pub fn clock_usage_percent(&self) -> f64 {
        if self.clock_max_graphics_mhz <= 0.0 {
            return 0.0;
        }
        (self.clock_graphics_mhz / self.clock_max_graphics_mhz) * 100.0
    }
}

/// Per-GPU history buffers for chart/sparkline rendering.
#[derive(Debug, Clone)]
pub struct GpuHistory {
    pub utilization: RingBuffer,
    pub temperature: RingBuffer,
    pub power: RingBuffer,
    pub memory_usage: RingBuffer,
}

impl GpuHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            utilization: RingBuffer::new(capacity),
            temperature: RingBuffer::new(capacity),
            power: RingBuffer::new(capacity),
            memory_usage: RingBuffer::new(capacity),
        }
    }

    pub fn record(&mut self, stats: &GpuStats) {
        self.utilization.push(stats.utilization_gpu);
        self.temperature.push(stats.temperature);
        self.power.push(stats.power_draw_watts);
        self.memory_usage.push(stats.memory_usage_percent());
    }
}

/// A process running on a GPU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuProcessStats {
    pub pid: u32,
    pub user: String,
    pub gpu_index: u32,
    pub process_type: GpuProcessType,
    pub gpu_utilization: f64,
    pub gpu_memory_bytes: u64,
    pub cpu_percent: f64,
    pub host_memory_bytes: u64,
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuProcessType {
    Compute,
    Graphics,
    ComputeAndGraphics,
    Unknown,
}

impl std::fmt::Display for GpuProcessType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compute => write!(f, "Compute"),
            Self::Graphics => write!(f, "Graphics"),
            Self::ComputeAndGraphics => write!(f, "C+G"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

/// NVLink connection statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvLinkStats {
    pub gpu_index: u32,
    pub link_index: u32,
    pub is_active: bool,
    pub tx_bytes_per_sec: u64,
    pub rx_bytes_per_sec: u64,
    pub remote_gpu_index: Option<u32>,
}
