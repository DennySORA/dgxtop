//! Process History projection (D-76, D-77): the runs, newest start first,
//! and the run the session opened with a chart of each thing it used, as
//! bars of the session's size against the scales of the host they ran on.
//! The history's process actor publishes the runs, their figures and the
//! opened run's bars (D-79); this only shapes them into the view.

use std::collections::BTreeMap;

use dgxtop_core::commands::ViewKind;
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::process_history::{RunBucket, RunInfo, RunMetric};
use dgxtop_core::process_model::{ProcessHistoryModel, RunBars, RunDetail, RunRequest, RunRow};
use dgxtop_core::tiers::BarSize;
use dgxtop_core::view::{
    HistoryPointView, ProcessHistoryView, RunChartView, RunDetailView, RunRowView, SessionState,
};

use super::cpu::logical_cpu_count;
use super::gpu_directory::GpuDirectory;
use super::history_charts::{gpu_memory_scale, ram_total};
use super::plan::ViewportLayout;
use super::reader::MetricReader;

const PERCENT_SCALE: f64 = 100.0;

/// What the projection needs.
pub(crate) struct RunHistoryInputs<'a> {
    /// The runs and colors the process actor published.
    pub(crate) model: &'a ProcessHistoryModel,
    /// The run the session opened, at its bar size and plot width.
    pub(crate) opened: Option<&'a RunRequest>,
    pub(crate) utc_offset_s: i32,
    pub(crate) bar: BarSize,
    pub(crate) scales: &'a RunScales,
}

/// The run `session` shows on a `viewport` terminal: the one its Process
/// History opened, at its bar size, over as many bars as a plot has
/// columns (D-77).
pub(crate) fn run_request(session: &SessionState, viewport: (u16, u16)) -> Option<RunRequest> {
    if session.view != ViewKind::History {
        return None;
    }
    Some(RunRequest {
        key: session.selected_process.clone()?,
        bar: session.process_bar,
        columns: ViewportLayout::new(viewport).run_chart_bars(),
    })
}

/// The tops of the charts' scales, from the host now.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RunScales {
    /// 100 % for each logical CPU.
    pub(crate) cpu: Option<f64>,
    pub(crate) ram: Option<f64>,
    /// Each GPU's memory scale by its label (`GPU 0`).
    pub(crate) gpu_memory: BTreeMap<String, Option<f64>>,
}

impl RunScales {
    pub(crate) fn read(reader: &MetricReader<'_>, directory: &GpuDirectory<'_>) -> Self {
        let cpus = logical_cpu_count(reader);
        #[allow(clippy::cast_precision_loss)]
        let cpu = (cpus > 0).then_some(cpus as f64 * PERCENT_SCALE);
        Self {
            cpu,
            ram: ram_total(reader),
            gpu_memory: directory
                .gpus()
                .iter()
                .map(|gpu| (gpu.label.clone(), gpu_memory_scale(reader, gpu)))
                .collect(),
        }
    }

    /// The top of `metric`'s scale for `info`; `None` scales it to what it
    /// draws.
    fn of(&self, metric: RunMetric, info: &RunInfo) -> Option<f64> {
        match metric {
            RunMetric::Cpu | RunMetric::ChildrenCpu => self.cpu,
            RunMetric::Memory | RunMetric::ChildrenMemory | RunMetric::Swap => self.ram,
            #[allow(clippy::cast_precision_loss)]
            RunMetric::GpuUtilization => Some(PERCENT_SCALE * info.gpus.len().max(1) as f64),
            RunMetric::GpuMemory => {
                let scales: Option<Vec<f64>> = info
                    .gpus
                    .iter()
                    .map(|gpu| self.gpu_memory.get(gpu).copied().flatten())
                    .collect();
                scales
                    .filter(|scales| !scales.is_empty())
                    .map(|scales| scales.iter().sum())
            }
            RunMetric::DiskRead
            | RunMetric::DiskWrite
            | RunMetric::HostNetworkReceive
            | RunMetric::HostNetworkTransmit
            | RunMetric::Threads
            | RunMetric::Children
            | RunMetric::OpenFiles => None,
        }
    }
}

