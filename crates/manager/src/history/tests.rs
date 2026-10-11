use super::*;
use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::commands::SortKey;
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::history::{ArchivePoint, HistoryMethod, HistoryPoint};
use dgxtop_core::metric::{CanonicalUnit, Scalar};
use dgxtop_core::testing::{CapturingDispatch, FakeClock, metric_key};
use dgxtop_core::view::HistoryWindowSummary;

/// A pick of `charts` and no statistics.
fn picks(charts: Vec<HistoryChartSpec>) -> HistoryPicks {
    HistoryPicks {
        charts,
        stats: Vec::new(),
    }
}

const SECOND: u64 = NANOS_PER_SECOND;
const NOW: u64 = 100_000 * SECOND;
/// A 120-column plot: one panel 144 columns wide inside the border, less
/// its gutter and legend (D-50).
/// 60 columns of plot: a 1 m bar each is an hour (D-78).
const COLUMNS: u16 = 86;

struct Bench {
    service: HistoryService,
    port: CapturingDispatch<HistoryJob>,
    archive: CapturingDispatch<ArchiveReadJob>,
    clock: Arc<FakeClock>,
    session: SessionId,
}

fn bench(max_inflight: usize) -> Bench {
    bench_with_archive(max_inflight, None)
}

/// A bench whose archive holds the time before boot time `since` (D-51).
fn bench_with_archive(max_inflight: usize, since: Option<u64>) -> Bench {
    let port = CapturingDispatch::new();
    let archive = CapturingDispatch::new();
    let clock = FakeClock::new(NOW);
    let mut service = HistoryService::new(
        Box::new(port.clone()),
        since.map(|since_boot_ns| ArchiveSide {
            port: Box::new(archive.clone()),
            since_boot_ns,
        }),
        clock.clone(),
        HistorySettings {
            max_inflight,
            deadline_ms: 200,
            max_points: 2000,
        },
        1000,
    );
    let session = SessionId::new("s1").unwrap();
    service.open_session(&session);
    Bench {
        service,
        port,
        archive,
        clock,
        session,
    }
}

impl Bench {
    /// The wall-clock time of boot time `boot_ns`.
    fn utc(&self, boot_ns: u64) -> u64 {
        Wall::now(self.clock.as_ref()).utc_of(boot_ns)
    }
}

fn state(view: ViewKind, filter: &str) -> SessionState {
    SessionState {
        session_id: SessionId::new("s1").unwrap(),
        view,
        selected_entity_id: None,
        selected_process: None,
        filter: filter.into(),
        sort: SortKey::GpuMemory,
        ascending: false,
    }
}

fn cpu() -> HistoryChartSpec {
    HistoryChartSpec {
        kind: HistoryChartKind::CpuUtilization,
        subject: String::new(),
        key: metric_key("cpu", "cpu.utilization.total", "host"),
        scale_max: Some(100.0),
    }
}

fn read_rate(entity: &str) -> HistoryChartSpec {
    HistoryChartSpec {
        kind: HistoryChartKind::DiskRead,
        subject: entity.to_owned(),
        key: metric_key(entity, "disk.read.bytes_per_second", "device"),
        scale_max: None,
    }
}

impl Bench {
    /// The session on the history view with `charts` picked.
    fn open(&mut self, filter: &str, charts: &[HistoryChartSpec]) {
        self.open_with_stats(filter, charts, &[]);
    }

    /// The session on the history view with `charts` and the statistics of
    /// `stats` picked.
    fn open_with_stats(
        &mut self,
        filter: &str,
        charts: &[HistoryChartSpec],
        stats: &[HistoryChartSpec],
    ) {
        self.service.sync_session(
            &self.session,
            &state(ViewKind::History, filter),
            COLUMNS,
            NOW,
        );
        let picked = HistoryPicks {
            charts: charts.to_vec(),
            stats: stats.to_vec(),
        };
        self.service.update_charts(1, move |_| picked.clone(), NOW);
    }

    fn answer(&mut self, job: &HistoryJob, values: &[Option<f64>]) -> Option<SessionId> {
        self.service.on_result(
            job.query.request_id,
            &self.session,
            job.session_generation,
            Ok(result_for(job, values)),
        )
    }
}

