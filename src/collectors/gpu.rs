use std::ffi::OsStr;

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::enums::device::UsedGpuMemory;

use crate::domain::gpu::{GpuStats, NvLinkStats};
use crate::domain::system::SystemInfo;
use crate::error::{DgxTopError, Result};

use super::Collector;

/// Try to initialize NVML by probing multiple library paths.
/// This handles WSL, native Linux, and non-standard installations.
pub fn init_nvml() -> Result<Nvml> {
    // 1. Try standard init (dlopen("libnvidia-ml.so"))
    if let Ok(nvml) = Nvml::init() {
        return Ok(nvml);
    }

    // 2. Try versioned library name (WSL provides libnvidia-ml.so.1 only)
    if let Ok(nvml) = Nvml::builder()
        .lib_path(OsStr::new("libnvidia-ml.so.1"))
        .init()
    {
        return Ok(nvml);
    }

    // 3. Try well-known absolute paths
    let candidate_paths = [
        "/usr/lib/wsl/lib/libnvidia-ml.so.1",
        "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1",
        "/usr/lib64/libnvidia-ml.so.1",
        "/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1",
        "/usr/local/cuda/targets/x86_64-linux/lib/stubs/libnvidia-ml.so",
    ];

    for path in &candidate_paths {
        if std::path::Path::new(path).exists()
            && let Ok(nvml) = Nvml::builder().lib_path(OsStr::new(path)).init()
        {
            return Ok(nvml);
        }
    }

    Err(DgxTopError::Gpu(
        "NVML initialization failed: could not find libnvidia-ml.so. \
         Ensure NVIDIA drivers are installed."
            .to_owned(),
    ))
}

/// Collects GPU statistics via NVML (NVIDIA Management Library).
/// Far more reliable and performant than shelling out to nvidia-smi.
pub struct GpuCollector {
    nvml: Nvml,
    device_count: u32,
}

impl GpuCollector {
    pub fn try_new() -> Result<Self> {
        let nvml = init_nvml()?;
        let device_count = nvml
            .device_count()
            .map_err(|e| DgxTopError::Gpu(format!("failed to get GPU count: {e}")))?;

        Ok(Self { nvml, device_count })
    }

    pub fn device_count(&self) -> u32 {
        self.device_count
    }

    /// Gather static system-level GPU info (driver version, CUDA version).
    pub fn system_gpu_info(&self) -> (Option<String>, Option<String>) {
        let driver = self.nvml.sys_driver_version().ok();
        let cuda = self
            .nvml
            .sys_cuda_driver_version()
            .ok()
            .map(|v| format!("{}.{}", v / 1000, (v % 1000) / 10));
        (driver, cuda)
    }

    fn read_system_memory_total_bytes() -> Option<u64> {
        let content = std::fs::read_to_string("/proc/meminfo").ok()?;
        content.lines().find_map(|line| {
            if !line.starts_with("MemTotal:") {
                return None;
            }
            line.split_whitespace()
                .nth(1)
                .and_then(|kb| kb.parse::<u64>().ok())
                .map(|kb| kb * 1024)
        })
    }

    fn collect_process_used_gpu_memory_bytes(device: &nvml_wrapper::Device<'_>) -> u64 {
        let mut usage_by_pid: std::collections::HashMap<u32, u64> =
            std::collections::HashMap::new();

        if let Ok(compute_procs) = device.running_compute_processes() {
            for proc_info in compute_procs {
                let used_bytes = match proc_info.used_gpu_memory {
                    UsedGpuMemory::Used(bytes) => bytes,
                    UsedGpuMemory::Unavailable => 0,
                };
                usage_by_pid.insert(proc_info.pid, used_bytes);
            }
        }

        if let Ok(graphics_procs) = device.running_graphics_processes() {
            for proc_info in graphics_procs {
                let used_bytes = match proc_info.used_gpu_memory {
                    UsedGpuMemory::Used(bytes) => bytes,
                    UsedGpuMemory::Unavailable => 0,
                };
                usage_by_pid
                    .entry(proc_info.pid)
                    .and_modify(|existing| *existing = (*existing).max(used_bytes))
                    .or_insert(used_bytes);
            }
        }

        usage_by_pid.values().copied().sum()
    }

