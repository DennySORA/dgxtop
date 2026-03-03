use std::collections::HashMap;
use std::fs;
use std::time::Instant;

use crate::domain::disk::{DiskRawCounters, DiskStats};
use crate::error::{DgxTopError, Result};

use super::Collector;

const SECTOR_SIZE: u64 = 512;

/// Collects disk I/O statistics from /proc/diskstats.
pub struct DiskCollector {
    prev_counters: HashMap<String, DiskRawCounters>,
    prev_time: Instant,
}

impl Default for DiskCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl DiskCollector {
    pub fn new() -> Self {
        Self {
            prev_counters: HashMap::new(),
            prev_time: Instant::now(),
        }
    }

    fn is_tracked_device(name: &str) -> bool {
        let prefixes = ["sd", "nvme", "vd", "hd", "xvd", "mmcblk"];
        let excluded = ["loop", "ram", "dm-", "sr", "fd"];

        if excluded.iter().any(|e| name.starts_with(e)) {
            return false;
        }
        prefixes.iter().any(|p| name.starts_with(p))
    }

    fn parse_diskstats() -> Result<HashMap<String, DiskRawCounters>> {
        let content = fs::read_to_string("/proc/diskstats")
            .map_err(|e| DgxTopError::Collector(format!("failed to read /proc/diskstats: {e}")))?;

        let mut result = HashMap::new();

        for line in content.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 14 {
                continue;
            }

            let device_name = fields[2].to_owned();
            if !Self::is_tracked_device(&device_name) {
                continue;
            }

            let parse_field =
                |idx: usize| -> u64 { fields.get(idx).and_then(|f| f.parse().ok()).unwrap_or(0) };

            result.insert(
                device_name.clone(),
                DiskRawCounters {
                    device_name,
                    reads_completed: parse_field(3),
                    reads_merged: parse_field(4),
                    sectors_read: parse_field(5),
                    read_time_ms: parse_field(6),
                    writes_completed: parse_field(7),
                    writes_merged: parse_field(8),
                    sectors_written: parse_field(9),
                    write_time_ms: parse_field(10),
                    io_in_progress: parse_field(11),
                    io_time_ms: parse_field(12),
                    weighted_io_time_ms: parse_field(13),
                },
            );
        }

        Ok(result)
    }
}

impl Collector for DiskCollector {
    type Output = Vec<DiskStats>;

    fn collect(&mut self) -> Result<Vec<DiskStats>> {
        let now = Instant::now();
        let current = Self::parse_diskstats()?;
        let elapsed = now.duration_since(self.prev_time).as_secs_f64();

        let mut stats = Vec::new();

        if elapsed > 0.0 {
            for (name, curr) in &current {
                if let Some(prev) = self.prev_counters.get(name) {
                    let read_sectors_delta = curr.sectors_read.saturating_sub(prev.sectors_read);
                    let write_sectors_delta =
                        curr.sectors_written.saturating_sub(prev.sectors_written);
                    let read_ios_delta = curr.reads_completed.saturating_sub(prev.reads_completed);
                    let write_ios_delta =
                        curr.writes_completed.saturating_sub(prev.writes_completed);
                    let read_time_delta = curr.read_time_ms.saturating_sub(prev.read_time_ms);
                    let write_time_delta = curr.write_time_ms.saturating_sub(prev.write_time_ms);

                    let await_read = if read_ios_delta > 0 {
                        read_time_delta as f64 / read_ios_delta as f64
                    } else {
                        0.0
                    };

                    let await_write = if write_ios_delta > 0 {
                        write_time_delta as f64 / write_ios_delta as f64
                    } else {
                        0.0
                    };

                    stats.push(DiskStats {
                        device_name: name.clone(),
                        read_bytes_per_sec: (read_sectors_delta * SECTOR_SIZE) as f64 / elapsed,
                        write_bytes_per_sec: (write_sectors_delta * SECTOR_SIZE) as f64 / elapsed,
                        read_iops: read_ios_delta as f64 / elapsed,
                        write_iops: write_ios_delta as f64 / elapsed,
                        await_read_ms: await_read,
                        await_write_ms: await_write,
                        io_in_progress: curr.io_in_progress,
                    });
                }
            }
        }

        stats.sort_by(|a, b| a.device_name.cmp(&b.device_name));

        self.prev_counters = current;
        self.prev_time = now;

        Ok(stats)
    }

    fn is_available(&self) -> bool {
        std::path::Path::new("/proc/diskstats").exists()
    }
}
