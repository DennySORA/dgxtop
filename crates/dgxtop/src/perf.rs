//! Performance evidence primitives shared by the benchmark harness and the
//! validation report: nearest-rank quantiles, bounded raw latency series,
//! process-tree CPU accounting and the `performance-report.schema.json`
//! document (spec/dgxtop-v2/PERFORMANCE.md §2, §5).
//!
//! Nothing here fabricates results: a report is `measured` only when built
//! from recorded samples; budgets are read from `bench/budgets.json`.
//!
//! Harness building blocks shared by `benches/` (and unit-tested here):
//! [`scenarios`] and [`fixture`] (seeded REF workload, AT35), [`pipeline`]
//! (in-process replay → collectors), [`history_load`] (24 h history shape),
//! [`latency`], [`coverage`], [`verdict`], [`environment`], [`threads`],
//! [`evidence`] and [`report`] (honest report assembly).

pub mod coverage;
pub mod environment;
pub mod evidence;
pub mod fixture;
pub mod history_load;
pub mod latency;
pub mod pipeline;
pub mod report;
pub mod scenarios;
pub mod threads;
pub mod verdict;

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Nearest-rank quantile: sort ascending, take index `ceil(p * N) - 1`.
pub fn nearest_rank(sorted: &[f64], quantile: f64) -> Option<f64> {
    if sorted.is_empty() || !(0.0..=1.0).contains(&quantile) {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let rank = (quantile * sorted.len() as f64).ceil();
    // rank is within 1..=len because 0 <= quantile <= 1 and len >= 1.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let index = (rank as usize).clamp(1, sorted.len()) - 1;
    sorted.get(index).copied()
}

/// Bounded raw latency samples in microseconds (all samples kept up to the cap).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencySeries {
    samples_us: Vec<u64>,
    capacity: usize,
    dropped: u64,
}