pub(crate) fn build_process_history_view(inputs: &RunHistoryInputs<'_>) -> ProcessHistoryView {
    let model = inputs.model;
    ProcessHistoryView {
        runs: model.runs.iter().map(|row| row_view(row, model)).collect(),
        more_runs: model.more_runs,
        opened: inputs
            .opened
            .and_then(|request| model.detail_for(request))
            .map(|detail| detail_view(detail, inputs)),
        kept_on_disk: model.kept_on_disk,
        utc_offset_s: inputs.utc_offset_s,
        bar: inputs.bar,
        cpu_scale: inputs.scales.cpu,
    }
}

fn row_view(row: &RunRow, model: &ProcessHistoryModel) -> RunRowView {
    let whole = &row.whole;
    RunRowView {
        key: row.key.clone(),
        name: row.name.clone(),
        started_utc_ns: row.started_utc_ns,
        runtime_ns: row.runtime_ns,
        live: row.live,
        end: row.end,
        gpus: row.gpus.clone(),
        peak_gpu_memory: whole.value(RunMetric::GpuMemory).peak(),
        mean_gpu_utilization: whole.value(RunMetric::GpuUtilization).mean(),
        mean_cpu: whole.value(RunMetric::Cpu).mean(),
        peak_memory: whole.value(RunMetric::Memory).peak(),
        peak_children: whole.value(RunMetric::Children).peak(),
        slot: model.slot_of(&row.key),
    }
}

fn detail_view(detail: &RunDetail, inputs: &RunHistoryInputs<'_>) -> RunDetailView {
    let info = &detail.info;
    let bars = detail.bars.as_ref();
    let window = bars.map_or((0, 0), |bars| bars.window);
    RunDetailView {
        info: info.clone(),
        live: detail.live,
        slot: inputs.model.slot_of(&info.key),
        bar: bars.map_or(inputs.bar, |bars| bars.bar),
        window_start_utc_ns: utc(window.0),
        window_end_utc_ns: utc(window.1),
        charts: RunMetric::ALL
            .iter()
            .map(|metric| {
                let points = bars.map_or_else(Vec::new, |bars| points_of(bars, *metric));
                RunChartView {
                    metric: *metric,
                    unit: unit_of(*metric),
                    scale_max: inputs.scales.of(*metric, info),
                    latest: points.last().and_then(|point| point.mean),
                    points,
                    mean: detail.whole.value(*metric).mean(),
                    peak: detail.whole.value(*metric).peak(),
                }
            })
            .collect(),
        loading: detail.loading,
    }
}

/// The bars with a sample of `metric`: each its mean and peak.
fn points_of(bars: &RunBars, metric: RunMetric) -> Vec<HistoryPointView> {
    bars.bars
        .iter()
        .filter(|bar| bar.value(metric).count > 0)
        .map(|bar: &RunBucket| HistoryPointView {
            start_utc_ns: utc(bar.start_utc_ns),
            end_utc_ns: utc(bar.start_utc_ns.saturating_add(bars.step_ns)),
            mean: bar.value(metric).mean(),
            min: None,
            max: bar.value(metric).peak(),
        })
        .collect()
}

/// A UTC time on the views' unsigned axis (none is before 1970).
fn utc(utc_ns: i64) -> u64 {
    u64::try_from(utc_ns).unwrap_or(0)
}

const fn unit_of(metric: RunMetric) -> CanonicalUnit {
    match metric {
        RunMetric::Cpu | RunMetric::GpuUtilization | RunMetric::ChildrenCpu => {
            CanonicalUnit::Percent
        }
        RunMetric::Memory | RunMetric::GpuMemory | RunMetric::ChildrenMemory | RunMetric::Swap => {
            CanonicalUnit::Bytes
        }
        RunMetric::DiskRead
        | RunMetric::DiskWrite
        | RunMetric::HostNetworkReceive
        | RunMetric::HostNetworkTransmit => CanonicalUnit::BytesPerSecond,
        RunMetric::Threads | RunMetric::Children | RunMetric::OpenFiles => CanonicalUnit::Count,
    }
}

#[cfg(test)]
mod tests;
