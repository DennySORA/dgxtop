use serde::{Deserialize, Serialize};

use super::history::{RingBuffer, TimeWindowAggregator};

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

/// Per-GPU history buffers for chart/sparkline rendering and long-term aggregation.
#[derive(Debug, Clone)]
pub struct GpuHistory {
    pub utilization: RingBuffer,
    pub temperature: RingBuffer,
    pub power: RingBuffer,
    pub memory_usage: RingBuffer,
    pub utilization_agg: TimeWindowAggregator,
    pub temperature_agg: TimeWindowAggregator,
    pub power_agg: TimeWindowAggregator,
    pub memory_agg: TimeWindowAggregator,
}

impl GpuHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            utilization: RingBuffer::new(capacity),
            temperature: RingBuffer::new(capacity),
            power: RingBuffer::new(capacity),
            memory_usage: RingBuffer::new(capacity),
            utilization_agg: TimeWindowAggregator::new(),
            temperature_agg: TimeWindowAggregator::new(),
            power_agg: TimeWindowAggregator::new(),
            memory_agg: TimeWindowAggregator::new(),
        }
    }

    pub fn record(&mut self, stats: &GpuStats) {
        let util = stats.utilization_gpu;
        let temp = stats.temperature;
        let power = stats.power_draw_watts;
        let mem = stats.memory_usage_percent();

        self.utilization.push(util);
        self.temperature.push(temp);
        self.power.push(power);
        self.memory_usage.push(mem);

        self.utilization_agg.push(util);
        self.temperature_agg.push(temp);
        self.power_agg.push(power);
        self.memory_agg.push(mem);
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
