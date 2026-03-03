use serde::{Deserialize, Serialize};

use super::history::RingBuffer;

/// System memory statistics from /proc/meminfo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStats {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub buffers_bytes: u64,
    pub cached_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub swap_free_bytes: u64,
}

impl MemoryStats {
    pub fn usage_percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.used_bytes as f64 / self.total_bytes as f64) * 100.0
    }

    pub fn swap_usage_percent(&self) -> f64 {
        if self.swap_total_bytes == 0 {
            return 0.0;
        }
        (self.swap_used_bytes as f64 / self.swap_total_bytes as f64) * 100.0
    }
}

/// Memory history buffers.
#[derive(Debug, Clone)]
pub struct MemoryHistory {
    pub usage: RingBuffer,
    pub swap_usage: RingBuffer,
}

impl MemoryHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            usage: RingBuffer::new(capacity),
            swap_usage: RingBuffer::new(capacity),
        }
    }

    pub fn record(&mut self, stats: &MemoryStats) {
        self.usage.push(stats.usage_percent());
        self.swap_usage.push(stats.swap_usage_percent());
    }
}
