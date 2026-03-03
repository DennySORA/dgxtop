use std::fs;

use crate::domain::memory::MemoryStats;
use crate::error::{DgxTopError, Result};

use super::Collector;

/// Collects memory statistics from /proc/meminfo.
pub struct MemoryCollector;

impl Default for MemoryCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryCollector {
    pub fn new() -> Self {
        Self
    }

    fn parse_kb_value(line: &str) -> Option<u64> {
        line.split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kb| kb * 1024)
    }
}

impl Collector for MemoryCollector {
    type Output = MemoryStats;

    fn collect(&mut self) -> Result<MemoryStats> {
        let content = fs::read_to_string("/proc/meminfo")
            .map_err(|e| DgxTopError::Collector(format!("failed to read /proc/meminfo: {e}")))?;

        let mut total = 0u64;
        let mut free = 0u64;
        let mut available = 0u64;
        let mut buffers = 0u64;
        let mut cached = 0u64;
        let mut swap_total = 0u64;
        let mut swap_free = 0u64;

        for line in content.lines() {
            if let Some(val) = Self::parse_kb_value(line) {
                if line.starts_with("MemTotal:") {
                    total = val;
                } else if line.starts_with("MemFree:") {
                    free = val;
                } else if line.starts_with("MemAvailable:") {
                    available = val;
                } else if line.starts_with("Buffers:") {
                    buffers = val;
                } else if line.starts_with("Cached:") {
                    cached = val;
                } else if line.starts_with("SwapTotal:") {
                    swap_total = val;
                } else if line.starts_with("SwapFree:") {
                    swap_free = val;
                }
            }
        }

        let used = total
            .saturating_sub(free)
            .saturating_sub(buffers)
            .saturating_sub(cached);

        Ok(MemoryStats {
            total_bytes: total,
            used_bytes: used,
            free_bytes: free,
            available_bytes: available,
            buffers_bytes: buffers,
            cached_bytes: cached,
            swap_total_bytes: swap_total,
            swap_used_bytes: swap_total.saturating_sub(swap_free),
            swap_free_bytes: swap_free,
        })
    }

    fn is_available(&self) -> bool {
        std::path::Path::new("/proc/meminfo").exists()
    }
}
