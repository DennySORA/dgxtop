use std::fs;
use std::time::Instant;

use crate::domain::cpu::{CoreStats, CpuStats, CpuTimeSample};
use crate::error::{DgxTopError, Result};

use super::Collector;

/// Collects CPU statistics from /proc/stat and /sys/devices/system/cpu.
pub struct CpuCollector {
    prev_total: Option<CpuTimeSample>,
    prev_cores: Vec<CpuTimeSample>,
    prev_time: Instant,
}

impl Default for CpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuCollector {
    pub fn new() -> Self {
        Self {
            prev_total: None,
            prev_cores: Vec::new(),
            prev_time: Instant::now(),
        }
    }

    fn parse_proc_stat() -> Result<(CpuTimeSample, Vec<CpuTimeSample>)> {
        let content = fs::read_to_string("/proc/stat")
            .map_err(|e| DgxTopError::Collector(format!("failed to read /proc/stat: {e}")))?;

        let mut total = CpuTimeSample::default();
        let mut cores = Vec::new();

        for line in content.lines() {
            if line.starts_with("cpu ") {
                total = Self::parse_cpu_line(line)?;
            } else if line.starts_with("cpu") {
                cores.push(Self::parse_cpu_line(line)?);
            }
        }

        Ok((total, cores))
    }

    fn parse_cpu_line(line: &str) -> Result<CpuTimeSample> {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            return Err(DgxTopError::Parse {
                field: "cpu".to_owned(),
                message: format!("expected at least 8 fields, got {}", fields.len()),
            });
        }

        let parse = |idx: usize, name: &str| -> Result<u64> {
            fields[idx].parse::<u64>().map_err(|_| DgxTopError::Parse {
                field: name.to_owned(),
                message: format!("invalid value: {}", fields[idx]),
            })
        };

        Ok(CpuTimeSample {
            user: parse(1, "user")?,
            nice: parse(2, "nice")?,
            system: parse(3, "system")?,
            idle: parse(4, "idle")?,
            iowait: parse(5, "iowait")?,
            irq: parse(6, "irq")?,
            softirq: parse(7, "softirq")?,
            steal: if fields.len() > 8 {
                parse(8, "steal")?
            } else {
                0
            },
        })
    }

    fn read_cpu_frequency(core_index: usize) -> Option<f64> {
        let path = format!("/sys/devices/system/cpu/cpu{core_index}/cpufreq/scaling_cur_freq");
        fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map(|khz| khz / 1000.0)
    }

    fn read_max_frequency() -> Option<f64> {
        // Try sysfs first (native Linux)
        let path = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq";
        if let Some(freq) = fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map(|khz| khz / 1000.0)
        {
            return Some(freq);
        }

        // Fallback: parse /proc/cpuinfo (works on WSL and containers)
        Self::read_frequency_from_cpuinfo()
    }

    /// Parse "cpu MHz" or "model name" from /proc/cpuinfo as frequency fallback.
    fn read_frequency_from_cpuinfo() -> Option<f64> {
        let content = fs::read_to_string("/proc/cpuinfo").ok()?;
        for line in content.lines() {
            if line.starts_with("cpu MHz") {
                return line.split(':').nth(1)?.trim().parse::<f64>().ok();
            }
        }
        None
    }

    fn read_cpu_temperature() -> Option<f64> {
        // Try common thermal zone paths
        for i in 0..20 {
            let type_path = format!("/sys/class/thermal/thermal_zone{i}/type");
            let temp_path = format!("/sys/class/thermal/thermal_zone{i}/temp");

            if let Ok(zone_type) = fs::read_to_string(&type_path) {
                let zone_type = zone_type.trim().to_lowercase();
                // Look for CPU-related thermal zones
                if (zone_type.contains("cpu")
                    || zone_type.contains("x86_pkg")
                    || zone_type.contains("coretemp")
                    || zone_type.contains("soc"))
                    && let Ok(temp_str) = fs::read_to_string(&temp_path)
                    && let Ok(millideg) = temp_str.trim().parse::<f64>()
                {
                    return Some(millideg / 1000.0);
                }
            }
        }

        // Fallback: use first thermal zone
        fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map(|millideg| millideg / 1000.0)
    }

    /// Parse /proc/loadavg: "0.50 0.60 0.70 3/500 12345"
    fn read_load_average() -> (f64, f64, f64, u32, u32) {
        let content = match fs::read_to_string("/proc/loadavg") {
            Ok(c) => c,
            Err(_) => return (0.0, 0.0, 0.0, 0, 0),
        };
        let fields: Vec<&str> = content.split_whitespace().collect();
        if fields.len() < 4 {
            return (0.0, 0.0, 0.0, 0, 0);
        }
        let avg1 = fields[0].parse::<f64>().unwrap_or(0.0);
        let avg5 = fields[1].parse::<f64>().unwrap_or(0.0);
        let avg15 = fields[2].parse::<f64>().unwrap_or(0.0);
        let (running, total) = if let Some((r, t)) = fields[3].split_once('/') {
            (r.parse::<u32>().unwrap_or(0), t.parse::<u32>().unwrap_or(0))
        } else {
            (0, 0)
        };
        (avg1, avg5, avg15, running, total)
    }

    fn calculate_usage(prev: &CpuTimeSample, curr: &CpuTimeSample) -> f64 {
        let total_delta = curr.total().saturating_sub(prev.total());
        if total_delta == 0 {
            return 0.0;
        }
        let active_delta = curr.active().saturating_sub(prev.active());
        (active_delta as f64 / total_delta as f64) * 100.0
    }

    fn calculate_component_percent(
        prev: &CpuTimeSample,
        curr: &CpuTimeSample,
        prev_val: u64,
        curr_val: u64,
    ) -> f64 {
        let total_delta = curr.total().saturating_sub(prev.total());
        if total_delta == 0 {
            return 0.0;
        }
        let delta = curr_val.saturating_sub(prev_val);
        (delta as f64 / total_delta as f64) * 100.0
    }
}