fn result_for(job: &HistoryJob, values: &[Option<f64>]) -> HistoryResult {
    let span = job.query.span_ns();
    let step = span / u64::try_from(values.len().max(1)).unwrap();
    let points = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let start = job.query.start_boot_ns + step * u64::try_from(index).unwrap();
            HistoryPoint {
                start_boot_ns: start,
                end_boot_ns: start + step,
                value: value.map(|number| Scalar::from_f64(number).unwrap()),
                covered_ns: if value.is_some() { step } else { 0 },
                method: HistoryMethod::TimeWeightedStep,
                source_ids: Vec::new(),
                reason: None,
            }
        })
        .collect();
    HistoryResult {
        schema_version: 1,
        request_id: job.query.request_id,
        key: job.query.key.clone(),
        start_boot_ns: job.query.start_boot_ns,
        end_boot_ns: job.query.end_boot_ns,
        covered_ns: span / 2,
        coverage_ratio: 0.5,
        truncated: false,
        points,
        source_changes: Vec::new(),
        clock_discontinuities: Vec::new(),
        snapshot_revision: 1,
        next_start_boot_ns: None,
    }
}

#[test]
fn every_chart_queries_one_window_at_a_bounded_resolution() {
    let mut bench = bench(4);
    assert!(bench.service.sync_session(
        &bench.session,
        &state(ViewKind::History, ""),
        COLUMNS,
        NOW
    ));
    // On the view the session waits for its charts.
    assert!(bench.service.wants_charts(1));
    assert_eq!(bench.service.tick(NOW, false).dispatched, 0);
    let dirty =
        bench
            .service
            .update_charts(1, |_| picks(vec![cpu(), read_rate("blockdev:8:0")]), NOW);
    assert_eq!(dirty, vec![bench.session.clone()]);
    assert!(!bench.service.wants_charts(1));
    assert!(bench.service.wants_charts(2), "a new snapshot picks again");
    let tick = bench.service.tick(NOW, false);
    assert_eq!(tick.dispatched, 4, "bounded by queues.history_requests");
    let jobs = bench.port.take();
    // The mean and, since a 30 s point merges samples, the maximum.
    let purposes: Vec<(&str, HistoryStat)> = jobs
        .iter()
        .map(|job| (job.query.key.metric_id.as_str(), job.query.stat))
        .collect();
    assert_eq!(
        purposes,
        vec![
            ("cpu.utilization.total", HistoryStat::Mean),
            ("cpu.utilization.total", HistoryStat::Max),
            ("disk.read.bytes_per_second", HistoryStat::Mean),
            ("disk.read.bytes_per_second", HistoryStat::Max),
        ]
    );
    for job in &jobs {
        assert_eq!(job.query.end_boot_ns, NOW, "one time axis for all charts");
        assert_eq!(
            job.query.span_ns(),
            3_600 * SECOND,
            "default 1 m bars, 60 of them"
        );
        assert_eq!(job.query.resolution_ms, 60_000, "a point a bar");
        assert!(job.query.max_points <= 61);
        assert_eq!(job.deadline_boot_ns, NOW + 200 * NANOS_PER_MILLI);
    }
    let view = bench.service.view(&bench.session);
    assert_eq!(view.window_label, "1m");
    assert_eq!(view.end_utc_ns, bench.utc(NOW));
    assert_eq!(view.end_utc_ns - view.start_utc_ns, 3_600 * SECOND);
    assert_eq!(view.disk, DiskHistory::Off);
    assert!(view.charts.iter().all(|chart| chart.pending));
    assert_eq!(view.charts[1].subject, "blockdev:8:0");
}

