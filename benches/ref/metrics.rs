//! Turns one REF measurement window into the schema's run metrics, each
//! from the samples that define it.

use dgxtop::perf::coverage::CoverageReport;
use dgxtop::perf::cpu_percent_of_one_core;
use dgxtop::perf::latency::summarize_us;
use dgxtop::perf::report::RunRecord;
use serde::Serialize;

use super::probes::{HistoryWindow, ProjectionWindow};
use super::sampler::ResourceSample;
use crate::common::BenchResult;

const MIB: f64 = 1_048_576.0;

/// Everything measured in one window.
pub struct WindowData {
    pub wall_seconds: f64,
    pub resources: Vec<ResourceSample>,
    pub clock_ticks_per_second: u64,
    pub commands_us: Vec<u64>,
    pub frames_us: Vec<u64>,
    pub projection: ProjectionWindow,
    pub history: HistoryWindow,
    pub coverage: CoverageReport,
    /// Overload transitions the Manager logged in the window.
    pub overload_transitions: u64,
    /// Gaps and frames the Manager logged as dropped in the window.
    pub logged_drops: u64,
    pub completion_drops: u64,
    pub collector_panics: u64,
    pub host_restarts: u64,
    pub manager_died: bool,
    pub frontend_failed: bool,
    pub stalls: u64,
}

/// Process-tree CPU split into what the harness threads used and the rest.
#[derive(Debug, Clone, Serialize)]
pub struct CpuBreakdown {
    pub tree_percent: f64,
    pub harness_percent: f64,
    pub dgxtop_percent: f64,
    pub wall_seconds: f64,
    pub samples: usize,
}

/// CPU between the first and last resource sample, in percent of one core.
pub fn cpu_breakdown(
    resources: &[ResourceSample],
    clock_ticks_per_second: u64,
) -> Option<CpuBreakdown> {
    let (first, last) = (resources.first()?, resources.last()?);
    let wall_seconds = last.at_s - first.at_s;
    let tree_ticks = last.cpu_ticks.checked_sub(first.cpu_ticks)?;
    let harness_ticks = last.harness_ticks.saturating_sub(first.harness_ticks);
    let percent = |ticks| cpu_percent_of_one_core(ticks, clock_ticks_per_second, wall_seconds);
    Some(CpuBreakdown {
        tree_percent: percent(tree_ticks)?,
        harness_percent: percent(harness_ticks)?,
        dgxtop_percent: percent(tree_ticks.saturating_sub(harness_ticks))?,
        wall_seconds,
        samples: resources.len(),
    })
}

/// CPU and peak RSS of one process of the tree over the window.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessUsage {
    pub pid: i32,
    pub role: &'static str,
    pub cpu_percent: Option<f64>,
    pub peak_rss_mib: Option<f64>,
}

/// Per-process attribution (parent vs. each adapter host).
#[allow(clippy::cast_precision_loss)]
pub fn usage_by_process(
    resources: &[ResourceSample],
    clock_ticks_per_second: u64,
) -> Vec<ProcessUsage> {
    let (Some(first), Some(last)) = (resources.first(), resources.last()) else {
        return Vec::new();
    };
    let own = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    let wall = last.at_s - first.at_s;
    last.per_pid
        .iter()
        .map(|usage| {
            let start = first
                .per_pid
                .iter()
                .find(|earlier| earlier.pid == usage.pid);
            let delta = start
                .and_then(|start| start.cpu_ticks)
                .zip(usage.cpu_ticks)
                .and_then(|(start, end)| end.checked_sub(start));
            let peak = resources
                .iter()
                .flat_map(|sample| sample.per_pid.iter())
                .filter(|sample| sample.pid == usage.pid)
                .filter_map(|sample| sample.rss_bytes)
                .max();
            ProcessUsage {
                pid: usage.pid,
                role: if usage.pid == own {
                    "parent (manager, workers, frontend, harness)"
                } else {
                    "adapter host"
                },
                cpu_percent: delta
                    .and_then(|ticks| cpu_percent_of_one_core(ticks, clock_ticks_per_second, wall)),
                peak_rss_mib: peak.map(|bytes| bytes as f64 / MIB),
            }
        })
        .collect()
}

/// CPU of one parent thread over the window (threads grouped by name).
#[derive(Debug, Clone, Serialize)]
pub struct ThreadUsage {
    pub name: String,
    pub threads: usize,
    pub cpu_percent: f64,
}

