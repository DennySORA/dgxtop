use serde::{Deserialize, Serialize};

use super::history::{RingBuffer, TimeWindowAggregator};

/// I/O statistics for a single disk device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskStats {
    pub device_name: String,
    pub read_bytes_per_sec: f64,
    pub write_bytes_per_sec: f64,
    pub read_iops: f64,
    pub write_iops: f64,
    pub await_read_ms: f64,
    pub await_write_ms: f64,
    pub io_in_progress: u64,
}

impl DiskStats {
    pub fn total_bytes_per_sec(&self) -> f64 {
        self.read_bytes_per_sec + self.write_bytes_per_sec
    }
}

/// Raw counters from /proc/diskstats for delta calculation.
#[derive(Debug, Clone, Default)]
pub struct DiskRawCounters {
    pub device_name: String,
    pub reads_completed: u64,
    pub reads_merged: u64,
    pub sectors_read: u64,
    pub read_time_ms: u64,
    pub writes_completed: u64,
    pub writes_merged: u64,
    pub sectors_written: u64,
    pub write_time_ms: u64,
    pub io_in_progress: u64,
    pub io_time_ms: u64,
    pub weighted_io_time_ms: u64,
}

/// Per-device disk history buffers and long-term aggregation.
#[derive(Debug, Clone)]
pub struct DiskHistory {
    pub read_throughput: RingBuffer,
    pub write_throughput: RingBuffer,
    pub read_agg: TimeWindowAggregator,
    pub write_agg: TimeWindowAggregator,
}

impl DiskHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            read_throughput: RingBuffer::new(capacity),
            write_throughput: RingBuffer::new(capacity),
            read_agg: TimeWindowAggregator::new(),
            write_agg: TimeWindowAggregator::new(),
        }
    }

    pub fn record(&mut self, stats: &DiskStats) {
        let read = stats.read_bytes_per_sec;
        let write = stats.write_bytes_per_sec;

        self.read_throughput.push(read);
        self.write_throughput.push(write);

        self.read_agg.push(read);
        self.write_agg.push(write);
    }
}
