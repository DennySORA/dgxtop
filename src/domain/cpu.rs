use serde::{Deserialize, Serialize};

use super::history::RingBuffer;

/// Aggregate CPU statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuStats {
    pub usage_percent: f64,
    pub user_percent: f64,
    pub system_percent: f64,
    pub iowait_percent: f64,
    pub idle_percent: f64,
    pub frequency_mhz: f64,
    pub frequency_max_mhz: f64,
    pub temperature_celsius: Option<f64>,
    pub core_count: usize,
    pub cores: Vec<CoreStats>,
}

/// Per-core CPU statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreStats {
    pub index: usize,
    pub usage_percent: f64,
    pub frequency_mhz: Option<f64>,
}

/// Raw CPU time counters from /proc/stat for delta calculation.
#[derive(Debug, Clone, Default)]
pub struct CpuTimeSample {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimeSample {
    pub fn total(&self) -> u64 {
        self.user
            .saturating_add(self.nice)
            .saturating_add(self.system)
            .saturating_add(self.idle)
            .saturating_add(self.iowait)
            .saturating_add(self.irq)
            .saturating_add(self.softirq)
            .saturating_add(self.steal)
    }

    pub fn active(&self) -> u64 {
        self.total()
            .saturating_sub(self.idle)
            .saturating_sub(self.iowait)
    }
}

/// CPU history buffers.
#[derive(Debug, Clone)]
pub struct CpuHistory {
    pub usage: RingBuffer,
    pub temperature: RingBuffer,
}

impl CpuHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            usage: RingBuffer::new(capacity),
            temperature: RingBuffer::new(capacity),
        }
    }

    pub fn record(&mut self, stats: &CpuStats) {
        self.usage.push(stats.usage_percent);
        if let Some(temp) = stats.temperature_celsius {
            self.temperature.push(temp);
        }
    }
}
