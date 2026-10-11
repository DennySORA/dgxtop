//! Process History projection (D-76): the runs, newest start first, and the
//! run the session opened with a chart of each thing it used.
//!
//! An opened run's charts take the finest bucket size that covers the whole
//! run (5 s, minutes, ten minutes), on an even grid from its first bucket;
//! a grid longer than [`MAX_RUN_CHART_POINTS`] merges neighbouring buckets.

use dgxtop_core::identity::ProcessKey;
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::process_history::{Aggregate, RunBucket, RunMetric, Tier};
use dgxtop_core::view::{
    MAX_RUN_CHART_POINTS, MAX_RUN_ROWS, ProcessHistoryView, RunChartView, RunDetailView, RunRowView,
};

use crate::process_history::series::RunSeries;
use crate::process_history::{ProcessHistoryBook, Run};
use crate::process_memory::ProcessMemoryBook;

/// What the projection needs.
pub(crate) struct RunHistoryInputs<'a> {
    pub(crate) book: &'a ProcessHistoryBook,
    /// Colors of the processes that hold one now.
    pub(crate) colors: &'a ProcessMemoryBook,
    pub(crate) opened: Option<&'a ProcessKey>,
    pub(crate) kept_on_disk: bool,
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
        .map(|run| detail_view(run, inputs.colors));
    ProcessHistoryView {
        runs: rows,
        more_runs,
        opened,
        kept_on_disk: inputs.kept_on_disk,
    }
}

fn row_view(run: &Run, colors: &ProcessMemoryBook) -> RunRowView {
    let info = run.info();
    let whole = whole_run(run.series());
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
        slot: colors.slot_of(&info.key),
    }
}

fn detail_view(run: &Run, colors: &ProcessMemoryBook) -> RunDetailView {
    let info = run.info();
    let series = run.series();
    let tier = chart_tier(series, info.started_utc_ns);
    let buckets: Vec<&RunBucket> =
        tier.map_or_else(Vec::new, |tier| series.buckets(tier).collect());
    let step_ns = tier.map_or(0, Tier::step_ns);
    let grid = Grid::of(&buckets, step_ns);
    let whole = whole_run(series);
    RunDetailView {
        info: info.clone(),
        live: run.is_live(),
        slot: colors.slot_of(&info.key),
        first_utc_ns: grid.first_utc_ns,
        step_ns: grid.step_ns,
        charts: RunMetric::ALL
            .iter()
            .map(|metric| RunChartView {
                metric: *metric,
                unit: unit_of(*metric),
                means: grid.means(&buckets, *metric),
                mean: whole.value(*metric).mean(),
                peak: whole.value(*metric).peak(),
            })
            .collect(),
        loading: series.is_empty() && !run.is_live(),
    }
}

/// The run as one bucket: every bucket of the coarsest size held, and the
/// finer buckets being filled, which have not reached it yet (each size
/// merges the finer ones as they close, so every sample counts once).
fn whole_run(series: &RunSeries) -> RunBucket {
    let mut whole = RunBucket::new(0);
    let Some(coarsest) = [Tier::TenMinutes, Tier::Minute, Tier::Recent]
        .into_iter()
        .find(|tier| series.buckets(*tier).next().is_some())
    else {
        return whole;
    };
    for bucket in series.buckets(coarsest) {
        whole.merge(bucket);
    }
    for (tier, bucket) in series.filling() {
        if tier < coarsest {
            whole.merge(&bucket);
        }
    }
    whole
}

/// The finest size whose buckets reach back to the run's start (within a
/// bucket); the coarsest held otherwise.
fn chart_tier(series: &RunSeries, started_utc_ns: i64) -> Option<Tier> {
    let held: Vec<Tier> = [Tier::Recent, Tier::Minute, Tier::TenMinutes]
        .into_iter()
        .filter(|tier| series.buckets(*tier).next().is_some())
        .collect();
    held.iter()
        .copied()
        .find(|tier| {
            series.buckets(*tier).next().is_some_and(|first| {
                let step = i64::try_from(tier.step_ns()).unwrap_or(i64::MAX);
                first.start_utc_ns <= started_utc_ns.saturating_add(step)
            })
        })
        .or_else(|| held.last().copied())
}

/// An even grid over buckets from the first bucket's start; a point spans
/// one or more buckets' time.
struct Grid {
    first_utc_ns: i64,
    step_ns: u64,
    points: usize,
}

impl Grid {
    fn of(buckets: &[&RunBucket], bucket_step_ns: u64) -> Self {
        let (Some(first), Some(last)) = (buckets.first(), buckets.last()) else {
            return Self {
                first_utc_ns: 0,
                step_ns: bucket_step_ns,
                points: 0,
            };
        };
        let step = i64::try_from(bucket_step_ns.max(1)).unwrap_or(i64::MAX);
        let span =
            usize::try_from((last.start_utc_ns - first.start_utc_ns) / step + 1).unwrap_or(1);
        let per_point = span.div_ceil(MAX_RUN_CHART_POINTS).max(1);
        Self {
            first_utc_ns: first.start_utc_ns,
            step_ns: bucket_step_ns.saturating_mul(u64::try_from(per_point).unwrap_or(1)),
            points: span.div_ceil(per_point),
        }
    }