/// The filter's first token is the bar size (D-78): a bar of it for each
/// column; the Process History's after it changes nothing here (D-77).
#[test]
fn a_bar_size_charts_a_bar_a_column() {
    let mut bench = bench(4);
    bench.open("6h 30m", &[cpu()]);
    bench.service.tick(NOW, false);
    let jobs = bench.port.take();
    assert!(!jobs.is_empty());
    for job in &jobs {
        // 60 bars of 6 h reach before boot: memory answers from boot on.
        assert_eq!(job.query.start_boot_ns, 0);
        assert_eq!(job.query.resolution_ms, 6 * 3_600_000);
    }
    let view = bench.service.view(&bench.session);
    assert_eq!(view.window_label, "6h");
    assert_eq!(view.requested_span_ns, 60 * 6 * 3_600 * SECOND);
    // 24 h bars: 60 days asked; memory answers from boot.
    bench.open("24h", &[cpu()]);
    bench.service.tick(NOW, false);
    assert!(
        bench
            .port
            .take()
            .iter()
            .all(|job| job.query.resolution_ms == 86_400_000)
    );
}

#[test]
fn results_give_points_mean_peak_and_coverage_and_late_ones_are_dropped() {
    let mut bench = bench(4);
    bench.open("1m", &[cpu()]);
    bench.service.tick(NOW, false);
    let jobs = bench.port.take();
    let by_stat = |stat| jobs.iter().find(|job| job.query.stat == stat).unwrap();
    let (mean, max) = (by_stat(HistoryStat::Mean), by_stat(HistoryStat::Max));
    assert_eq!(
        bench.answer(mean, &[Some(10.0), None, None, Some(30.0)]),
        Some(bench.session.clone())
    );
    bench.answer(max, &[Some(15.0), None, None, Some(45.0)]);
    let view = bench.service.view(&bench.session);
    let chart = &view.charts[0];
    assert!(!chart.pending);
    assert_eq!(chart.points.len(), 4);
    assert_eq!(
        (chart.points[0].mean, chart.points[3].max),
        (Some(10.0), Some(45.0))
    );
    assert_eq!(chart.points[1].mean, None, "a gap stays a gap");
    assert_eq!(chart.mean, Some(20.0), "time-weighted over covered points");
    assert_eq!(chart.peak, Some(45.0), "from the bucket maxima");
    assert_eq!(chart.coverage_ratio, Some(0.5));
    assert!(chart.display_downsampled);
    assert_eq!(chart.unit, CanonicalUnit::Percent);
    // A window change makes the results still in flight stale.
    bench.service.tick(NOW, false);
    let pending = bench.port.take();
    bench.service.sync_session(
        &bench.session,
        &state(ViewKind::History, "5m"),
        COLUMNS,
        NOW,
    );
    for job in &pending {
        assert!(job.cancel.is_cancelled());
        assert_eq!(bench.answer(job, &[Some(1.0)]), None);
    }
    assert!(bench.service.view(&bench.session).charts[0].pending);
}

#[test]
fn a_new_series_restarts_the_queries_and_a_new_scale_only_redraws() {
    let mut bench = bench(4);
    bench.open("1m", &[cpu()]);
    bench.service.tick(NOW, false);
    let first = bench.port.take();
    let mut rescaled = cpu();
    rescaled.scale_max = Some(50.0);
    let dirty = bench
        .service
        .update_charts(2, move |_| picks(vec![rescaled.clone()]), NOW);
    assert_eq!(dirty.len(), 1);
    assert!(first.iter().all(|job| !job.cancel.is_cancelled()));
    assert_eq!(
        bench.service.view(&bench.session).charts[0].scale_max,
        Some(50.0)
    );
    assert!(bench.answer(&first[0], &[Some(1.0)]).is_some());
    // The pick receives what the session shows, and a new device restarts.
    bench.service.update_charts(
        3,
        |request| {
            assert_eq!(request.shown[0].scale_max, Some(50.0));
            picks(vec![read_rate("blockdev:8:0")])
        },
        NOW,
    );
    assert!(first[1].cancel.is_cancelled());
    assert_eq!(bench.answer(&first[1], &[Some(1.0)]), None);
    let view = bench.service.view(&bench.session);
    assert_eq!(view.charts.len(), 1);
    assert!(view.charts[0].pending);
    // An unchanged pick changes nothing.
    assert!(
        bench
            .service
            .update_charts(4, |request| picks(request.shown.to_vec()), NOW)
            .is_empty()
    );
}