/// Parent-process CPU by thread name, busiest first.
pub fn usage_by_thread(
    start: &[dgxtop::perf::threads::ThreadTicks],
    end: &[dgxtop::perf::threads::ThreadTicks],
    clock_ticks_per_second: u64,
    wall_seconds: f64,
) -> Vec<ThreadUsage> {
    let mut grouped: std::collections::BTreeMap<String, (usize, u64)> =
        std::collections::BTreeMap::new();
    for thread in end {
        let before = start
            .iter()
            .find(|earlier| earlier.tid == thread.tid)
            .map_or(0, |earlier| earlier.ticks);
        let entry = grouped.entry(thread.name.clone()).or_default();
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(thread.ticks.saturating_sub(before));
    }
    let mut usage: Vec<ThreadUsage> = grouped
        .into_iter()
        .filter_map(|(name, (threads, ticks))| {
            Some(ThreadUsage {
                name,
                threads,
                cpu_percent: cpu_percent_of_one_core(ticks, clock_ticks_per_second, wall_seconds)?,
            })
        })
        .collect();
    usage.sort_by(|left, right| right.cpu_percent.total_cmp(&left.cpu_percent));
    usage
}

/// Unexplained drops: the gaps and frames the product dropped for a full
/// queue or budget plus completions the composition could not deliver.
/// Deferred adapter dispatches are not drops, because the scheduler retries
/// them (D-37).
pub fn unexplained_drops(data: &WindowData) -> u64 {
    data.logged_drops.saturating_add(data.completion_drops)
}

#[allow(clippy::cast_precision_loss)]
fn as_f64(value: u64) -> f64 {
    value as f64
}

/// The schema run of a window; only metrics with samples are set.
pub fn run_record(data: &WindowData) -> BenchResult<RunRecord> {
    let mut record = RunRecord {
        duration_seconds: data.wall_seconds,
        commands: u64::try_from(data.commands_us.len()).unwrap_or(u64::MAX),
        frames: u64::try_from(data.frames_us.len()).unwrap_or(u64::MAX),
        history_queries: u64::try_from(data.history.latencies_us.len()).unwrap_or(u64::MAX),
        ..RunRecord::default()
    };
    let resource_samples = u64::try_from(data.resources.len()).unwrap_or(u64::MAX);
    if let Some(cpu) = cpu_breakdown(&data.resources, data.clock_ticks_per_second) {
        record.set(
            "cpu_mean_one_core_percent",
            cpu.dgxtop_percent,
            resource_samples,
        )?;
    }
    if let Some(rss) = data.resources.iter().map(|sample| sample.rss_bytes).max() {
        record.set("rss_tree_peak_mib", as_f64(rss) / MIB, resource_samples)?;
    }
    let managed = data
        .resources
        .iter()
        .map(|sample| {
            sample
                .managed_bytes
                .saturating_add(sample.shadow_history_bytes)
        })
        .max();
    if let Some(managed) = managed {
        record.set("managed_peak_mib", as_f64(managed) / MIB, resource_samples)?;
    }
    if let Some(commands) = summarize_us(&data.commands_us) {
        record.set("command_p95_ms", commands.p95_ms, commands.samples)?;
        record.set("command_p99_ms", commands.p99_ms, commands.samples)?;
    }
    if let Some(frames) = summarize_us(&data.frames_us) {
        record.set("frame_p95_ms", frames.p95_ms, frames.samples)?;
        record.set("frame_p99_ms", frames.p99_ms, frames.samples)?;
    }
    if let Some(projection) = summarize_us(&data.projection.latencies_us) {
        record.set("projection_p95_ms", projection.p95_ms, projection.samples)?;
    }
    if let Some(history) = summarize_us(&data.history.latencies_us) {
        record.set("history_p95_ms", history.p95_ms, history.samples)?;
    }
    if let Some(percent) = data.coverage.percent {
        record.set("coverage_percent", percent, data.coverage.expected_slots)?;
    }
    set_window_counts(&mut record, data)?;
    Ok(record)
}

/// Counts over the whole window (one observation each).
fn set_window_counts(record: &mut RunRecord, data: &WindowData) -> BenchResult<()> {
    let crashes = data
        .host_restarts
        .saturating_add(data.collector_panics)
        .saturating_add(u64::from(data.manager_died))
        .saturating_add(u64::from(data.frontend_failed));
    record.set("unexplained_drops", as_f64(unexplained_drops(data)), 1)?;
    record.set(
        "adaptive_throttle_events",
        as_f64(data.overload_transitions),
        1,
    )?;
    record.set("crashes", as_f64(crashes), 1)?;
    record.set("deadlocks", as_f64(data.stalls), 1)?;
    Ok(())
}
