use super::*;
use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::commands::SortKey;
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::process_model::MemoryPart;
use dgxtop_core::testing::{CapturingDispatch, FakeClock, metric_key};

const SECOND: u64 = NANOS_PER_SECOND;
const NOW: u64 = 100_000 * SECOND;
const MINUTE: i64 = 60 * 1_000_000_000;
const HOUR: i64 = 60 * MINUTE;
/// A plot of 60 columns inside an 86-column terminal (D-50): a bar each.
const COLUMNS: u16 = 86;
const BARS: i64 = 60;

/// A pick of `charts`.
fn picks(charts: Vec<HistoryChartSpec>) -> HistoryPicks {
    HistoryPicks { charts }
}

struct Bench {
    service: HistoryService,
    port: CapturingDispatch<HistoryJob>,
    clock: Arc<FakeClock>,
    session: SessionId,
}

fn bench(max_inflight: usize) -> Bench {
    bench_at(max_inflight, 0)
}

/// A bench whose local clock is `utc_offset_s` east of UTC.
fn bench_at(max_inflight: usize, utc_offset_s: i32) -> Bench {
    let port = CapturingDispatch::new();
    let clock = FakeClock::new(NOW);
    let mut service = HistoryService::new(
        Box::new(port.clone()),
        clock.clone(),
        HistorySettings {
            max_inflight,
            deadline_ms: 200,
            max_points: 2000,
            utc_offset_s,
            kept_on_disk: true,
        },
        1000,
    );
    let session = SessionId::new("s1").unwrap();
    service.open_session(&session);
    Bench {
        service,
        port,
        clock,
        session,
    }
}

/// The session on `view` with the Overview History's bar size `bar`.
fn state(view: ViewKind, bar: &str) -> SessionState {
    SessionState {
        session_id: SessionId::new("s1").unwrap(),
        view,
        selected_entity_id: None,
        selected_process: None,
        filter: String::new(),
        sort: SortKey::GpuMemory,
        ascending: false,
        overview_bar: BarSize::parse(bar).unwrap_or(BarSize::OneMinute),
        process_bar: BarSize::Auto,
    }
}

fn cpu() -> HistoryChartSpec {
    HistoryChartSpec {
        kind: HistoryChartKind::CpuUtilization,
        subject: String::new(),
        key: metric_key("cpu", "cpu.utilization.total", "host"),
        scale_max: Some(100.0),
        parts_of: BTreeSet::new(),
    }
}

fn read_rate(entity: &str) -> HistoryChartSpec {
    HistoryChartSpec {
        kind: HistoryChartKind::DiskRead,
        subject: entity.to_owned(),
        key: metric_key(entity, "disk.read.bytes_per_second", "device"),
        scale_max: None,
        parts_of: BTreeSet::new(),
    }
}

fn gpu_series(kind: HistoryChartKind, metric: &str) -> HistoryChartSpec {
    HistoryChartSpec {
        kind,
        subject: "0 · NVIDIA L4".to_owned(),
        key: metric_key("gpu:GPU-a", metric, "gpu"),
        scale_max: None,
        parts_of: BTreeSet::new(),
    }
}

/// A bar starting at `start` holding `minutes` minutes at `value`.
#[allow(clippy::cast_precision_loss)]
fn bar(start: i64, minutes: u64, value: f64) -> OverviewBucket {
    let covered = minutes * 60 * SECOND;
    OverviewBucket::gauge(
        start,
        covered,
        value * covered as f64,
        (value, value + 5.0),
        12,
    )
}

/// The history's answer of `bars` starting at `start`, its totals merged
/// from them as the history merges them (D-99).
fn bars_answer(start: i64, bars: Vec<OverviewBucket>) -> OverviewAnswer {
    let mut whole = OverviewBucket::new(start);
    for bar in &bars {
        whole.merge(bar);
    }
    answer_of(bars, whole, Vec::new())
}

fn answer_of(
    bars: Vec<OverviewBucket>,
    whole: OverviewBucket,
    parts: Vec<MemoryPart>,
) -> OverviewAnswer {
    OverviewAnswer::Bars(OverviewBars {
        bars,
        whole,
        parts,
        kept_on_disk: true,
        store_issue: None,
    })
}

impl Bench {
    fn now_utc(&self) -> i64 {
        i64::try_from(self.clock.now_utc_ns()).unwrap()
    }

    /// The session on the history view with `charts` picked.
    fn open(&mut self, bar: &str, charts: &[HistoryChartSpec]) {
        self.service
            .sync_session(&self.session, &state(ViewKind::History, bar), COLUMNS, NOW);
        let picked = picks(charts.to_vec());
        self.service.update_charts(1, move |_| picked.clone(), NOW);
    }