#[test]
fn timeouts_and_errors_show_on_their_chart_and_leaving_cancels() {
    let mut bench = bench(1);
    bench.open("", &[cpu(), read_rate("blockdev:8:0")]);
    bench.service.tick(NOW, false);
    let job = bench.port.take().pop().unwrap();
    // Overdue: the slot is reclaimed and the error shows on the chart.
    let later = NOW + 400 * NANOS_PER_MILLI;
    let tick = bench.service.tick(later, false);
    assert_eq!(tick.expired, 1);
    assert!(job.cancel.is_cancelled());
    assert_eq!(tick.dispatched, 1, "the next query uses the freed slot");
    let view = bench.service.view(&bench.session);
    assert_eq!(
        view.charts[0].error.as_deref(),
        Some("history query timed out")
    );
    assert!(!view.charts[0].pending);
    // The freed slot went to chart 0's maximum; answered, the next goes to
    // chart 1's mean, whose refusal shows on chart 1 only.
    let maximum = bench.port.take().pop().unwrap();
    assert_eq!(maximum.query.stat, HistoryStat::Max);
    bench.answer(&maximum, &[Some(1.0)]);
    bench.service.tick(later, false);
    let next = bench.port.take().pop().unwrap();
    assert_eq!(
        (next.query.stat, next.query.key.metric_id.as_str()),
        (HistoryStat::Mean, "disk.read.bytes_per_second")
    );
    bench.service.on_result(
        next.query.request_id,
        &bench.session,
        next.session_generation,
        Err(ErrorDetail::new(
            dgxtop_core::errors::ErrorCode::QueryTooLarge,
            "scan limit",
        )),
    );
    let view = bench.service.view(&bench.session);
    let errors: Vec<Option<&str>> = view
        .charts
        .iter()
        .map(|chart| chart.error.as_deref())
        .collect();
    assert_eq!(
        errors,
        vec![Some("history query timed out"), Some("scan limit")]
    );
    assert!(bench.service.sync_session(
        &bench.session,
        &state(ViewKind::Overview, ""),
        COLUMNS,
        later
    ));
    assert!(!bench.service.sync_session(
        &bench.session,
        &state(ViewKind::Overview, ""),
        COLUMNS,
        later
    ));
    assert!(!bench.service.wants_charts(9), "only sessions on the view");
    bench.service.cancel_all();
    assert_eq!(bench.service.inflight_count(), 0);
    bench.service.close_session(&bench.session);
    assert!(bench.service.view(&bench.session).charts.is_empty());
}

fn gpu_series(entity: &str, kind: HistoryChartKind, metric: &str) -> HistoryChartSpec {
    HistoryChartSpec {
        kind,
        subject: format!("{} · NVIDIA L4", entity.trim_start_matches("gpu:")),
        key: metric_key(entity, metric, "gpu"),
        scale_max: None,
    }
}

/// GPU A's utilization and power, summarized over 1 h to 24 h.
fn gpu_stats() -> Vec<HistoryChartSpec> {
    vec![
        gpu_series(
            "gpu:GPU-a",
            HistoryChartKind::GpuUtilization,
            "gpu.utilization.compute",
        ),
        gpu_series("gpu:GPU-a", HistoryChartKind::GpuPower, "gpu.power.draw"),
    ]
}

/// Whether a job draws a chart of the 1 h window (30 s points) rather than
/// summarizing a whole window.
fn charts_one_hour(job: &HistoryJob) -> bool {
    job.query.resolution_ms == 60_000
}

