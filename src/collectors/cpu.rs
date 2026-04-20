use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::domain::cpu::{CoreStats, CpuStats, CpuTimeSample};
use crate::error::{DgxTopError, Result};

use super::Collector;

/// Collects CPU statistics from /proc/stat and /sys/devices/system/cpu.
pub struct CpuCollector {
    prev_total: Option<CpuTimeSample>,
    prev_cores: Vec<CpuTimeSample>,
    prev_power_counters: Vec<PowerEnergyCounter>,
    prev_time: Instant,
}

#[derive(Debug, Clone)]
struct PowerEnergyCounter {
    key: String,
    energy_uj: u64,
    max_energy_range_uj: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HwmonPowerMetricKind {
    Average,
    Input,
}

#[derive(Debug, Clone)]
struct HwmonPowerCandidate {
    index: u32,
    kind: HwmonPowerMetricKind,
    watts: f64,
    label: Option<String>,
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
            prev_power_counters: Vec::new(),
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

    fn read_trimmed(path: &Path) -> Option<String> {
        fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    fn read_u64(path: &Path) -> Option<u64> {
        Self::read_trimmed(path)?.parse::<u64>().ok()
    }

    fn powercap_domain_rank(name: &str) -> Option<u8> {
        let lower = name.trim().to_ascii_lowercase();
        if lower.starts_with("package-")
            || lower.contains("package")
            || lower.contains("socket")
            || lower.contains("pkg")
            || lower.starts_with("die-")
            || lower.contains("cpu")
        {
            Some(0)
        } else if lower == "psys" || lower.contains("platform") {
            Some(1)
        } else {
            None
        }
    }

    fn powercap_domains() -> Vec<PathBuf> {
        let mut ranked = Vec::new();

        let entries = match fs::read_dir("/sys/class/powercap") {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = Self::read_trimmed(&path.join("name")) else {
                continue;
            };
            let Some(rank) = Self::powercap_domain_rank(&name) else {
                continue;
            };
            ranked.push((rank, path));
        }

        ranked.sort_by(|(left_rank, left_path), (right_rank, right_path)| {
            left_rank
                .cmp(right_rank)
                .then_with(|| left_path.cmp(right_path))
        });

        let Some(best_rank) = ranked.first().map(|(rank, _)| *rank) else {
            return Vec::new();
        };

        ranked
            .into_iter()
            .filter(|(rank, _)| *rank == best_rank)
            .map(|(_, path)| path)
            .collect()
    }

    fn read_powercap_power_watts() -> Option<f64> {
        let mut total_uw = 0u64;
        let mut found = false;

        for domain in Self::powercap_domains() {
            if let Some(power_uw) = Self::read_u64(&domain.join("power_uw")) {
                total_uw = total_uw.saturating_add(power_uw);
                found = true;
            }
        }

        found.then_some(total_uw as f64 / 1_000_000.0)
    }

    fn read_powercap_energy_counters() -> Vec<PowerEnergyCounter> {
        let mut counters = Vec::new();

        for domain in Self::powercap_domains() {
            let Some(energy_uj) = Self::read_u64(&domain.join("energy_uj")) else {
                continue;
            };
            counters.push(PowerEnergyCounter {
                key: domain.to_string_lossy().into_owned(),
                energy_uj,
                max_energy_range_uj: Self::read_u64(&domain.join("max_energy_range_uj"))
                    .unwrap_or(0),
            });
        }

        counters.sort_by(|left, right| left.key.cmp(&right.key));
        counters
    }

    fn energy_delta_uj(
        prev_energy_uj: u64,
        current_energy_uj: u64,
        max_energy_range_uj: u64,
    ) -> u64 {
        if current_energy_uj >= prev_energy_uj {
            return current_energy_uj - prev_energy_uj;
        }

        if max_energy_range_uj > prev_energy_uj {
            max_energy_range_uj - prev_energy_uj + current_energy_uj
        } else {
            current_energy_uj
        }
    }

    fn estimate_powercap_energy_watts(&mut self, now: Instant) -> Option<f64> {
        let counters = Self::read_powercap_energy_counters();
        if counters.is_empty() {
            return None;
        }

        let prev_counters = std::mem::replace(&mut self.prev_power_counters, counters);
        if prev_counters.is_empty() {
            return None;
        }

        let elapsed_secs = now.duration_since(self.prev_time).as_secs_f64();
        if elapsed_secs <= f64::EPSILON {
            return None;
        }

        let mut total_delta_uj = 0u64;
        let mut matched = 0usize;

        for counter in &self.prev_power_counters {
            if let Some(prev) = prev_counters
                .iter()
                .find(|candidate| candidate.key == counter.key)
            {
                total_delta_uj = total_delta_uj.saturating_add(Self::energy_delta_uj(
                    prev.energy_uj,
                    counter.energy_uj,
                    counter.max_energy_range_uj.max(prev.max_energy_range_uj),
                ));
                matched += 1;
            }
        }

        if matched == 0 {
            return None;
        }

        Some(total_delta_uj as f64 / 1_000_000.0 / elapsed_secs)
    }

    fn parse_power_metric_filename(file_name: &str) -> Option<(u32, HwmonPowerMetricKind)> {
        let rest = file_name.strip_prefix("power")?;
        let (index, suffix) = rest.split_once('_')?;
        let index = index.parse::<u32>().ok()?;
        let kind = match suffix {
            "average" => HwmonPowerMetricKind::Average,
            "input" => HwmonPowerMetricKind::Input,
            _ => return None,
        };
        Some((index, kind))
    }

    fn is_cpu_power_label(label: &str) -> bool {
        let lower = label.trim().to_ascii_lowercase();
        lower.contains("cpu")
            || lower.contains("package")
            || lower.contains("pkg")
            || lower.contains("socket")
            || lower.contains("soc")
    }

    fn is_cpu_hwmon_name(name: &str) -> bool {
        let lower = name.trim().to_ascii_lowercase();
        lower.contains("coretemp")
            || lower.contains("k10temp")
            || lower.contains("zenpower")
            || lower.contains("fam15h_power")
            || lower.contains("cpu")
    }

    fn hwmon_label_matches(label: &str, expected: &str) -> bool {
        label.trim().eq_ignore_ascii_case(expected)
    }

    fn read_hwmon_candidates(path: &Path) -> Vec<HwmonPowerCandidate> {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };

        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            let Some((index, kind)) = Self::parse_power_metric_filename(&file_name) else {
                continue;
            };
            let Some(raw_uw) = Self::read_u64(&entry.path()) else {
                continue;
            };
            let label = Self::read_trimmed(&path.join(format!("power{index}_label")));
            candidates.push(HwmonPowerCandidate {
                index,
                kind,
                watts: raw_uw as f64 / 1_000_000.0,
                label,
            });
        }

