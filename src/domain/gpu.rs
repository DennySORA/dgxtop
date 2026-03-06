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
    pub clock_sm_mhz: f64,
    pub clock_video_mhz: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub memory_free_bytes: u64,
    #[serde(default)]
    pub memory_is_shared: bool,
    pub memory_bus_width_bits: Option<u32>,
    pub pcie_tx_bytes_per_sec: Option<u64>,
    pub pcie_rx_bytes_per_sec: Option<u64>,
    pub pcie_gen: Option<u32>,
    pub pcie_width: Option<u32>,
    pub pcie_max_gen: Option<u32>,
    pub pcie_max_width: Option<u32>,
    pub ecc_errors_corrected: Option<u64>,
    pub ecc_errors_uncorrected: Option<u64>,
    pub bar1_used_bytes: Option<u64>,
    pub bar1_total_bytes: Option<u64>,
    pub performance_state: Option<String>,
    pub throttle_reasons: Vec<String>,
    pub compute_mode: Option<String>,
    pub persistence_mode: Option<bool>,
    pub uuid: Option<String>,
    pub serial: Option<String>,
    pub encoder_utilization: Option<f64>,
    pub decoder_utilization: Option<f64>,
    pub retired_pages_sbe: Option<u64>,
    pub retired_pages_dbe: Option<u64>,
    pub temp_shutdown: Option<f64>,
    pub temp_slowdown: Option<f64>,
    pub total_energy_joules: Option<f64>,
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

    /// Theoretical peak memory bandwidth in GB/s.
    ///
    /// For discrete GPUs: `mem_clock_mhz * bus_width_bits * 2 (DDR) / 8 / 1000`.
    /// For unified-memory GPUs (GH200, GB10, etc.): uses known specs lookup
    /// since NVML does not report mem clock or bus width on these architectures.
    pub fn theoretical_mem_bandwidth_gbps(&self) -> Option<f64> {
        // Try standard formula first (discrete GPUs)
        if let Some(bus_width) = self.memory_bus_width_bits {
            let bw = bus_width as f64;
            if self.clock_memory_mhz > 0.0 && bw > 0.0 {
                return Some(self.clock_memory_mhz * bw * 2.0 / 8.0 / 1000.0);
            }
        }

        // Unified-memory fallback: lookup by GPU name
        if self.memory_is_shared {
            return self.unified_memory_bandwidth_gbps();
        }

        None
    }

    /// Estimated actual memory bandwidth in GB/s based on utilization.
    /// Returns None on unified-memory GPUs where NVML always reports 0%.
    pub fn actual_mem_bandwidth_gbps(&self) -> Option<f64> {
        let theoretical = self.theoretical_mem_bandwidth_gbps()?;
        // On unified memory, NVML reports utilization_memory = 0 always.
        // Return None so the UI knows the value is not meaningful.
        if self.memory_is_shared && self.utilization_memory <= 0.0 {
            return None;
        }
        Some(theoretical * self.utilization_memory / 100.0)
    }

    /// Known memory bandwidth for unified-memory GPU models.
    /// These GPUs use LPDDR5X shared with the CPU; NVML cannot report
    /// mem clock or bus width, so we use published specs.
    fn unified_memory_bandwidth_gbps(&self) -> Option<f64> {
        let lower = self.name.to_ascii_lowercase();
        // DGX Spark / Jetson Thor — GB10 (Blackwell) with LPDDR5X
        if lower.contains("gb10") {
            return Some(273.0); // 128-bit LPDDR5X-8533
        }
        // GH200 Grace Hopper — LPDDR5X
        if lower.contains("gh200") || lower.contains("grace hopper") {
            return Some(546.0); // 512 GB LPDDR5X
        }
        // GH100 in Grace Hopper Superchip
        if lower.contains("gh100") {
            return Some(546.0);
        }
        // Jetson AGX Orin — LPDDR5
        if lower.contains("orin") {
            return Some(204.8); // 256-bit LPDDR5
        }
        // Jetson AGX Xavier — LPDDR4x
        if lower.contains("xavier") && !lower.contains("nx") {
            return Some(136.5);
        }
        None
    }

    /// Descriptive string for the memory type.
    pub fn memory_type_label(&self) -> &'static str {
        if !self.memory_is_shared {
            return "GDDR/HBM";
        }
        let lower = self.name.to_ascii_lowercase();
        if lower.contains("gb10") || lower.contains("gh200") || lower.contains("gh100") {
            "LPDDR5X (Unified)"
        } else if lower.contains("orin") {
            "LPDDR5 (Unified)"
        } else if lower.contains("xavier") {
            "LPDDR4x (Unified)"
        } else {
            "Unified Memory"
        }
    }
}

/// Per-GPU history buffers for chart/sparkline rendering and long-term aggregation.
#[derive(Debug, Clone)]
pub struct GpuHistory {
    pub utilization: RingBuffer,
    pub temperature: RingBuffer,
    pub power: RingBuffer,
    pub memory_usage: RingBuffer,
    pub pcie_tx: RingBuffer,
    pub pcie_rx: RingBuffer,
    pub utilization_agg: TimeWindowAggregator,
    pub temperature_agg: TimeWindowAggregator,
    pub power_agg: TimeWindowAggregator,
    pub memory_agg: TimeWindowAggregator,
    pub pcie_tx_agg: TimeWindowAggregator,
    pub pcie_rx_agg: TimeWindowAggregator,
}

impl GpuHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            utilization: RingBuffer::new(capacity),
            temperature: RingBuffer::new(capacity),
            power: RingBuffer::new(capacity),
            memory_usage: RingBuffer::new(capacity),
            pcie_tx: RingBuffer::new(capacity),
            pcie_rx: RingBuffer::new(capacity),
            utilization_agg: TimeWindowAggregator::new(),
            temperature_agg: TimeWindowAggregator::new(),
            power_agg: TimeWindowAggregator::new(),
            memory_agg: TimeWindowAggregator::new(),
            pcie_tx_agg: TimeWindowAggregator::new(),
            pcie_rx_agg: TimeWindowAggregator::new(),
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

        if let Some(tx) = stats.pcie_tx_bytes_per_sec {
            let tx_f = tx as f64;
            self.pcie_tx.push(tx_f);
            self.pcie_tx_agg.push(tx_f);
        }
        if let Some(rx) = stats.pcie_rx_bytes_per_sec {
            let rx_f = rx as f64;
            self.pcie_rx.push(rx_f);
            self.pcie_rx_agg.push(rx_f);
        }
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