impl Collector for CpuCollector {
    type Output = CpuStats;

    fn collect(&mut self) -> Result<CpuStats> {
        let (total, cores_raw) = Self::parse_proc_stat()?;
        let core_count = cores_raw.len();

        let usage_percent;
        let user_percent;
        let system_percent;
        let iowait_percent;
        let idle_percent;

        if let Some(prev) = &self.prev_total {
            usage_percent = Self::calculate_usage(prev, &total);
            user_percent = Self::calculate_component_percent(
                prev,
                &total,
                prev.user + prev.nice,
                total.user + total.nice,
            );
            system_percent =
                Self::calculate_component_percent(prev, &total, prev.system, total.system);
            iowait_percent =
                Self::calculate_component_percent(prev, &total, prev.iowait, total.iowait);
            idle_percent = Self::calculate_component_percent(prev, &total, prev.idle, total.idle);
        } else {
            usage_percent = 0.0;
            user_percent = 0.0;
            system_percent = 0.0;
            iowait_percent = 0.0;
            idle_percent = 100.0;
        }

        let mut per_core_stats = Vec::with_capacity(core_count);
        for (i, core_raw) in cores_raw.iter().enumerate() {
            let core_usage = if i < self.prev_cores.len() {
                Self::calculate_usage(&self.prev_cores[i], core_raw)
            } else {
                0.0
            };
            per_core_stats.push(CoreStats {
                index: i,
                usage_percent: core_usage,
                frequency_mhz: Self::read_cpu_frequency(i),
            });
        }

        let avg_freq = {
            let known: Vec<f64> = per_core_stats
                .iter()
                .filter_map(|c| c.frequency_mhz)
                .collect();
            if known.is_empty() {
                // sysfs unavailable (e.g., WSL) — fall back to /proc/cpuinfo
                Self::read_frequency_from_cpuinfo().unwrap_or(0.0)
            } else {
                known.iter().sum::<f64>() / known.len() as f64
            }
        };

        let (load_avg_1m, load_avg_5m, load_avg_15m, tasks_running, tasks_total) =
            Self::read_load_average();

        let stats = CpuStats {
            usage_percent,
            user_percent,
            system_percent,
            iowait_percent,
            idle_percent,
            frequency_mhz: avg_freq,
            frequency_max_mhz: Self::read_max_frequency().unwrap_or(0.0),
            temperature_celsius: Self::read_cpu_temperature(),
            core_count,
            cores: per_core_stats,
            load_avg_1m,
            load_avg_5m,
            load_avg_15m,
            tasks_running,
            tasks_total,
        };

        self.prev_total = Some(total);
        self.prev_cores = cores_raw;
        self.prev_time = Instant::now();

        Ok(stats)
    }

    fn is_available(&self) -> bool {
        std::path::Path::new("/proc/stat").exists()
    }
}
