//! Process History projection (D-76, D-77): the runs, newest start first,
//! and the run the session opened with a chart of each thing it used, as
//! bars of the session's size (the compression's [`bars_of`], D-79)
//! against the scales of the host they ran on.

use std::collections::BTreeMap;

use dgxtop_compression::{Bars, bars_of, whole_series};
use dgxtop_core::history::HistoryMethod;
use dgxtop_core::identity::ProcessKey;
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::process_history::{RunBar, RunBucket, RunInfo, RunMetric};
use dgxtop_core::view::{
    HistoryPointView, MAX_RUN_CHART_POINTS, MAX_RUN_ROWS, ProcessHistoryView, RunChartView,
    RunDetailView, RunRowView,
};

use super::cpu::logical_cpu_count;
use super::gpu_directory::GpuDirectory;
use super::history_charts::{gpu_memory_scale, ram_total};
use super::reader::MetricReader;
use crate::process_history::{ProcessHistoryBook, Run};
use crate::process_memory::ProcessMemoryBook;

const PERCENT_SCALE: f64 = 100.0;

/// What the projection needs.
pub(crate) struct RunHistoryInputs<'a> {
    pub(crate) book: &'a ProcessHistoryBook,
    /// Colors of the processes that hold one now.
    pub(crate) colors: &'a ProcessMemoryBook,
    pub(crate) opened: Option<&'a ProcessKey>,
    pub(crate) kept_on_disk: bool,
    pub(crate) utc_offset_s: i32,
    pub(crate) bar: RunBar,
    /// Bars a chart's plot holds: its columns.
    pub(crate) columns: usize,
    pub(crate) scales: &'a RunScales,
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
    let mut runs: Vec<&Run> = inputs.book.runs().collect();
    runs.sort_by(|left, right| {
        right
            .info()
            .started_utc_ns
            .cmp(&left.info().started_utc_ns)
            .then_with(|| left.info().key.cmp(&right.info().key))
    });
    let more_runs = runs.len().saturating_sub(MAX_RUN_ROWS);
    let rows = runs
        .iter()
        .take(MAX_RUN_ROWS)
        .map(|run| row_view(run, inputs.colors))
        .collect();
    let opened = inputs
        .opened
        .and_then(|key| inputs.book.run(key))
        .map(|run| detail_view(run, inputs));
    ProcessHistoryView {
        runs: rows,
        more_runs,
        opened,
        kept_on_disk: inputs.kept_on_disk,
        utc_offset_s: inputs.utc_offset_s,
        bar: inputs.bar,
        cpu_scale: inputs.scales.cpu,
    }
}

fn row_view(run: &Run, colors: &ProcessMemoryBook) -> RunRowView {
    let info = run.info();
    let whole = whole_series(run.series());
    RunRowView {
        key: info.key.clone(),
        name: info.name.clone(),
        started_utc_ns: info.started_utc_ns,
        runtime_ns: info.runtime_ns(),
        live: run.is_live(),
        end: info.end,
        gpus: info.gpus.clone(),
        peak_gpu_memory: whole.value(RunMetric::GpuMemory).peak(),
        mean_gpu_utilization: whole.value(RunMetric::GpuUtilization).mean(),
        mean_cpu: whole.value(RunMetric::Cpu).mean(),
        peak_memory: whole.value(RunMetric::Memory).peak(),
        peak_children: whole.value(RunMetric::Children).peak(),
        slot: colors.slot_of(&info.key),
    }
}

fn detail_view(run: &Run, inputs: &RunHistoryInputs<'_>) -> RunDetailView {
    let info = run.info();
    let series = run.series();
    let columns = inputs.columns.clamp(1, MAX_RUN_CHART_POINTS);
    let bars = bars_of(series, inputs.bar, columns);
    let whole = whole_series(series);
    let window = bars.as_ref().map_or((0, 0), |bars| bars.window);
    RunDetailView {
        info: info.clone(),
        live: run.is_live(),
        slot: inputs.colors.slot_of(&info.key),
        bar: bars.as_ref().map_or(inputs.bar, |bars| bars.bar),
        window_start_utc_ns: utc(window.0),
        window_end_utc_ns: utc(window.1),
        charts: RunMetric::ALL
            .iter()
            .map(|metric| {
                let points = bars
                    .as_ref()
                    .map_or_else(Vec::new, |bars| points_of(bars, *metric));
                RunChartView {
                    metric: *metric,
                    unit: unit_of(*metric),
                    scale_max: inputs.scales.of(*metric, info),
                    latest: points.last().and_then(|point| point.mean),
                    points,
                    mean: whole.value(*metric).mean(),
                    peak: whole.value(*metric).peak(),
                }
            })
            .collect(),
        loading: series.is_empty() && !run.is_live(),
    }
}