impl LatencySeries {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            samples_us: Vec::with_capacity(capacity.min(1 << 20)),
            capacity,
            dropped: 0,
        }
    }

    pub fn record_us(&mut self, value_us: u64) {
        if self.samples_us.len() < self.capacity {
            self.samples_us.push(value_us);
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    pub fn len(&self) -> usize {
        self.samples_us.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples_us.is_empty()
    }

    /// Samples beyond the capacity (a harness must size capacity so this stays 0).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Nearest-rank quantile in milliseconds over all recorded samples.
    #[allow(clippy::cast_precision_loss)]
    pub fn quantile_ms(&self, quantile: f64) -> Option<f64> {
        let mut sorted: Vec<f64> = self
            .samples_us
            .iter()
            .map(|value| *value as f64 / 1000.0)
            .collect();
        sorted.sort_by(f64::total_cmp);
        nearest_rank(&sorted, quantile)
    }

    pub fn raw_us(&self) -> &[u64] {
        &self.samples_us
    }
}

/// CPU seconds of a process tree between two samples, as percent of ONE
/// logical core: `100 * Σ Δ(utime+stime) / (CLK_TCK * wall_seconds)`.
#[allow(clippy::cast_precision_loss)]
pub fn cpu_percent_of_one_core(
    delta_ticks: u64,
    clock_ticks_per_second: u64,
    wall_seconds: f64,
) -> Option<f64> {
    if clock_ticks_per_second == 0 || !(wall_seconds.is_finite() && wall_seconds > 0.0) {
        return None;
    }
    Some(100.0 * delta_ticks as f64 / (clock_ticks_per_second as f64 * wall_seconds))
}

/// `performance-report.schema.json` (kind `measured` only for real runs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformanceReport {
    pub schema_version: u32,
    pub kind: ReportKind,
    pub profile: String,
    pub source_commit: String,
    pub config_sha256: String,
    pub environment: Environment,
    pub evidence_files: Vec<EvidenceFile>,
    pub runs: Vec<RunResult>,
    pub soak_runs: Vec<SoakResult>,
    pub workload: Workload,
    pub config_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    Measured,
    SyntheticExample,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub cpu: String,
    pub architecture: String,
    pub os_kernel: String,
    pub logical_cpus: u32,
    pub ram_mib: u64,
    pub rustc: String,
    pub sqlite: String,
    pub gpu_driver: String,
    pub filesystem: String,
    pub terminal: String,
    pub background_load: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceFile {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleCounts {
    pub commands: u64,
    pub frames: u64,
    pub history_queries: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunMetrics {
    pub cpu_mean_one_core_percent: f64,
    pub rss_tree_peak_mib: f64,
    pub managed_peak_mib: f64,
    pub command_p95_ms: f64,
    pub command_p99_ms: f64,
    pub frame_p95_ms: f64,
    pub frame_p99_ms: f64,
    pub projection_p95_ms: f64,
    pub history_p95_ms: f64,
    pub coverage_percent: f64,
    pub unexplained_drops: f64,
    pub adaptive_throttle_events: f64,
    pub crashes: f64,
    pub deadlocks: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResult {
    pub duration_seconds: f64,
    pub sample_counts: SampleCounts,
    pub metrics: RunMetrics,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakResult {
    pub duration_seconds: f64,
    pub warmup_seconds: f64,
    pub rss_growth_after_warmup_mib: f64,
    pub fd_growth_after_warmup: f64,
    pub thread_growth_after_warmup: f64,
    pub crashes: f64,
    pub deadlocks: f64,
    pub unexplained_drops: f64,
    pub scenario: SoakScenario,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoakScenario {
    Fixed,
    Churn,
}

/// REF workload as fixed by the schema (constants, not tunables).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    pub series: u32,
    pub sources: u32,
    pub processes: u32,
    pub process_gpu_links: u32,
    pub adapter_hosts: u32,
    pub sampling_ms: u32,
    pub terminal_columns: u32,
    pub terminal_rows: u32,
    pub sessions: u32,
    pub archive_enabled: bool,
}

impl Workload {
    /// The REF profile of `bench/scenarios.json`.
    pub const fn reference() -> Self {
        Self {
            series: 512,
            sources: 2048,
            processes: 256,
            process_gpu_links: 1024,
            adapter_hosts: 7,
            sampling_ms: 1000,
            terminal_columns: 160,
            terminal_rows: 50,
            sessions: 1,
            archive_enabled: false,
        }
    }
}

/// One threshold of `bench/budgets.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bound {
    pub max: Option<f64>,
    pub min: Option<f64>,
    pub unit: String,
}

/// `bench/budgets.json` (machine-readable source of every threshold).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budgets {
    pub schema_version: u32,
    pub profile: String,
    pub limits: std::collections::BTreeMap<String, Bound>,
    pub minimum_duration_seconds: f64,
    pub minimum_samples: std::collections::BTreeMap<String, u64>,
    pub soak_limits: std::collections::BTreeMap<String, Bound>,
}

impl Budgets {
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    /// Gate failures of one REF run (empty = passes every REF threshold).
    pub fn evaluate_run(&self, run: &RunResult) -> Vec<String> {
        let mut failures = Vec::new();
        if run.duration_seconds < self.minimum_duration_seconds {
            failures.push(format!(
                "duration {}s below minimum {}s",
                run.duration_seconds, self.minimum_duration_seconds
            ));
        }
        let counts = [
            ("commands", run.sample_counts.commands),
            ("frames", run.sample_counts.frames),
            ("history_queries", run.sample_counts.history_queries),
        ];
        for (name, count) in counts {
            if let Some(minimum) = self.minimum_samples.get(name)
                && count < *minimum
            {
                failures.push(format!("{name}: {count} samples below minimum {minimum}"));
            }
        }
        let metrics = serde_json::to_value(&run.metrics).unwrap_or_default();
        for (name, bound) in &self.limits {
            let Some(value) = metrics.get(name).and_then(serde_json::Value::as_f64) else {
                failures.push(format!("{name}: missing metric"));
                continue;
            };
            if bound.max.is_some_and(|max| value > max) || bound.min.is_some_and(|min| value < min)
            {
                failures.push(format!("{name}={value} outside {bound:?}"));
            }
        }
        failures
    }
}

/// Lower-case hex SHA-256 of a file (evidence integrity).
pub fn file_sha256(path: &Path) -> Result<String, std::io::Error> {
    Ok(dgxtop_core::digest::sha256_hex(&std::fs::read(path)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_series_is_bounded_and_counts_drops() {
        let mut series = LatencySeries::with_capacity(3);
        for value in [1000, 2000, 3000, 4000] {
            series.record_us(value);
        }
        assert_eq!(series.len(), 3);
        assert_eq!(series.dropped(), 1);
        assert_eq!(series.quantile_ms(0.95), Some(3.0));
    }

    #[test]
    fn cpu_percent_uses_one_core_denominator() {
        // 15 ticks at 100 Hz over 30 s = 0.15 s CPU = 0.5 % of one core.
        assert_eq!(cpu_percent_of_one_core(15, 100, 30.0), Some(0.5));
        assert_eq!(cpu_percent_of_one_core(15, 0, 30.0), None);
        assert_eq!(cpu_percent_of_one_core(15, 100, 0.0), None);
    }

    fn passing_run() -> RunResult {
        RunResult {
            duration_seconds: 1800.0,
            sample_counts: SampleCounts {
                commands: 10_000,
                frames: 1000,
                history_queries: 1000,
            },
            metrics: RunMetrics {
                cpu_mean_one_core_percent: 1.0,
                rss_tree_peak_mib: 100.0,
                managed_peak_mib: 50.0,
                command_p95_ms: 1.0,
                command_p99_ms: 2.0,
                frame_p95_ms: 3.0,
                frame_p99_ms: 4.0,
                projection_p95_ms: 10.0,
                history_p95_ms: 5.0,
                coverage_percent: 100.0,
                unexplained_drops: 0.0,
                adaptive_throttle_events: 0.0,
                crashes: 0.0,
                deadlocks: 0.0,
            },
        }
    }

    #[test]
    fn budgets_gate_runs_from_the_spec_file() {
        let budgets = Budgets::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/dgxtop-v2/bench/budgets.json"),
        )
        .unwrap();
        assert!(budgets.evaluate_run(&passing_run()).is_empty());
        let mut slow = passing_run();
        slow.metrics.frame_p95_ms = 17.0;
        slow.sample_counts.frames = 10;
        slow.duration_seconds = 60.0;
        let failures = budgets.evaluate_run(&slow);
        assert_eq!(failures.len(), 3, "{failures:?}");
        let mut low_coverage = passing_run();
        low_coverage.metrics.coverage_percent = 99.0;
        assert_eq!(budgets.evaluate_run(&low_coverage).len(), 1);
    }
}