#[test]
fn the_history_view_summarizes_each_gpu_over_four_windows_once_a_minute() {
    let mut bench = bench(64);
    bench.open_with_stats("1m", &[cpu()], &gpu_stats());
    bench.service.tick(NOW, false);
    let jobs = bench.port.take();
    let (charted, summarized): (Vec<&HistoryJob>, Vec<&HistoryJob>) =
        jobs.iter().partition(|job| charts_one_hour(job));
    // The chart's mean and maximum go first, then two series, four windows,
    // the mean and the maximum of each.
    assert!(jobs.iter().take(2).all(charts_one_hour));
    assert_eq!(charted.len(), 2);
    assert_eq!(summarized.len(), 2 * 4 * 2);
    let mut spans: Vec<u64> = summarized
        .iter()
        .map(|job| job.query.span_ns() / SECOND)
        .collect();
    spans.sort_unstable();
    spans.dedup();
    assert_eq!(spans, vec![3_600, 21_600, 43_200, 86_400]);
    for job in &summarized {
        assert_eq!(job.query.end_boot_ns, NOW);
        assert_eq!(
            u64::from(job.query.resolution_ms) * 1_000_000,
            job.query.span_ns(),
            "one point over the whole window"
        );
        assert_eq!(job.query.max_points, 2);
    }
    for job in &jobs {
        let power = job.query.key.metric_id.as_str() == "gpu.power.draw";
        let value = match (power, job.query.stat) {
            (true, HistoryStat::Mean) => 100.0,
            (true, _) => 250.0,
            (false, HistoryStat::Mean) => 40.0,
            (false, _) => 90.0,
        };
        bench.answer(job, &[Some(value)]);
    }
    let view = bench.service.view(&bench.session);
    assert_eq!(view.charts.len(), 1, "the statistics draw no chart");
    assert_eq!(view.summaries.len(), 4 * 2);
    let hour: Vec<&HistoryWindowSummary> = view
        .summaries
        .iter()
        .filter(|summary| summary.window_label == "1h")
        .collect();
    assert_eq!(
        hour.iter().map(|summary| summary.kind).collect::<Vec<_>>(),
        vec![HistoryChartKind::GpuUtilization, HistoryChartKind::GpuPower]
    );
    assert!(
        hour.iter()
            .all(|summary| summary.subject == "GPU-a · NVIDIA L4")
    );
    let power = hour[1];
    assert_eq!((power.mean, power.peak), (Some(100.0), Some(250.0)));
    assert_eq!(power.coverage_ratio, Some(0.5));
    assert_eq!(power.unit, CanonicalUnit::Watts);
    // 100 W over the covered hour (one point covering it all) is 360 kJ.
    assert_eq!(power.integral, Some(360_000.0));
    assert_eq!(hour[0].integral, None, "only power has an energy");
    assert!(view.summaries.iter().all(|summary| !summary.pending));
    // The chart refreshes at its resolution (a minute), as the statistics.
    bench.service.tick(NOW + 30 * SECOND, false);
    assert!(bench.port.take().is_empty());
    bench.service.tick(NOW + 60 * SECOND, false);
    assert_eq!(bench.port.take().len(), 2 + 2 * 4 * 2);
}

#[test]
fn new_statistics_restart_only_their_queries_and_other_views_query_nothing() {
    let mut bench = bench(64);
    bench.open_with_stats("1m", &[cpu()], &gpu_stats());
    bench.service.tick(NOW, false);
    let first = bench.port.take();
    // A second GPU appears: the statistics query again, the chart goes on.
    let mut more = gpu_stats();
    more.push(gpu_series(
        "gpu:GPU-b",
        HistoryChartKind::GpuUtilization,
        "gpu.utilization.compute",
    ));
    let dirty = bench.service.update_charts(
        2,
        move |request| HistoryPicks {
            charts: request.shown.to_vec(),
            stats: more.clone(),
        },
        NOW,
    );
    assert_eq!(dirty, vec![bench.session.clone()]);
    let (charted, summarized): (Vec<&HistoryJob>, Vec<&HistoryJob>) =
        first.iter().partition(|job| charts_one_hour(job));
    assert!(charted.iter().all(|job| !job.cancel.is_cancelled()));
    assert!(summarized.iter().all(|job| job.cancel.is_cancelled()));
    assert!(bench.answer(charted[0], &[Some(1.0)]).is_some());
    assert_eq!(bench.answer(summarized[0], &[Some(1.0)]), None, "stale");
    let view = bench.service.view(&bench.session);
    assert_eq!(view.summaries.len(), 3 * 4);
    assert!(view.summaries.iter().all(|summary| summary.pending));
    // The GPU view and the other views query no history (D-48).
    assert!(bench.service.sync_session(
        &bench.session,
        &state(ViewKind::GpuDetail, ""),
        COLUMNS,
        NOW
    ));
    assert!(!bench.service.wants_charts(3));
    bench.service.tick(NOW + 120 * SECOND, false);
    assert!(bench.port.take().is_empty());
    let left = bench.service.view(&bench.session);
    assert!(left.summaries.iter().all(|summary| !summary.pending));
}