/// The bars with a sample of `metric`: each its mean and peak.
fn points_of(bars: &Bars<RunBucket>, metric: RunMetric) -> Vec<HistoryPointView> {
    bars.bars
        .iter()
        .filter(|bar| bar.value(metric).count > 0)
        .map(|bar| HistoryPointView {
            start_utc_ns: utc(bar.start_utc_ns),
            end_utc_ns: utc(bar.start_utc_ns.saturating_add(bars.step_ns)),
            mean: bar.value(metric).mean(),
            min: None,
            max: bar.value(metric).peak(),
            method: HistoryMethod::BucketExact,
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
mod tests {
    use std::collections::BTreeMap;

    use dgxtop_core::canonical::ProcessRow;
    use dgxtop_core::identity::EntityId;
    use dgxtop_core::testing::boot_id;

    use super::*;
    use crate::process::Now;
    use crate::process_history::SampleInputs;
    use crate::test_support::{gpu_process, process_table, test_process_key};

    const SECOND: u64 = 1_000_000_000;
    const GPU: &str = "gpu:GPU-aaaa";
    const GIB: f64 = 1_073_741_824.0;

    fn row(pid: i64, start_ticks: u64, gpu_bytes: u64) -> ProcessRow {
        let mut row = gpu_process(pid, "acme_llm.train", &[(GPU, Some(gpu_bytes))]);
        row.key.start_time_ticks = start_ticks;
        row.cpu_percent = Some(50.0);
        row.rss_bytes = Some(3 << 30);
        row
    }

    fn observe(book: &mut ProcessHistoryBook, second: u64, rows: Vec<ProcessRow>) {
        let labels = BTreeMap::from([(EntityId::new(GPU).unwrap(), "GPU 0".to_owned())]);
        book.observe(
            &process_table(second, second * SECOND, rows),
            &SampleInputs {
                now: Now {
                    boot_ns: second * SECOND,
                    utc_ns: 1_789_999_800_000_000_000 + i128::from(second * SECOND),
                },
                clk_tck: Some(100),
                gpu_labels: &labels,
                host_network: [Some(10.0), None],
            },
        );
    }

    fn scales() -> RunScales {
        RunScales {
            cpu: Some(2_000.0),
            ram: Some(128.0 * GIB),
            gpu_memory: BTreeMap::from([("GPU 0".to_owned(), Some(120.0 * GIB))]),
        }
    }

    fn view_at(
        book: &ProcessHistoryBook,
        opened: Option<&ProcessKey>,
        (bar, columns): (RunBar, usize),
    ) -> ProcessHistoryView {
        build_process_history_view(&RunHistoryInputs {
            book,
            colors: &ProcessMemoryBook::new(1000),
            opened,
            kept_on_disk: true,
            utc_offset_s: 0,
            bar,
            columns,
            scales: &scales(),
        })
    }

    fn view(book: &ProcessHistoryBook, opened: Option<&ProcessKey>) -> ProcessHistoryView {
        view_at(book, opened, (RunBar::Auto, 80))
    }

    fn started_at_one_second() -> ProcessKey {
        let mut key = test_process_key(1);
        key.start_time_ticks = 100;
        key
    }

    /// The list puts the latest start first and sums up each run.
    #[test]
    fn runs_are_listed_newest_start_first_with_their_figures() {
        let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
        for second in 100..170 {
            observe(
                &mut book,
                second,
                vec![row(1, 100, 4 << 30), row(2, 900, 1 << 30)],
            );
        }
        let view = view(&book, None);
        let pids: Vec<i32> = view.runs.iter().map(|run| run.key.pid.get()).collect();
        assert_eq!(pids, vec![2, 1], "started at 9 s, then at 1 s");
        let first = &view.runs[1];
        assert!(first.live && first.end.is_none());
        assert_eq!(first.gpus, vec!["GPU 0".to_owned()]);
        assert_eq!(first.peak_gpu_memory, Some(4.0 * GIB));
        assert_eq!(first.peak_memory, Some(3.0 * GIB));
        assert_eq!(first.peak_children, None, "no probe, no descendants");
        assert_eq!(first.mean_cpu, Some(50.0));
        assert_eq!(view.more_runs, 0);
        assert!(view.opened.is_none());
        assert!(view.kept_on_disk);
        assert_eq!((view.bar, view.cpu_scale), (RunBar::Auto, Some(2_000.0)));
    }

    /// An opened run charts every metric as bars against the host's
    /// scales; `Auto` takes the finest bar that shows the run whole.
    #[test]
    fn an_opened_run_charts_each_metric_as_bars() {
        let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
        // Sampled from 2 s to 31 s.
        for second in 2..32 {
            observe(&mut book, second, vec![row(1, 100, 4 << 30)]);
        }
        let key = started_at_one_second();
        let opened = view(&book, Some(&key)).opened.unwrap();
        assert_eq!(opened.bar, RunBar::OneMinute);
        assert_eq!(
            opened.window_end_utc_ns - opened.window_start_utc_ns,
            80 * 60 * SECOND,
            "a bar for each of the 80 columns"
        );
        assert_eq!(opened.charts.len(), RunMetric::ALL.len());
        let cpu = &opened.charts[RunMetric::Cpu.index()];
        assert_eq!(cpu.unit, CanonicalUnit::Percent);
        assert_eq!(cpu.scale_max, Some(2_000.0), "every core");
        assert_eq!(cpu.points.len(), 1, "the minute it ran");
        assert_eq!(cpu.points[0].mean, Some(50.0));
        assert_eq!(
            cpu.points[0].end_utc_ns - cpu.points[0].start_utc_ns,
            60 * SECOND
        );
        assert_eq!(
            (cpu.latest, cpu.mean, cpu.peak),
            (Some(50.0), Some(50.0), Some(50.0))
        );
        let gpu = |metric: RunMetric| opened.charts[metric.index()].scale_max;
        assert_eq!(gpu(RunMetric::GpuUtilization), Some(100.0));
        assert_eq!(gpu(RunMetric::GpuMemory), Some(120.0 * GIB));
        assert_eq!(gpu(RunMetric::Memory), Some(128.0 * GIB));
        assert_eq!(gpu(RunMetric::DiskRead), None, "a rate scales to itself");
        let threads = &opened.charts[RunMetric::Threads.index()];
        assert_eq!(threads.unit, CanonicalUnit::Count);
        assert!(threads.points.is_empty(), "no probe, no threads");
        let network = &opened.charts[RunMetric::HostNetworkReceive.index()];
        assert_eq!(network.mean, Some(10.0));
        assert!(!opened.loading);
    }

    /// The session's bar size: a 5 s sample every second over 30 s is one
    /// 1 h bar, whose mean is the run's.
    #[test]
    fn the_session_bar_size_is_drawn() {
        let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
        for second in 2..32 {
            observe(&mut book, second, vec![row(1, 100, 4 << 30)]);
        }
        let key = started_at_one_second();
        let opened = view_at(&book, Some(&key), (RunBar::OneHour, 24))
            .opened
            .unwrap();
        assert_eq!(opened.bar, RunBar::OneHour);
        assert_eq!(
            opened.window_end_utc_ns - opened.window_start_utc_ns,
            24 * 3_600 * SECOND
        );
        let memory = &opened.charts[RunMetric::GpuMemory.index()];
        assert_eq!(memory.points.len(), 1);
        assert_eq!(memory.points[0].mean, Some(4.0 * GIB));
    }

    /// A run on GPUs the host does not report has no GPU memory scale.
    #[test]
    fn a_gpu_without_a_scale_leaves_the_chart_to_its_peak() {
        let mut info = book_with_one_run()
            .run(&started_at_one_second())
            .unwrap()
            .info()
            .clone();
        info.gpus = vec!["GPU 0".to_owned(), "GPU 7".to_owned()];
        assert_eq!(scales().of(RunMetric::GpuMemory, &info), None);
        assert_eq!(scales().of(RunMetric::GpuUtilization, &info), Some(200.0));
        info.gpus.clear();
        assert_eq!(scales().of(RunMetric::GpuMemory, &info), None);
        assert_eq!(scales().of(RunMetric::GpuUtilization, &info), Some(100.0));
    }

    fn book_with_one_run() -> ProcessHistoryBook {
        let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
        observe(&mut book, 2, vec![row(1, 100, 4 << 30)]);
        book
    }

    /// A run that started long before dgxtop saw it charts what dgxtop saw
    /// at the finest size, not one coarse bucket.
    #[test]
    fn a_run_older_than_dgxtop_charts_what_was_seen_finely() {
        let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
        // Started 1 s after boot; seen from an hour later.
        for second in 3_600..3_700 {
            observe(&mut book, second, vec![row(1, 100, 4 << 30)]);
        }
        let key = started_at_one_second();
        let opened = view(&book, Some(&key)).opened.unwrap();
        assert_eq!(opened.bar, RunBar::OneMinute);
        assert!(opened.charts[RunMetric::Cpu.index()].points.len() >= 2);
    }
}
