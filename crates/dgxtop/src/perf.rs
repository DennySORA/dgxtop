//! Performance evidence primitives shared by the benchmark harness and the
//! validation report: nearest-rank quantiles, process-tree CPU accounting
//! and the `performance-report.schema.json` document
//! (spec/dgxtop-v2/PERFORMANCE.md §2, §5). Compiled only for the benches
//! (`bench-support`) and this crate's tests: no CLI path uses it.
//!
//! Nothing here fabricates results: a report is `measured` only when built
//! from recorded samples; budgets are read from `bench/budgets.json`.
//!
//! Harness building blocks shared by `benches/` (and unit-tested here):
//! [`scenarios`] and [`fixture`] (seeded REF workload, AT35), [`pipeline`]
//! (in-process replay → collectors), [`history_load`] (24 h history shape),
//! [`latency`], [`coverage`], [`verdict`], [`environment`], [`threads`],
//! [`tree_stats`], [`evidence`] and [`report`] (honest report assembly).

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
pub mod tree_stats;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_uses_one_core_denominator() {
        // 15 ticks at 100 Hz over 30 s = 0.15 s CPU = 0.5 % of one core.
        assert_eq!(cpu_percent_of_one_core(15, 100, 30.0), Some(0.5));
        assert_eq!(cpu_percent_of_one_core(15, 0, 30.0), None);
        assert_eq!(cpu_percent_of_one_core(15, 100, 0.0), None);
    }
}