    fn collect_device(&self, index: u32) -> Result<GpuStats> {
        let device = self.nvml.device_by_index(index)?;

        let name = device.name().unwrap_or_else(|_| format!("GPU {index}"));

        let utilization = device.utilization_rates().ok().map(|u| (u.gpu, u.memory));
        let temperature = device
            .temperature(TemperatureSensor::Gpu)
            .ok()
            .map(|t| t as f64);

        let power_draw = device.power_usage().ok().map(|mw| mw as f64 / 1000.0);
        let power_limit = device
            .enforced_power_limit()
            .ok()
            .map(|mw| mw as f64 / 1000.0);

        let fan_speed = device.fan_speed(0).ok().map(|s| s as f64);

        let clock_graphics = device.clock_info(Clock::Graphics).ok().map(|c| c as f64);
        let clock_max = device
            .max_clock_info(Clock::Graphics)
            .ok()
            .map(|c| c as f64);
        let clock_mem = device.clock_info(Clock::Memory).ok().map(|c| c as f64);

        let memory_info = device.memory_info().ok();
        let (memory_used_bytes, memory_total_bytes, memory_free_bytes, memory_is_shared) =
            match memory_info {
                Some(mem_info) if mem_info.total > 0 => {
                    (mem_info.used, mem_info.total, mem_info.free, false)
                }
                _ => {
                    // Unified/shared-memory GPUs may report FB memory as N/A.
                    // Fallback to "sum(process used_gpu_memory) / system RAM total".
                    let total = Self::read_system_memory_total_bytes().unwrap_or(0);
                    let used = Self::collect_process_used_gpu_memory_bytes(&device);
                    let free = total.saturating_sub(used);
                    (used, total, free, true)
                }
            };

        let pcie_tx = device
            .pcie_throughput(nvml_wrapper::enum_wrappers::device::PcieUtilCounter::Send)
            .ok()
            .map(|kb| kb as u64 * 1024);
        let pcie_rx = device
            .pcie_throughput(nvml_wrapper::enum_wrappers::device::PcieUtilCounter::Receive)
            .ok()
            .map(|kb| kb as u64 * 1024);

        // ECC errors: total_ecc_errors(MemoryError, EccCounter) -> u64
        let ecc_corrected = device
            .total_ecc_errors(
                nvml_wrapper::enum_wrappers::device::MemoryError::Corrected,
                nvml_wrapper::enum_wrappers::device::EccCounter::Aggregate,
            )
            .ok();
        let ecc_uncorrected = device
            .total_ecc_errors(
                nvml_wrapper::enum_wrappers::device::MemoryError::Uncorrected,
                nvml_wrapper::enum_wrappers::device::EccCounter::Aggregate,
            )
            .ok();

        Ok(GpuStats {
            index,
            name,
            utilization_gpu: utilization.map(|(gpu, _)| gpu as f64).unwrap_or(0.0),
            utilization_memory: utilization.map(|(_, mem)| mem as f64).unwrap_or(0.0),
            temperature: temperature.unwrap_or(0.0),
            power_draw_watts: power_draw.unwrap_or(0.0),
            power_limit_watts: power_limit.unwrap_or(0.0),
            fan_speed,
            clock_graphics_mhz: clock_graphics.unwrap_or(0.0),
            clock_max_graphics_mhz: clock_max.unwrap_or(0.0),
            clock_memory_mhz: clock_mem.unwrap_or(0.0),
            memory_used_bytes,
            memory_total_bytes,
            memory_free_bytes,
            memory_is_shared,
            pcie_tx_bytes_per_sec: pcie_tx,
            pcie_rx_bytes_per_sec: pcie_rx,
            ecc_errors_corrected: ecc_corrected,
            ecc_errors_uncorrected: ecc_uncorrected,
        })
    }

    /// Collect NVLink statistics for all GPUs.
    /// Uses the NvLink wrapper API from nvml-wrapper.
    pub fn collect_nvlink(&self) -> Vec<NvLinkStats> {
        let mut links = Vec::new();

        for idx in 0..self.device_count {
            let device = match self.nvml.device_by_index(idx) {
                Ok(d) => d,
                Err(_) => continue,
            };

            // Probe up to 18 NVLink lanes (DGX H100 has up to 18)
            for link_idx in 0..18u32 {
                let nv_link = device.link_wrapper_for(link_idx);

                let is_active = match nv_link.is_active() {
                    Ok(active) => active,
                    Err(_) => continue,
                };

                if !is_active {
                    continue;
                }

                let remote_gpu = nv_link.remote_pci_info().ok().and_then(|pci| {
                    for other_idx in 0..self.device_count {
                        if other_idx == idx {
                            continue;
                        }
                        if let Ok(other_device) = self.nvml.device_by_index(other_idx)
                            && let Ok(other_pci) = other_device.pci_info()
                            && other_pci.bus_id == pci.bus_id
                        {
                            return Some(other_idx);
                        }
                    }
                    None
                });

                // NVLink utilization counters are not accessible in nvml-wrapper v0.10
                // (Counter enum is private). We report link presence without throughput.
                let tx = 0u64;
                let rx = 0u64;

                links.push(NvLinkStats {
                    gpu_index: idx,
                    link_index: link_idx,
                    is_active,
                    tx_bytes_per_sec: tx,
                    rx_bytes_per_sec: rx,
                    remote_gpu_index: remote_gpu,
                });
            }
        }

        links
    }
}

impl Collector for GpuCollector {
    type Output = Vec<GpuStats>;

    fn collect(&mut self) -> Result<Vec<GpuStats>> {
        let mut gpus = Vec::with_capacity(self.device_count as usize);
        for idx in 0..self.device_count {
            gpus.push(self.collect_device(idx)?);
        }
        Ok(gpus)
    }

    fn is_available(&self) -> bool {
        self.device_count > 0
    }
}

/// Gather static system info including GPU details.
pub fn gather_system_info(gpu_collector: Option<&GpuCollector>) -> SystemInfo {
    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "unknown".to_owned());

    let kernel_version = std::fs::read_to_string("/proc/version")
        .ok()
        .and_then(|s| s.split_whitespace().nth(2).map(|v| v.to_owned()))
        .unwrap_or_else(|| "unknown".to_owned());

    let os_name = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("PRETTY_NAME="))
                .map(|l| {
                    l.trim_start_matches("PRETTY_NAME=")
                        .trim_matches('"')
                        .to_owned()
                })
        })
        .unwrap_or_else(|| "Linux".to_owned());

    let architecture = std::env::consts::ARCH.to_owned();

    let uptime_seconds = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
        })
        .map(|s| s as u64)
        .unwrap_or(0);

    let (driver_version, cuda_version) = gpu_collector
        .map(|gc| gc.system_gpu_info())
        .unwrap_or((None, None));

    let gpu_count = gpu_collector.map(|gc| gc.device_count()).unwrap_or(0);

    SystemInfo {
        hostname,
        kernel_version,
        os_name,
        architecture,
        uptime_seconds,
        gpu_driver_version: driver_version,
        cuda_version,
        gpu_count,
    }
}