    fn answer(&mut self, job: &HistoryJob, answer: OverviewAnswer) -> Option<SessionId> {
        self.service.on_result(
            job.request_id,
            &self.session,
            job.session_generation,
            Ok(answer),
        )
    }

    fn sent(&mut self, now: u64) -> Vec<HistoryJob> {
        self.service.tick(now, false);
        self.port.take()
    }
}

fn bars_request(job: &HistoryJob) -> (Tier, i64, i64, i64) {
    match &job.request {
        OverviewRequest::Bars {
            tier,
            from_utc_ns,
            end_utc_ns,
            at_utc_ns,
            ..
        } => (*tier, *from_utc_ns, *end_utc_ns, *at_utc_ns),
        OverviewRequest::Windows { .. } => panic!("bars expected"),
    }
}

/// Every chart reads the same bars: one per plot column, aligned to the
/// local clock, the last one being filled now.
#[test]
fn every_chart_reads_the_same_local_clock_bars() {
    let mut bench = bench_at(4, 8 * 3_600);
    bench.open("1h", &[cpu(), read_rate("blockdev:8:0")]);
    let jobs = bench.sent(NOW);
    assert_eq!(jobs.len(), 2);
    let now = bench.now_utc();
    let local_hour = Tier::OneHour.align(now, 8 * HOUR);
    for job in &jobs {
        let (tier, from, end, at) = bars_request(job);
        assert_eq!(tier, Tier::OneHour);
        assert_eq!(end, local_hour + HOUR, "the bar being filled is the last");
        assert_eq!(from, end - BARS * HOUR, "a bar per column");
        assert_eq!(at, now);
    }
    let view = bench.service.view(&bench.session);
    assert_eq!(view.window_label, "1h");
    assert_eq!(view.requested_span_ns, (BARS * HOUR).unsigned_abs());
    assert_eq!(view.end_utc_ns, u64::try_from(local_hour + HOUR).unwrap());
    assert_eq!(view.utc_offset_s, 8 * 3_600);
    assert!(view.charts.iter().all(|chart| chart.pending));
}

/// A round's view is built once its last answer arrived: bars as points
/// (a bar is the mean of what it holds, a missing bar is a gap), the
/// chart's mean, peak and time held merged from them.
#[test]
fn a_round_is_drawn_once_all_its_answers_arrived() {
    let mut bench = bench(4);
    bench.open("1h", &[cpu(), read_rate("blockdev:8:0")]);
    let jobs = bench.sent(NOW);
    let (_, from, end, _) = bars_request(&jobs[0]);
    let open = end - HOUR;
    // Two hours ago a 45-minute hour, an hour ago nothing, now 20 minutes.
    let answered = vec![bar(open - 2 * HOUR, 45, 30.0), bar(open, 20, 60.0)];
    assert_eq!(bench.answer(&jobs[0], bars_answer(from, answered)), None);
    assert!(
        bench.service.view(&bench.session).charts[0].pending,
        "not drawn yet"
    );
    let done = bench.answer(&jobs[1], bars_answer(from, Vec::new()));
    assert_eq!(done, Some(bench.session.clone()));
    let view = bench.service.view(&bench.session);
    let chart = &view.charts[0];
    assert!(!chart.pending);
    let spans: Vec<(i64, i64, Option<f64>)> = chart
        .points
        .iter()
        .map(|point| {
            let start = i64::try_from(point.start_utc_ns).unwrap();
            let end = i64::try_from(point.end_utc_ns).unwrap();
            ((start - open) / HOUR, (end - start) / MINUTE, point.mean)
        })
        .collect();
    assert_eq!(spans, vec![(-2, 60, Some(30.0)), (0, 60, Some(60.0))]);
    assert_eq!(chart.points[0].max, Some(35.0));
    assert_eq!(chart.covered_ns, Some(65 * 60 * SECOND));
    let mean = (30.0 * 45.0 + 60.0 * 20.0) / 65.0;
    assert!((chart.mean.unwrap() - mean).abs() < 1e-9);
    assert_eq!(chart.peak, Some(65.0));
    assert!(chart.display_downsampled, "a bar merges samples");
    assert_eq!(view.charts[1].covered_ns, Some(0), "nothing recorded");
    assert_eq!(view.start_utc_ns, u64::try_from(from).unwrap());
    // A late answer of the same round is stale.
    assert_eq!(bench.answer(&jobs[0], bars_answer(from, Vec::new())), None);
}

