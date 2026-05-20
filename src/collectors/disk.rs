use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::time::Instant;

use crate::domain::disk::{DiskRawCounters, DiskStats};
use crate::error::{DgxTopError, Result};

use super::Collector;

const SECTOR_SIZE: u64 = 512;

#[derive(Debug, Clone, Default)]
struct DiskCapacity {
    total_bytes: u64,
    used_bytes: u64,
    available_bytes: u64,
    mount_point: Option<String>,
}

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

    fn collect_capacities(
        devices: &HashMap<String, DiskRawCounters>,
    ) -> HashMap<String, DiskCapacity> {
        let mut capacities = HashMap::new();

        for name in devices.keys() {
            capacities.insert(
                name.clone(),
                DiskCapacity {
                    total_bytes: Self::read_block_total_bytes(name).unwrap_or(0),
                    ..DiskCapacity::default()
                },
            );
        }

        for (name, mounted_capacity) in Self::mounted_filesystem_capacities() {
            if capacities.contains_key(&name) {
                capacities.insert(name, mounted_capacity);
            }
        }

        capacities
    }

    fn read_block_total_bytes(device_name: &str) -> Option<u64> {
        let path = format!("/sys/class/block/{device_name}/size");
        let sectors = fs::read_to_string(path).ok()?.trim().parse::<u64>().ok()?;
        Some(sectors.saturating_mul(SECTOR_SIZE))
    }

    fn mounted_filesystem_capacities() -> HashMap<String, DiskCapacity> {
        let Ok(content) = fs::read_to_string("/proc/self/mountinfo") else {
            return HashMap::new();
        };

        let mut result = HashMap::new();

        for line in content.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let Some(separator) = fields.iter().position(|field| *field == "-") else {
                continue;
            };
            if fields.len() <= separator + 2 || fields.len() <= 4 {
                continue;
            }

            let Some(device_name) = Self::mount_source_device_name(fields[separator + 2]) else {
                continue;
            };
            if !Self::is_tracked_device(&device_name) {
                continue;
            }

            let mount_point = decode_mountinfo_path(fields[4]);
            let Some(capacity) = Self::statvfs_capacity(&mount_point) else {
                continue;
            };

            let replace_existing = result
                .get(&device_name)
                .and_then(|existing: &DiskCapacity| existing.mount_point.as_deref())
                .is_none_or(|existing_mount| mount_point.len() < existing_mount.len());

            if replace_existing {
                result.insert(device_name, capacity);
            }
        }

        result
    }

    fn mount_source_device_name(source: &str) -> Option<String> {
        if !source.starts_with("/dev/") {
            return None;
        }

        let source_path = Path::new(source);
        let resolved = fs::canonicalize(source_path).unwrap_or_else(|_| source_path.to_path_buf());
        resolved
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }

    fn statvfs_capacity(mount_point: &str) -> Option<DiskCapacity> {
        let path = CString::new(mount_point.as_bytes()).ok()?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        let rc = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
        if rc != 0 {
            return None;
        }

        let stat = unsafe { stat.assume_init() };
        let block_size = if stat.f_frsize > 0 {
            stat.f_frsize
        } else {
            stat.f_bsize
        };
        let total_bytes = stat.f_blocks.saturating_mul(block_size);
        let free_bytes = stat.f_bfree.saturating_mul(block_size);
        let available_bytes = stat.f_bavail.saturating_mul(block_size);

        Some(DiskCapacity {
            total_bytes,
            used_bytes: total_bytes.saturating_sub(free_bytes),
            available_bytes,
            mount_point: Some(mount_point.to_owned()),
        })
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
        let capacities = Self::collect_capacities(&current);
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
                    let capacity = capacities.get(name).cloned().unwrap_or_default();

                    stats.push(DiskStats {
                        device_name: name.clone(),
                        total_bytes: capacity.total_bytes,
                        used_bytes: capacity.used_bytes,
                        available_bytes: capacity.available_bytes,
                        mount_point: capacity.mount_point,
                        read_bytes_per_sec: read_sectors_delta as f64 * SECTOR_SIZE as f64
                            / elapsed,
                        write_bytes_per_sec: write_sectors_delta as f64 * SECTOR_SIZE as f64
                            / elapsed,
                        read_iops: read_ios_delta as f64 / elapsed,
                        write_iops: write_ios_delta as f64 / elapsed,
                        await_read_ms: await_read,
                        await_write_ms: await_write,
                        io_in_progress: curr.io_in_progress,
                    });
                }
            }
        }

        // Sort by total throughput descending; tie-break by name for stability.
        stats.sort_by(|a, b| {
            b.total_bytes_per_sec()
                .partial_cmp(&a.total_bytes_per_sec())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.device_name.cmp(&b.device_name))
        });

        self.prev_counters = current;
        self.prev_time = now;

        Ok(stats)
    }

    fn is_available(&self) -> bool {
        std::path::Path::new("/proc/diskstats").exists()
    }
}

fn decode_mountinfo_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
            && bytes[i + 1] < b'8'
            && bytes[i + 2] < b'8'
            && bytes[i + 3] < b'8'
        {
            let byte =
                (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0');
            decoded.push(byte);
            i += 4;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::decode_mountinfo_path;

    #[test]
    fn decodes_mountinfo_octal_escapes() {
        assert_eq!(decode_mountinfo_path("/mnt/data\\040set"), "/mnt/data set");
        assert_eq!(
            decode_mountinfo_path("/mnt/backslash\\134x"),
            "/mnt/backslash\\x"
        );
    }
}