const MINUTE: u64 = 60 * SECOND;

impl Bench {
    /// Answers an archive read with `points` of (minutes before this run's
    /// start, mean, covered minutes), each as wide as it covers.
    fn answer_archive(&mut self, job: &ArchiveReadJob, points: &[(u64, f64, u64)]) {
        let seam = job.query.end_utc_ns;
        let result = ArchiveResult {
            request_id: job.query.request_id,
            points: points
                .iter()
                .map(|(before, mean, covered)| ArchivePoint {
                    start_utc_ns: seam - before * MINUTE,
                    end_utc_ns: seam - before * MINUTE + covered * MINUTE,
                    mean: Some(*mean),
                    max: Some(mean + 1.0),
                    covered_ns: covered * MINUTE,
                })
                .collect(),
            covered_ns: points.iter().map(|point| point.2 * MINUTE).sum(),
            boots: 1,
            truncated: false,
        };
        self.service.on_archive_result(
            job.query.request_id,
            &self.session,
            job.session_generation,
            Ok(result),
        );
    }
}

#[test]
fn the_time_before_this_run_is_read_from_the_archive_once_a_generation() {
    // This run started half an hour ago.
    let since = NOW - 30 * MINUTE;
    let mut bench = bench_with_archive(4, Some(since));
    bench.open("1m", &[cpu(), read_rate("blockdev:8:0")]);
    bench.service.tick(NOW, false);
    let reads = bench.archive.take();
    assert_eq!(reads.len(), 2, "one read per chart");
    for read in &reads {
        assert_eq!(read.query.end_utc_ns, bench.utc(since), "up to this run");
        assert_eq!(read.query.start_utc_ns, bench.utc(NOW) - 60 * MINUTE);
        // The view's 1 m bars in the archive's whole minutes.
        assert_eq!(read.query.point_ns, MINUTE);
        assert_eq!(read.deadline_boot_ns, NOW + 200 * NANOS_PER_MILLI);
    }
    assert_eq!(
        reads[0].query.series,
        SeriesInEveryBoot::from(&cpu().key),
        "the series in every boot"
    );
    for read in &reads {
        bench.answer_archive(read, &[(5, 40.0, 1)]);
    }
    // Refresh rounds read memory again, never the archive.
    for second in 1..=120 {
        bench.service.tick(NOW + second * SECOND, false);
    }
    assert!(!bench.port.take().is_empty());
    assert!(bench.archive.take().is_empty());
    // A new window is a new generation: read again, at its resolution.
    bench.service.sync_session(
        &bench.session,
        &state(ViewKind::History, "5m"),
        COLUMNS,
        NOW + 121 * SECOND,
    );
    bench.service.tick(NOW + 121 * SECOND, false);
    let again = bench.archive.take();
    assert_eq!(again.len(), 2);
    assert!(again.iter().all(|read| read.query.point_ns == 5 * MINUTE));
}

#[test]
fn archived_minutes_lead_the_chart_on_the_wall_clock() {
    let since = NOW - 30 * MINUTE;
    let mut bench = bench_with_archive(4, Some(since));
    bench.open("1m", &[cpu()]);
    bench.service.tick(NOW, false);
    for job in bench.port.take() {
        // This run: two values over its half hour, 15 minutes each.
        bench.service.on_result(
            job.query.request_id,
            &bench.session,
            job.session_generation,
            Ok(HistoryResult {
                points: vec![
                    point(since, 15 * MINUTE, 20.0),
                    point(since + 15 * MINUTE, 15 * MINUTE, 60.0),
                ],
                covered_ns: 30 * MINUTE,
                ..result_for(&job, &[])
            }),
        );
    }
    let read = bench.archive.take().remove(0);
    // Ten minutes recorded by an earlier run, 20 and 11 minutes before
    // this one.
    bench.answer_archive(&read, &[(20, 80.0, 5), (11, 100.0, 5)]);
    let view = bench.service.view(&bench.session);
    assert_eq!(view.disk, DiskHistory::On);
    assert_eq!(view.end_utc_ns - view.start_utc_ns, 60 * MINUTE);
    let chart = &view.charts[0];
    let starts: Vec<u64> = chart
        .points
        .iter()
        .map(|point| (view.end_utc_ns - point.start_utc_ns) / MINUTE)
        .collect();
    assert_eq!(starts, vec![50, 41, 30, 15], "oldest first, minutes ago");
    assert_eq!(chart.covered_ns, Some(40 * MINUTE));
    // (80×5 + 100×5 + 20×15 + 60×15) / 40.
    assert!(
        (chart.mean.unwrap() - 52.5).abs() < 1e-9,
        "{:?}",
        chart.mean
    );
    assert_eq!(chart.peak, Some(101.0));
}