/// Every round reads a chart's whole window: the history's sizes are its
/// cache, so the service keeps only the latest answer and draws it as it
/// came (D-99), the window's mean, peak and time held included.
#[test]
fn every_round_reads_the_whole_window_and_draws_its_answer() {
    let mut bench = bench(4);
    bench.open("1h", &[cpu()]);
    let first = bench.sent(NOW).pop().unwrap();
    let (_, window_start, end, _) = bars_request(&first);
    let open = end - HOUR;
    bench.answer(
        &first,
        bars_answer(
            window_start,
            vec![bar(open - HOUR, 60, 10.0), bar(open, 20, 40.0)],
        ),
    );
    // The next round, a minute later, asks the same whole window.
    let later = NOW + 61 * SECOND;
    bench.clock.advance_ns(61 * SECOND);
    let next = bench.sent(later).pop().unwrap();
    let (_, from, _, _) = bars_request(&next);
    assert_eq!(from, window_start, "the whole window, every round");
    // The history's totals are drawn as they came, never merged again.
    let mut whole = OverviewBucket::new(window_start);
    whole.merge(&bar(open, 21, 41.0));
    whole.covered_ns = 90 * 60 * SECOND;
    bench.answer(
        &next,
        answer_of(vec![bar(open, 21, 41.0)], whole, Vec::new()),
    );
    let chart = &bench.service.view(&bench.session).charts[0];
    let means: Vec<Option<f64>> = chart.points.iter().map(|point| point.mean).collect();
    assert_eq!(means, vec![Some(41.0)], "the latest answer only");
    assert_eq!(chart.covered_ns, Some(90 * 60 * SECOND));
    assert_eq!(chart.peak, Some(46.0));
}

/// A memory chart asks for the processes' parts of the GPUs it names and
/// draws them as the history joined them (D-64, D-99); other charts ask
/// for none.
#[test]
fn a_memory_chart_carries_the_parts_the_history_joined() {
    let mut bench = bench(4);
    let gpu = EntityId::new("gpu:GPU-a").unwrap();
    let mut memory = gpu_series(HistoryChartKind::GpuMemory, "gpu.memory.process_allocated");
    memory.parts_of = BTreeSet::from([gpu.clone()]);
    bench.open("1h", &[cpu(), memory]);
    let jobs = bench.sent(NOW);
    let asked: Vec<BTreeSet<EntityId>> = jobs
        .iter()
        .map(|job| match &job.request {
            OverviewRequest::Bars { parts_of, .. } => parts_of.clone(),
            OverviewRequest::Windows { .. } => panic!("bars expected"),
        })
        .collect();
    assert_eq!(asked, vec![BTreeSet::new(), BTreeSet::from([gpu])]);
    let (_, from, _, _) = bars_request(&jobs[1]);
    let part = MemoryPart {
        slot: 2,
        means: vec![Some(6.0), None],
    };
    bench.answer(&jobs[0], bars_answer(from, Vec::new()));
    bench.answer(
        &jobs[1],
        answer_of(Vec::new(), OverviewBucket::new(from), vec![part]),
    );
    let view = bench.service.view(&bench.session);
    assert!(view.charts[0].parts.is_empty());
    let parts: Vec<(u8, Vec<Option<f64>>)> = view.charts[1]
        .parts
        .iter()
        .map(|part| (part.slot, part.means.clone()))
        .collect();
    assert_eq!(parts, vec![(2, vec![Some(6.0), None])]);
}

/// The history view reads its charts only (D-107): the GPUs' 1 h to 24 h
/// statistics are gone, at the first round and at every refresh.
#[test]
fn the_history_view_reads_its_charts_only() {
    let mut bench = bench(8);
    let power = gpu_series(HistoryChartKind::GpuPower, "gpu.power.draw");
    bench.open("1m", &[cpu(), power]);
    let charts_only = |jobs: &[HistoryJob]| {
        jobs.iter()
            .all(|job| matches!(job.request, OverviewRequest::Bars { .. }))
    };
    let first = bench.sent(NOW);
    assert_eq!(first.len(), 2);
    assert!(charts_only(&first));
    let refreshed = bench.sent(NOW + 61 * SECOND);
    assert!(!refreshed.is_empty() && charts_only(&refreshed));
}

#[test]
fn a_new_series_restarts_the_reads_and_a_new_scale_only_redraws() {
    let mut bench = bench(4);
    bench.open("1m", &[cpu()]);
    let first = bench.sent(NOW);
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
    // The pick receives what the session shows, and a new device restarts.
    bench.service.update_charts(
        3,
        |request| {
            assert_eq!(request.shown[0].scale_max, Some(50.0));
            picks(vec![read_rate("blockdev:8:0")])
        },
        NOW,
    );
    assert!(first[0].cancel.is_cancelled());
    assert_eq!(bench.answer(&first[0], bars_answer(0, Vec::new())), None);
    let view = bench.service.view(&bench.session);
    assert_eq!(view.charts.len(), 1);
    assert!(view.charts[0].pending);
    assert!(
        bench
            .service
            .update_charts(4, |request| picks(request.shown.to_vec()), NOW)
            .is_empty(),
        "an unchanged pick changes nothing"
    );
}