        candidates.sort_by(|left, right| {
            left.index
                .cmp(&right.index)
                .then_with(|| left.kind.cmp(&right.kind))
        });
        candidates
    }

    fn select_spark_hwmon_power_watts(candidates: &[HwmonPowerCandidate]) -> Option<f64> {
        let cluster_sum: f64 = candidates
            .iter()
            .filter(|candidate| {
                candidate.label.as_deref().is_some_and(|label| {
                    Self::hwmon_label_matches(label, "cpu_p")
                        || Self::hwmon_label_matches(label, "cpu_e")
                })
            })
            .map(|candidate| candidate.watts)
            .sum();

        (cluster_sum > 0.0).then_some(cluster_sum)
    }

    fn select_hwmon_power_watts(name: &str, candidates: &[HwmonPowerCandidate]) -> Option<f64> {
        if let Some(watts) = Self::select_spark_hwmon_power_watts(candidates) {
            return Some(watts);
        }

        let labeled_sum: f64 = candidates
            .iter()
            .filter(|candidate| {
                candidate
                    .label
                    .as_deref()
                    .is_some_and(Self::is_cpu_power_label)
            })
            .map(|candidate| candidate.watts)
            .sum();

        if labeled_sum > 0.0 {
            return Some(labeled_sum);
        }

        if Self::is_cpu_hwmon_name(name) {
            return candidates.first().map(|candidate| candidate.watts);
        }

        None
    }

