use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use crate::domain::network::{NetworkInterfaceStats, NetworkRawCounters};
use crate::error::Result;

use super::Collector;

/// Collects network interface statistics from /sys/class/net.
pub struct NetworkCollector {
    prev_counters: HashMap<String, NetworkRawCounters>,
    prev_time: Instant,
}

impl Default for NetworkCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkCollector {
    pub fn new() -> Self {
        Self {
            prev_counters: HashMap::new(),
            prev_time: Instant::now(),
        }
    }

    fn is_tracked_interface(name: &str) -> bool {
        let excluded_prefixes = ["lo", "virbr", "docker", "br-", "veth"];
        !excluded_prefixes.iter().any(|p| name.starts_with(p))
    }

    fn read_sys_stat(iface: &str, stat_name: &str) -> u64 {
        let path = format!("/sys/class/net/{iface}/statistics/{stat_name}");
        fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    fn read_operstate(iface: &str) -> bool {
        let path = format!("/sys/class/net/{iface}/operstate");
        fs::read_to_string(path)
            .map(|s| s.trim() == "up")
            .unwrap_or(false)
    }

    fn read_speed(iface: &str) -> Option<u64> {
        let path = format!("/sys/class/net/{iface}/speed");
        fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .and_then(|v| if v > 0 { Some(v as u64) } else { None })
    }

    fn collect_raw_counters() -> HashMap<String, NetworkRawCounters> {
        let mut result = HashMap::new();

        let net_dir = Path::new("/sys/class/net");
        let entries = match fs::read_dir(net_dir) {
            Ok(e) => e,
            Err(_) => return result,
        };

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !Self::is_tracked_interface(&name) {
                continue;
            }

            result.insert(
                name.clone(),
                NetworkRawCounters {
                    name: name.clone(),
                    rx_bytes: Self::read_sys_stat(&name, "rx_bytes"),
                    tx_bytes: Self::read_sys_stat(&name, "tx_bytes"),
                    rx_packets: Self::read_sys_stat(&name, "rx_packets"),
                    tx_packets: Self::read_sys_stat(&name, "tx_packets"),
                    rx_errors: Self::read_sys_stat(&name, "rx_errors"),
                    tx_errors: Self::read_sys_stat(&name, "tx_errors"),
                    rx_dropped: Self::read_sys_stat(&name, "rx_dropped"),
                    tx_dropped: Self::read_sys_stat(&name, "tx_dropped"),
                },
            );
        }

        result
    }

    /// Sort interfaces: WiFi first, then Ethernet, then InfiniBand, then others.
    fn interface_sort_key(name: &str) -> (u8, String) {
        let priority = if name.starts_with("wl") {
            0
        } else if name.starts_with("en") || name.starts_with("eth") || name.starts_with("em") {
            1
        } else if name.starts_with("ib") {
            2
        } else {
            3
        };
        (priority, name.to_owned())
    }
}

impl Collector for NetworkCollector {
    type Output = Vec<NetworkInterfaceStats>;

    fn collect(&mut self) -> Result<Vec<NetworkInterfaceStats>> {
        let now = Instant::now();
        let current = Self::collect_raw_counters();
        let elapsed = now.duration_since(self.prev_time).as_secs_f64();

        let mut stats = Vec::new();

        for (name, curr) in &current {
            let (rx_bps, tx_bps, rx_pps, tx_pps) = if let Some(prev) = self.prev_counters.get(name)
            {
                if elapsed > 0.0 {
                    (
                        curr.rx_bytes.saturating_sub(prev.rx_bytes) as f64 / elapsed,
                        curr.tx_bytes.saturating_sub(prev.tx_bytes) as f64 / elapsed,
                        curr.rx_packets.saturating_sub(prev.rx_packets) as f64 / elapsed,
                        curr.tx_packets.saturating_sub(prev.tx_packets) as f64 / elapsed,
                    )
                } else {
                    (0.0, 0.0, 0.0, 0.0)
                }
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };

            stats.push(NetworkInterfaceStats {
                name: name.clone(),
                rx_bytes_per_sec: rx_bps,
                tx_bytes_per_sec: tx_bps,
                rx_packets_per_sec: rx_pps,
                tx_packets_per_sec: tx_pps,
                rx_errors: curr.rx_errors,
                tx_errors: curr.tx_errors,
                rx_dropped: curr.rx_dropped,
                tx_dropped: curr.tx_dropped,
                is_up: Self::read_operstate(name),
                speed_mbps: Self::read_speed(name),
            });
        }

        stats.sort_by(|a, b| {
            Self::interface_sort_key(&a.name).cmp(&Self::interface_sort_key(&b.name))
        });

        self.prev_counters = current;
        self.prev_time = now;

        Ok(stats)
    }

    fn is_available(&self) -> bool {
        Path::new("/sys/class/net").exists()
    }
}