#[test]
fn a_failed_archive_read_shows_and_is_asked_again_next_round() {
    let mut bench = bench_with_archive(4, Some(NOW - 30 * MINUTE));
    bench.open("1m", &[cpu()]);
    bench.service.tick(NOW, false);
    let read = bench.archive.take().remove(0);
    bench.service.on_archive_result(
        read.query.request_id,
        &bench.session,
        read.session_generation,
        Err(ErrorDetail::new(ErrorCode::Unavailable, "disk on fire")),
    );
    let view = bench.service.view(&bench.session);
    assert_eq!(view.disk, DiskHistory::Failed("disk on fire".to_owned()));
    // Not again on every tick, only with the next round (1 m bars
    // refresh every minute).
    bench.service.tick(NOW + SECOND, false);
    assert!(bench.archive.take().is_empty());
    bench.service.tick(NOW + 60 * SECOND, false);
    let retry = bench.archive.take();
    assert_eq!(retry.len(), 1);
    bench.answer_archive(&retry[0], &[]);
    assert_eq!(bench.service.view(&bench.session).disk, DiskHistory::On);
    // A late answer to a read of an older generation is dropped.
    assert_eq!(
        bench.service.on_archive_result(
            read.query.request_id,
            &bench.session,
            read.session_generation,
            Ok(ArchiveResult::empty(read.query.request_id)),
        ),
        None
    );
}

#[test]
fn archive_reads_run_two_at_a_time_and_time_out() {
    let mut bench = bench_with_archive(64, Some(NOW - 30 * MINUTE));
    let charts = vec![cpu(), read_rate("a"), read_rate("b"), read_rate("c")];
    bench.open("1m", &charts);
    bench.service.tick(NOW, false);
    assert_eq!(bench.archive.take().len(), MAX_ARCHIVE_INFLIGHT);
    // Past the deadline and its grace: the slots free up, the view says
    // why and the next reads go out.
    bench.service.tick(NOW + 400 * NANOS_PER_MILLI, false);
    let view = bench.service.view(&bench.session);
    assert_eq!(
        view.disk,
        DiskHistory::Failed("history archive read timed out".to_owned())
    );
    assert_eq!(bench.archive.take().len(), MAX_ARCHIVE_INFLIGHT);
}

#[test]
fn without_an_archive_the_window_starts_at_boot_and_nothing_is_read() {
    // Booted 10 minutes before the bench's now: a 1 h window is charted
    // from boot (D-50).
    let mut bench = bench(4);
    bench.clock.set_boot_ns(10 * MINUTE);
    bench.service.sync_session(
        &bench.session,
        &state(ViewKind::History, "1m"),
        COLUMNS,
        10 * MINUTE,
    );
    let view = bench.service.view(&bench.session);
    assert_eq!(view.disk, DiskHistory::Off);
    assert_eq!(view.end_utc_ns - view.start_utc_ns, 10 * MINUTE);
    bench.service.tick(10 * MINUTE, false);
    assert!(bench.archive.take().is_empty());
}

/// A memory point of `covered` time at `value` starting at boot `start`.
fn point(start: u64, covered: u64, value: f64) -> HistoryPoint {
    HistoryPoint {
        start_boot_ns: start,
        end_boot_ns: start + covered,
        value: Some(Scalar::from_f64(value).unwrap()),
        covered_ns: covered,
        method: HistoryMethod::TimeWeightedStep,
        source_ids: Vec::new(),
        reason: None,
    }
}
