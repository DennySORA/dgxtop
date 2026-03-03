use std::collections::HashMap;
use std::fs;

use nvml_wrapper::Nvml;
use nvml_wrapper::enums::device::UsedGpuMemory;

use crate::domain::gpu::{GpuProcessStats, GpuProcessType};
use crate::error::Result;

use super::Collector;
use super::gpu::init_nvml;

/// Collects GPU process information via NVML + /proc filesystem.
pub struct GpuProcessCollector {
    nvml: Nvml,
    device_count: u32,
    /// Previous CPU time for delta-based CPU% calculation.
    prev_cpu_times: HashMap<u32, (u64, u64)>,
}

impl GpuProcessCollector {
    pub fn new(device_count: u32) -> Result<Self> {
        let nvml = init_nvml()?;
        Ok(Self {
            nvml,
            device_count,
            prev_cpu_times: HashMap::new(),
        })
    }

    /// Read username for a PID from /proc/PID/status.
    fn read_process_user(pid: u32) -> String {
        let path = format!("/proc/{pid}/status");
        fs::read_to_string(path)
            .ok()
            .and_then(|content| {
                content.lines().find_map(|line| {
                    if line.starts_with("Uid:") {
                        let uid: u32 = line.split_whitespace().nth(1)?.parse().ok()?;
                        // Convert UID to username via nix
                        nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
                            .ok()
                            .flatten()
                            .map(|u| u.name)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| "?".to_owned())
    }

    /// Read command line for a PID from /proc/PID/cmdline.
    fn read_process_command(pid: u32) -> String {
        let path = format!("/proc/{pid}/cmdline");
        fs::read_to_string(path)
            .ok()
            .map(|s| s.replace('\0', " ").trim().to_owned())
            .unwrap_or_else(|| {
                // Fallback to /proc/PID/comm
                let comm_path = format!("/proc/{pid}/comm");
                fs::read_to_string(comm_path)
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_else(|_| "?".to_owned())
            })
    }

    /// Read host memory (RSS) from /proc/PID/status in bytes.
    fn read_process_rss(pid: u32) -> u64 {
        let path = format!("/proc/{pid}/status");
        fs::read_to_string(path)
            .ok()
            .and_then(|content| {
                content.lines().find_map(|line| {
                    if line.starts_with("VmRSS:") {
                        line.split_whitespace()
                            .nth(1)
                            .and_then(|v| v.parse::<u64>().ok())
                            .map(|kb| kb * 1024)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(0)
    }

    /// Read CPU time from /proc/PID/stat and compute delta-based CPU%.
    fn read_process_cpu_percent(&mut self, pid: u32) -> f64 {
        let path = format!("/proc/{pid}/stat");
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return 0.0,
        };

        // Fields after the command name (which may contain spaces/parens)
        let after_comm = match content.rfind(')') {
            Some(pos) if pos + 2 < content.len() => &content[pos + 2..],
            _ => return 0.0,
        };

        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        if fields.len() < 13 {
            return 0.0;
        }

        // utime (field 14, index 11 after comm) and stime (field 15, index 12)
        let utime: u64 = fields[11].parse().unwrap_or(0);
        let stime: u64 = fields[12].parse().unwrap_or(0);
        let total_time = utime.saturating_add(stime);

        // Read system uptime in clock ticks.
        // sysconf returns -1 on error; fall back to the common Linux default of 100.
        let clock_ticks = {
            let raw = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            if raw <= 0 { 100 } else { raw as u64 }
        };
        let system_time_ticks = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .map(|s| (s * clock_ticks as f64) as u64)
            .unwrap_or(1);

        let cpu_percent = if let Some((prev_total, prev_system)) = self.prev_cpu_times.get(&pid) {
            let time_delta = system_time_ticks.saturating_sub(*prev_system);
            if time_delta > 0 {
                let process_delta = total_time.saturating_sub(*prev_total);
                (process_delta as f64 / time_delta as f64) * 100.0
            } else {
                0.0
            }
        } else {
            0.0
        };

        self.prev_cpu_times
            .insert(pid, (total_time, system_time_ticks));

        cpu_percent
    }
}

/// Intermediate data collected from NVML before computing CPU%.
struct NvmlRawData {
    /// (gpu_index, pid, gpu_memory_bytes, process_type)
    process_entries: Vec<(u32, u32, u64, GpuProcessType)>,
    /// (gpu_index, pid, sm_utilization)
    utilization_entries: Vec<(u32, u32, f64)>,
}

impl GpuProcessCollector {
    fn collect_nvml_data(&self) -> NvmlRawData {
        let mut process_entries = Vec::new();
        let mut utilization_entries = Vec::new();

        for gpu_idx in 0..self.device_count {
            let device = match self.nvml.device_by_index(gpu_idx) {
                Ok(d) => d,
                Err(_) => continue,
            };

            let mut seen_pids: std::collections::HashSet<u32> = std::collections::HashSet::new();

            if let Ok(compute_procs) = device.running_compute_processes() {
                for proc_info in compute_procs {
                    let gpu_mem = match proc_info.used_gpu_memory {
                        UsedGpuMemory::Used(bytes) => bytes,
                        UsedGpuMemory::Unavailable => 0,
                    };
                    process_entries.push((
                        gpu_idx,
                        proc_info.pid,
                        gpu_mem,
                        GpuProcessType::Compute,
                    ));
                    seen_pids.insert(proc_info.pid);
                }
            }

            if let Ok(graphics_procs) = device.running_graphics_processes() {
                for proc_info in graphics_procs {
                    if seen_pids.contains(&proc_info.pid) {
                        // Upgrade existing to ComputeAndGraphics
                        if let Some(entry) = process_entries
                            .iter_mut()
                            .find(|(gi, pid, _, _)| *gi == gpu_idx && *pid == proc_info.pid)
                        {
                            entry.3 = GpuProcessType::ComputeAndGraphics;
                        }
                        continue;
                    }
                    let gpu_mem = match proc_info.used_gpu_memory {
                        UsedGpuMemory::Used(bytes) => bytes,
                        UsedGpuMemory::Unavailable => 0,
                    };
                    process_entries.push((
                        gpu_idx,
                        proc_info.pid,
                        gpu_mem,
                        GpuProcessType::Graphics,
                    ));
                }
            }

            if let Ok(process_utils) = device.process_utilization_stats(None) {
                for pu in process_utils {
                    utilization_entries.push((gpu_idx, pu.pid, pu.sm_util as f64));
                }
            }
        }

        NvmlRawData {
            process_entries,
            utilization_entries,
        }
    }
}

impl Collector for GpuProcessCollector {
    type Output = Vec<GpuProcessStats>;

    fn collect(&mut self) -> Result<Vec<GpuProcessStats>> {
        // Phase 1: Gather raw process data from NVML (immutable borrow of self.nvml)
        let raw_data = self.collect_nvml_data();

        // Phase 2: Build process list with CPU% (mutable borrow of self.prev_cpu_times)
        let mut processes = Vec::new();
        for (gpu_idx, pid, gpu_mem, proc_type) in &raw_data.process_entries {
            let cpu_percent = self.read_process_cpu_percent(*pid);
            processes.push(GpuProcessStats {
                pid: *pid,
                user: Self::read_process_user(*pid),
                gpu_index: *gpu_idx,
                process_type: *proc_type,
                gpu_utilization: 0.0,
                gpu_memory_bytes: *gpu_mem,
                cpu_percent,
                host_memory_bytes: Self::read_process_rss(*pid),
                command: Self::read_process_command(*pid),
            });
        }

        // Phase 3: Enrich with GPU utilization data
        for (gpu_idx, pid, sm_util) in &raw_data.utilization_entries {
            if let Some(proc) = processes
                .iter_mut()
                .find(|p| p.pid == *pid && p.gpu_index == *gpu_idx)
            {
                proc.gpu_utilization = *sm_util;
            }
        }

        // Clean up stale PIDs from prev_cpu_times
        let active_pids: std::collections::HashSet<u32> = processes.iter().map(|p| p.pid).collect();
        self.prev_cpu_times
            .retain(|pid, _| active_pids.contains(pid));

        Ok(processes)
    }

    fn is_available(&self) -> bool {
        self.device_count > 0
    }
}
