use serde::{Deserialize, Serialize};

use super::history::RingBuffer;

/// Statistics for a single network interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterfaceStats {
    pub name: String,
    pub rx_bytes_per_sec: f64,
    pub tx_bytes_per_sec: f64,
    pub rx_packets_per_sec: f64,
    pub tx_packets_per_sec: f64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
    pub is_up: bool,
    pub speed_mbps: Option<u64>,
}

/// Raw counters from /sys/class/net for delta calculation.
#[derive(Debug, Clone, Default)]
pub struct NetworkRawCounters {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
}

/// Per-interface network history buffers.
#[derive(Debug, Clone)]
pub struct NetworkHistory {
    pub rx_throughput: RingBuffer,
    pub tx_throughput: RingBuffer,
}

impl NetworkHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            rx_throughput: RingBuffer::new(capacity),
            tx_throughput: RingBuffer::new(capacity),
        }
    }

    pub fn record(&mut self, stats: &NetworkInterfaceStats) {
        self.rx_throughput.push(stats.rx_bytes_per_sec);
        self.tx_throughput.push(stats.tx_bytes_per_sec);
    }
}