#[test]
fn timeouts_and_errors_show_on_their_chart_and_leaving_cancels() {
    let mut bench = bench(1);
    bench.open("1m", &[cpu(), read_rate("blockdev:8:0")]);
    let job = bench.sent(NOW).pop().unwrap();
    // Overdue: the slot is reclaimed and the error shows on the chart.
    let later = NOW + 400 * dgxtop_core::clock::NANOS_PER_MILLI;
    let tick = bench.service.tick(later, false);
    assert_eq!(tick.expired, 1);
    assert!(job.cancel.is_cancelled());
    assert_eq!(tick.dispatched, 1, "the next read uses the freed slot");
    let next = bench.port.take().pop().unwrap();
    assert_eq!(
        next.request.series().metric_id.as_str(),
        "disk.read.bytes_per_second"
    );
    let done = bench.service.on_result(
        next.request_id,
        &bench.session,
        next.session_generation,
        Err(ErrorDetail::new(ErrorCode::Unavailable, "history stopped")),
    );
    assert!(done.is_some(), "the round is over: drawn");
    let view = bench.service.view(&bench.session);
    let errors: Vec<Option<&str>> = view
        .charts
        .iter()
        .map(|chart| chart.error.as_deref())
        .collect();
    assert_eq!(
        errors,
        vec![Some("history read timed out"), Some("history stopped")]
    );
    assert!(view.charts.iter().all(|chart| !chart.pending));
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
    assert!(
        bench.sent(later + 120 * SECOND).is_empty(),
        "other views read nothing"
    );
    bench.service.cancel_all();
    assert_eq!(bench.service.inflight_count(), 0);
    bench.service.close_session(&bench.session);
    assert!(bench.service.view(&bench.session).charts.is_empty());
}

/// Where the history before this run comes from follows what the history
/// says: on disk, failed with its reason, or only while dgxtop runs.
#[test]
fn the_disk_state_follows_the_history() {
    let mut bench = bench(4);
    bench.open("1m", &[cpu()]);
    assert_eq!(bench.service.view(&bench.session).disk, DiskHistory::On);
    let job = bench.sent(NOW).pop().unwrap();
    let failing = OverviewAnswer::Bars(OverviewBars {
        bars: Vec::new(),
        whole: OverviewBucket::new(0),
        parts: Vec::new(),
        kept_on_disk: true,
        store_issue: Some("history tiers: disk full".to_owned()),
    });
    bench.answer(&job, failing);
    assert_eq!(
        bench.service.view(&bench.session).disk,
        DiskHistory::Failed("history tiers: disk full".to_owned())
    );
    bench.clock.advance_ns(61 * SECOND);
    let job = bench.sent(NOW + 61 * SECOND).pop().unwrap();
    let off = OverviewAnswer::Bars(OverviewBars {
        bars: Vec::new(),
        whole: OverviewBucket::new(0),
        parts: Vec::new(),
        kept_on_disk: false,
        store_issue: None,
    });
    bench.answer(&job, off);
    assert_eq!(bench.service.view(&bench.session).disk, DiskHistory::Off);
}

/// A throttled session refreshes three times slower; the bars' refresh
/// follows their size between 5 s and a minute.
#[test]
fn refreshes_follow_the_bar_size_and_slow_down_when_throttled() {
    let mut bench = bench(4);
    bench.open("1m", &[cpu()]);
    bench.sent(NOW);
    assert_eq!(
        bench.service.next_deadline(),
        Some(NOW + 200_000_000 + RESULT_GRACE_NS)
    );
    let first = bench.port.take();
    assert!(first.is_empty());
    bench.service.tick(NOW + 61 * SECOND, true);
    let refreshed = bench.port.take();
    assert_eq!(refreshed.len(), 1, "a minute bar refreshes every minute");
    bench.service.tick(NOW + 125 * SECOND, false);
    assert!(
        bench.port.take().is_empty(),
        "three times slower while throttled"
    );
    assert_eq!(
        BarGrid::new(BarSize::OneDay, 10).refresh_ms(),
        MAX_REFRESH_MS
    );
    assert_eq!(BarGrid::new(BarSize::Auto, 10).bar, BarSize::OneMinute);
}