    fn read_hwmon_power_watts() -> Option<f64> {
        let entries = fs::read_dir("/sys/class/hwmon").ok()?;
        let mut total_watts = 0.0;
        let mut found = false;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let name = Self::read_trimmed(&path.join("name")).unwrap_or_default();
            let candidates = Self::read_hwmon_candidates(&path);
            let Some(watts) = Self::select_hwmon_power_watts(&name, &candidates) else {
                continue;
            };

            total_watts += watts;
            found = true;
        }

        found.then_some(total_watts)
    }

    fn read_cpu_power_watts(&mut self, now: Instant) -> Option<f64> {
        Self::read_powercap_power_watts()
            .or_else(|| self.estimate_powercap_energy_watts(now))
            .or_else(Self::read_hwmon_power_watts)
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
        let now = Instant::now();
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
        let power_watts = self.read_cpu_power_watts(now);

        let stats = CpuStats {
            usage_percent,
            user_percent,
            system_percent,
            iowait_percent,
            idle_percent,
            frequency_mhz: avg_freq,
            frequency_max_mhz: Self::read_max_frequency().unwrap_or(0.0),
            temperature_celsius: Self::read_cpu_temperature(),
            power_watts,
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
        self.prev_time = now;

        Ok(stats)
    }

    fn is_available(&self) -> bool {
        std::path::Path::new("/proc/stat").exists()
    }
}

#[cfg(test)]
mod tests {
    use super::{CpuCollector, HwmonPowerCandidate, HwmonPowerMetricKind};

    #[test]
    fn powercap_domain_rank_prefers_package_domains() {
        assert_eq!(CpuCollector::powercap_domain_rank("package-0"), Some(0));
        assert_eq!(CpuCollector::powercap_domain_rank("socket-1"), Some(0));
        assert_eq!(CpuCollector::powercap_domain_rank("psys"), Some(1));
        assert_eq!(CpuCollector::powercap_domain_rank("dram"), None);
    }

    #[test]
    fn energy_delta_handles_counter_wrap() {
        assert_eq!(CpuCollector::energy_delta_uj(100, 160, 200), 60);
        assert_eq!(CpuCollector::energy_delta_uj(180, 20, 200), 40);
    }

    #[test]
    fn parse_power_metric_filename_accepts_supported_hwmon_metrics() {
        assert_eq!(
            CpuCollector::parse_power_metric_filename("power1_average"),
            Some((1, HwmonPowerMetricKind::Average))
        );
        assert_eq!(
            CpuCollector::parse_power_metric_filename("power2_input"),
            Some((2, HwmonPowerMetricKind::Input))
        );
        assert_eq!(
            CpuCollector::parse_power_metric_filename("power1_label"),
            None
        );
    }

    #[test]
    fn spark_hwmon_prefers_cpu_clusters_only() {
        let watts = CpuCollector::select_hwmon_power_watts(
            "spbm-acpi-0",
            &[
                HwmonPowerCandidate {
                    index: 1,
                    kind: HwmonPowerMetricKind::Input,
                    watts: 17.0,
                    label: Some("soc_pkg".to_owned()),
                },
                HwmonPowerCandidate {
                    index: 2,
                    kind: HwmonPowerMetricKind::Input,
                    watts: 6.0,
                    label: Some("cpu_gpu".to_owned()),
                },
                HwmonPowerCandidate {
                    index: 3,
                    kind: HwmonPowerMetricKind::Input,
                    watts: 8.5,
                    label: Some("cpu_p".to_owned()),
                },
                HwmonPowerCandidate {
                    index: 4,
                    kind: HwmonPowerMetricKind::Input,
                    watts: 1.2,
                    label: Some("cpu_e".to_owned()),
                },
            ],
        );

        let watts = watts.expect("expected Spark CPU power");
        assert!((watts - 9.7).abs() < 1e-9);
    }
}