    /// The mean of `metric` in each point; `None` where no bucket has one.
    fn means(&self, buckets: &[&RunBucket], metric: RunMetric) -> Vec<Option<f64>> {
        let mut points = vec![Aggregate::default(); self.points];
        let step = i64::try_from(self.step_ns.max(1)).unwrap_or(i64::MAX);
        for bucket in buckets {
            let index = usize::try_from((bucket.start_utc_ns - self.first_utc_ns) / step).ok();
            if let Some(point) = index.and_then(|index| points.get_mut(index)) {
                point.merge(bucket.value(metric));
            }
        }
        points.iter().map(Aggregate::mean).collect()
    }
}

const fn unit_of(metric: RunMetric) -> CanonicalUnit {
    match metric {
        RunMetric::Cpu | RunMetric::GpuUtilization => CanonicalUnit::Percent,
        RunMetric::Memory | RunMetric::GpuMemory => CanonicalUnit::Bytes,
        RunMetric::DiskRead
        | RunMetric::DiskWrite
        | RunMetric::HostNetworkReceive
        | RunMetric::HostNetworkTransmit => CanonicalUnit::BytesPerSecond,
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

    fn row(pid: i64, start_ticks: u64, gpu_bytes: u64) -> ProcessRow {
        let mut row = gpu_process(pid, "acme_llm.train", &[(GPU, Some(gpu_bytes))]);
        row.key.start_time_ticks = start_ticks;
        row.cpu_percent = Some(50.0);
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

    fn view(book: &ProcessHistoryBook, opened: Option<&ProcessKey>) -> ProcessHistoryView {
        build_process_history_view(&RunHistoryInputs {
            book,
            colors: &ProcessMemoryBook::new(1000),
            opened,
            kept_on_disk: true,
        })
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
        #[allow(clippy::cast_precision_loss)]
        let held = (4u64 << 30) as f64;
        assert_eq!(first.peak_gpu_memory, Some(held));
        assert_eq!(first.mean_cpu, Some(50.0));
        assert_eq!(view.more_runs, 0);
        assert!(view.opened.is_none());
        assert!(view.kept_on_disk);
    }

    /// An opened run charts every metric on its finest bucket size that
    /// covers it whole.
    #[test]
    fn an_opened_run_charts_each_metric() {
        let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
        // Started 1 s after boot, sampled from 2 s to 31 s.
        for second in 2..32 {
            observe(&mut book, second, vec![row(1, 100, 4 << 30)]);
        }
        let key = test_process_key(1);
        let mut key = key;
        key.start_time_ticks = 100;
        let view = view(&book, Some(&key));
        let opened = view.opened.unwrap();
        assert_eq!(opened.step_ns, 5 * SECOND);
        assert_eq!(opened.charts.len(), RunMetric::ALL.len());
        let cpu = &opened.charts[RunMetric::Cpu.index()];
        assert_eq!(cpu.unit, CanonicalUnit::Percent);
        assert_eq!(
            cpu.means.len(),
            7,
            "0-5 s … 25-30 s and 30-35 s being filled"
        );
        assert!(cpu.means.iter().all(|mean| *mean == Some(50.0)));
        let network = &opened.charts[RunMetric::HostNetworkReceive.index()];
        assert_eq!(network.mean, Some(10.0));
        let disk = &opened.charts[RunMetric::DiskRead.index()];
        assert!(disk.means.iter().all(Option::is_none), "no probe, no rate");
        assert!(!opened.loading);
    }

    /// A long grid merges neighbouring buckets so a chart stays bounded.
    #[test]
    fn long_grids_merge_into_bounded_points() {
        let mut buckets = Vec::new();
        for index in 0..1000 {
            let mut bucket = RunBucket::new(index * 60 * 1_000_000_000);
            bucket.add(RunMetric::Cpu, Some(10.0));
            buckets.push(bucket);
        }
        let refs: Vec<&RunBucket> = buckets.iter().collect();
        let grid = Grid::of(&refs, 60 * SECOND);
        let means = grid.means(&refs, RunMetric::Cpu);
        assert!(means.len() <= MAX_RUN_CHART_POINTS);
        assert_eq!(grid.step_ns, 5 * 60 * SECOND);
        assert!(means.iter().all(|mean| *mean == Some(10.0)));
        let empty = Grid::of(&[], 60 * SECOND);
        assert!(empty.means(&[], RunMetric::Cpu).is_empty());
    }
}
