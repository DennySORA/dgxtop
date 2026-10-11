//! History meso benchmark: the real hot store's history with a
//! realistic-shape 24 h preload of 512 series (`perf::history_load`), then
//!
//! * append: one 1 s commit of every series (gauges + exact counter
//!   intervals that bridge minute boundaries) followed by the per-second
//!   maintenance the Manager requests, back to back;
//! * reads: two concurrent clients (`history_workers = 2`) issuing the
//!   HistoryService's mix through `OverviewReader::read` (D-82, D-90): the
//!   160 bars of a chart in every bar size from 1 m to 24 h, and the
//!   1/6/12/24 h window statistics, over the Overview series, while a
//!   writer keeps committing at the live 1 Hz cadence.
//!
//! Budget mapping: read latency (submit → answer) is `history_p95_ms` of
//! `bench/budgets.json`. Append and maintenance have no budget entry of
//! their own and are reported as raw stage timings.

#[path = "../common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::{BenchOutcome, BenchResult, RawSamples};
use dgxtop::live_store::LiveStore;
use dgxtop::perf::evidence::EvidenceWriter;
use dgxtop::perf::history_load::{
    HistoryFeeder, HistorySeries, PreloadReport, PreloadShape, history_series, synthetic_keys,
    synthetic_utc_ns,
};
use dgxtop::perf::latency::{LatencySummary, duration_us, summarize_us};
use dgxtop::perf::report::{BenchMode, RunRecord};
use dgxtop_core::budget::ByteLedger;
use dgxtop_core::clock::{NANOS_PER_SECOND, TimeProvider, ms_to_ns};
use dgxtop_core::config::Config;
use dgxtop_core::history::{SeriesInEveryBoot, coverage_ratio};
use dgxtop_core::identity::{BootId, HostId};
use dgxtop_core::overview_history::{OverviewAnswer, OverviewRequest, is_overview_series};
use dgxtop_core::ports::{CancelFlag, OverviewReader, StoreWriter};
use dgxtop_core::process_history::{BarSize, Tier};
use dgxtop_core::quality::Namespace;
use dgxtop_history::HistoryIndex;
use dgxtop_state::StateWriter;
use serde::Serialize;
use serde_json::json;

const BENCH: &str = "history";
const RUNS: usize = 3;
const QUERY_CLIENTS: usize = 2;
/// Display columns of the REF terminal: the bars one chart asks for.
const BARS_TARGET: i64 = 160;
/// The window statistics' spans (the GPU statistics' last 1/6/12/24 h).
const STAT_SPANS_S: [u64; 4] = [3_600, 21_600, 43_200, 86_400];
/// Of every this many reads, one asks for window statistics (the charts
/// read more often than the slowly changing statistics).
const READS_PER_STATS: usize = 4;
/// Boot time at which the preload ends (24 h of history fit before it).
const PRELOAD_END_NS: u64 = 48 * 3_600 * NANOS_PER_SECOND;

/// Each client issues one read per interval (far above the
/// HistoryService's rate, so every run collects enough samples while the
/// writer's commits overlap the reads).
const QUERY_INTERVAL: Duration = Duration::from_millis(10);

struct Sizes {
    query_phase: Duration,
    append_cycles: u64,
}

const fn sizes(mode: BenchMode) -> Sizes {
    match mode {
        BenchMode::Smoke => Sizes {
            query_phase: Duration::from_secs(4),
            append_cycles: 120,
        },
        BenchMode::Full => Sizes {
            query_phase: Duration::from_secs(30),
            append_cycles: 1_800,
        },
    }
}

/// Continuous boot clock: `offset + real time since creation`, on the UTC
/// timeline the preload paired. Continuity matters because reads check
/// deadlines and merge the minutes not finalized yet up to "now"; it only
/// jumps forward ([`BenchClock::rebase`]) while no read runs.
struct BenchClock {
    boot_id: BootId,
    origin: Instant,
    offset_ns: AtomicU64,
}

impl BenchClock {
    fn new(base_ns: u64) -> Result<Self, String> {
        Ok(Self {
            boot_id: BootId::new("bench-history-boot").map_err(|error| error.to_string())?,
            origin: Instant::now(),
            offset_ns: AtomicU64::new(base_ns),
        })
    }

    fn elapsed_ns(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Moves "now" forward to at least `base_ns` (never backwards).
    fn rebase(&self, base_ns: u64) {
        let target = base_ns.max(self.now_boot_ns());
        self.offset_ns
            .store(target.saturating_sub(self.elapsed_ns()), Ordering::Release);
    }
}

impl TimeProvider for BenchClock {
    fn boot_id(&self) -> BootId {
        self.boot_id.clone()
    }

    fn now_boot_ns(&self) -> u64 {
        self.offset_ns
            .load(Ordering::Acquire)
            .saturating_add(self.elapsed_ns())
    }

    fn now_utc_ns(&self) -> i128 {
        synthetic_utc_ns(self.now_boot_ns())
    }
}

/// One read of the HistoryService mix.
struct PlannedQuery {
    window: &'static str,
    purpose: &'static str,
    request: OverviewRequest,
}

/// The bar sizes a chart can show.
fn bar_sizes() -> Vec<BarSize> {
    BarSize::ALL
        .into_iter()
        .filter(|bar| bar.tier().is_some())
        .collect()
}

fn planned_query(index: usize, series: &[HistorySeries], at_utc_ns: i64) -> PlannedQuery {
    let target = &series[(index * 7) % series.len()];
    let key = SeriesInEveryBoot::from(&target.key);
    if index % READS_PER_STATS == READS_PER_STATS - 1 {
        return PlannedQuery {
            window: "1h/6h/12h/24h",
            purpose: "windows",
            request: OverviewRequest::Windows {
                series: key,
                spans_ns: STAT_SPANS_S
                    .iter()
                    .map(|seconds| seconds * NANOS_PER_SECOND)
                    .collect(),
                at_utc_ns,
            },
        };
    }
    let sizes = bar_sizes();
    let bar = sizes[(index / READS_PER_STATS) % sizes.len()];
    let tier = bar.tier().unwrap_or(Tier::OneMinute);
    let step = i64::try_from(tier.step_ns()).unwrap_or(i64::MAX);
    let end = tier.align(at_utc_ns, 0).saturating_add(step);
    PlannedQuery {
        window: bar.token(),
        purpose: "bars",
        request: OverviewRequest::Bars {
            series: key,
            tier,
            from_utc_ns: end.saturating_sub(BARS_TARGET.saturating_mul(step)),
            end_utc_ns: end,
            at_utc_ns,
        },
    }
}

fn utc_now(clock: &BenchClock) -> i64 {
    i64::try_from(clock.now_utc_ns()).unwrap_or(i64::MAX)
}

/// Outcome of one read.
struct QuerySample {
    latency_us: u64,
    window: &'static str,
    purpose: &'static str,
    points: usize,
    error: Option<String>,
}

#[derive(Default)]
struct RunSamples {
    queries: Vec<QuerySample>,
    append_us: Vec<u64>,
    maintain_us: Vec<u64>,
    paced_appends: u64,
    duration_s: f64,
}

fn main() -> ExitCode {
    common::run_bench(BENCH, run)
}

fn run() -> BenchResult<PathBuf> {
    let args = common::parse_args(BENCH, false)?;
    let spec = common::load_spec()?;
    let config = common::reference_config()?;
    let sizes = sizes(args.mode);
    let paths = common::output_paths(BENCH, &args);
    let environment = common::environment(
        "none (history store only; no viewer)",
        "2 query client threads + 1 writer thread at 1 Hz during queries",
    )?;
    let ledger = ByteLedger::for_memory_limit_mib(u64::from(config.store.memory_limit_mib));
    let (mut writer, _snapshots, history) = LiveStore::new(
        &config.store,
        &config.sampling,
        ledger.clone(),
        Namespace::Live,
        false,
    )
    .into_parts();
    let clock = BenchClock::new(PRELOAD_END_NS)?;
    let (mut feeder, preload, preload_seconds) =
        preload_history(&config, spec.seed, &clock, &mut writer)?;
    let ledger_after_preload = ledger.snapshot();
    let charted: Vec<HistorySeries> = feeder
        .series()
        .iter()
        .filter(|series| {
            is_overview_series(series.key.metric_id.as_str(), series.key.entity_id.as_str())
        })
        .cloned()
        .collect();
    if charted.is_empty() {
        return Err("the preload holds no Overview series to read".into());
    }
    let coverage_probe = probe_day_coverage(&history, &charted, &clock);
    let mut evidence = common::open_evidence(&paths)?;
    let mut runs = Vec::new();
    let mut run_details = Vec::new();
    let mut last_append_ns = clock.now_boot_ns();
    for index in 1..=RUNS {
        let mut samples = RunSamples::default();
        last_append_ns = append_back_to_back(
            &mut feeder,
            &mut writer,
            last_append_ns,
            sizes.append_cycles,
            &mut samples,
        );
        clock.rebase(last_append_ns);
        query_under_writes(
            &mut feeder,
            &mut writer,
            Reads {
                history: &history,
                series: &charted,
                clock: &clock,
            },
            &config,
            sizes.query_phase,
            &mut samples,
        );
        last_append_ns = clock.now_boot_ns();
        write_run_evidence(&mut evidence, index, &samples)?;
        let latencies: Vec<u64> = samples
            .queries
            .iter()
            .map(|sample| sample.latency_us)
            .collect();
        let mut record = RunRecord {
            duration_seconds: samples.duration_s,
            history_queries: u64::try_from(latencies.len()).unwrap_or(u64::MAX),
            ..RunRecord::default()
        };
        if let Some(summary) = summarize_us(&latencies) {
            record.set("history_p95_ms", summary.p95_ms, summary.samples)?;
        }
        run_details.push(run_detail(&samples, &config));
        runs.push(record);
    }
    let mut extra_blockers = vec![
        "meso benchmark: measures history reads/appends only; other REF metrics are not measured here".to_owned(),
    ];
    if preload.rejected > 0 {
        extra_blockers.push(format!(
            "{} preload commits were rejected",
            preload.rejected
        ));
    }
    let outcome = BenchOutcome {
        bench: BENCH,
        mode: args.mode,
        runs,
        soak: None,
        details: json!({
            "preload": {
                "report": preload,
                "span_seconds": preload.span_seconds(),
                "wall_seconds": preload_seconds,
                "shape": PreloadShape::reference(),
                "ledger_after_preload": ledger_after_preload,
                "day_window_coverage_probe": coverage_probe,
                "labels": "synthetic values (reason SYNTHETIC_HISTORY_PRELOAD) in a private in-memory store",
            },
            "query_clients": QUERY_CLIENTS,
            "writer_cadence_during_queries": "1 commit + maintenance per real second",
            "deadline_ms": config.runtime.history_deadline_ms,
            "runs": run_details,
        }),
        extra_blockers,
        notes: vec![
            "history_p95_ms is measured over every Overview read of the run (failed reads included, counted under errors)".to_owned(),
            "bulk history is committed at a 60 s cadence (same rollups/bridges/segments as 1 s sampling), the newest 300 s at 1 s".to_owned(),
        ],
    };
    common::finish(outcome, &spec, &config, environment, &mut evidence, &paths)
}

fn preload_history(
    config: &Config,
    seed: u64,
    clock: &BenchClock,
    writer: &mut StateWriter,
) -> BenchResult<(HistoryFeeder, PreloadReport, f64)> {
    let host = HostId::new("bench-history-host").map_err(|error| error.to_string())?;
    let limit = usize::try_from(config.store.max_series).unwrap_or(512);
    let keys = synthetic_keys(&host, &clock.boot_id(), limit).map_err(|error| error.to_string())?;
    let mut feeder = HistoryFeeder::new(history_series(keys, limit), seed, &config.sampling)
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    let report = feeder.preload(writer, PreloadShape::reference(), clock.now_boot_ns());
    Ok((feeder, report, started.elapsed().as_secs_f64()))
}

/// Coverage of a 24 h window over the first Overview gauge series
/// (proves the preload is a full day in the Overview's sizes, not a few
/// minutes).
fn probe_day_coverage(
    history: &HistoryIndex,
    series: &[HistorySeries],
    clock: &BenchClock,
) -> serde_json::Value {
    let Some(gauge) = series.iter().find(|series| !series.rate) else {
        return json!(null);
    };
    let span_ns = 86_400 * NANOS_PER_SECOND;
    let request = OverviewRequest::Windows {
        series: SeriesInEveryBoot::from(&gauge.key),
        spans_ns: vec![span_ns],
        at_utc_ns: utc_now(clock),
    };
    match history.read(&request, u64::MAX, &CancelFlag::new(), clock) {
        Ok(OverviewAnswer::Windows(windows)) => json!({
            "series": gauge.key.series_selector(),
            "coverage_ratio": windows
                .first()
                .map_or(0.0, |window| coverage_ratio(window.covered_ns, span_ns)),
        }),
        Ok(OverviewAnswer::Bars(_)) => json!({"error": "asked for windows, got bars"}),
        Err(error) => json!({"error": error.message}),
    }
}

/// Back-to-back 1 s appends (each followed by maintenance); returns the
/// boot time of the last append.
fn append_back_to_back(
    feeder: &mut HistoryFeeder,
    writer: &mut StateWriter,
    from_ns: u64,
    cycles: u64,
    samples: &mut RunSamples,
) -> u64 {
    let mut at = from_ns;
    for _ in 0..cycles {
        at += NANOS_PER_SECOND;
        let started = Instant::now();
        feeder.append(writer, at, NANOS_PER_SECOND);
        samples.append_us.push(duration_us(started.elapsed()));
        let started = Instant::now();
        writer.maintain(at);
        samples.maintain_us.push(duration_us(started.elapsed()));
    }
    at
}

/// What the read clients read: the history, the series they ask for and
/// the clock their reads are timed by.
#[derive(Clone, Copy)]
struct Reads<'a> {
    history: &'a HistoryIndex,
    series: &'a [HistorySeries],
    clock: &'a BenchClock,
}

/// Two paced read clients run the mix for `phase` while a writer commits
/// once per second.
fn query_under_writes(
    feeder: &mut HistoryFeeder,
    writer: &mut StateWriter,
    reads: Reads<'_>,
    config: &Config,
    phase: Duration,
    samples: &mut RunSamples,
) {
    let finished = AtomicBool::new(false);
    let clock = reads.clock;
    let deadline_ns = ms_to_ns(u64::from(config.runtime.history_deadline_ms));
    let started = Instant::now();
    let phase_end = started + phase;
    let (results, appends) = std::thread::scope(|scope| {
        let writer_thread = scope.spawn(|| paced_writer(feeder, writer, clock, &finished));
        let clients: Vec<_> = (0..QUERY_CLIENTS)
            .map(|client| {
                scope.spawn(move || {
                    paced_client(client, phase_end, |index| {
                        run_query(reads, deadline_ns, index)
                    })
                })
            })
            .collect();
        let results: Vec<QuerySample> = clients
            .into_iter()
            .flat_map(|client| client.join().unwrap_or_default())
            .collect();
        finished.store(true, Ordering::Release);
        (results, writer_thread.join().unwrap_or(0))
    });
    samples.duration_s = started.elapsed().as_secs_f64();
    samples.queries = results;
    samples.paced_appends = appends;
}

/// Issues read `client, client + CLIENTS, ...` every [`QUERY_INTERVAL`]
/// until `phase_end`.
fn paced_client(
    client: usize,
    phase_end: Instant,
    mut query: impl FnMut(usize) -> QuerySample,
) -> Vec<QuerySample> {
    let mut results = Vec::new();
    let mut next = Instant::now();
    let mut index = client;
    while next < phase_end {
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        results.push(query(index));
        index += QUERY_CLIENTS;
        next += QUERY_INTERVAL;
    }
    results
}

fn paced_writer(
    feeder: &mut HistoryFeeder,
    writer: &mut StateWriter,
    clock: &BenchClock,
    finished: &AtomicBool,
) -> u64 {
    let mut appends = 0;
    let mut next = Instant::now() + Duration::from_secs(1);
    while !finished.load(Ordering::Acquire) {
        if Instant::now() < next {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        next += Duration::from_secs(1);
        let at = clock.now_boot_ns();
        feeder.append(writer, at, NANOS_PER_SECOND);
        writer.maintain(at);
        appends += 1;
    }
    appends
}

fn run_query(reads: Reads<'_>, deadline_ns: u64, index: usize) -> QuerySample {
    let clock = reads.clock;
    let planned = planned_query(index, reads.series, utc_now(clock));
    let started = Instant::now();
    let deadline = clock.now_boot_ns().saturating_add(deadline_ns);
    let result = reads
        .history
        .read(&planned.request, deadline, &CancelFlag::new(), clock);
    let latency_us = duration_us(started.elapsed());
    let (points, error) = match result {
        Ok(OverviewAnswer::Bars(bars)) => (bars.bars.len(), None),
        Ok(OverviewAnswer::Windows(windows)) => (windows.len(), None),
        Err(error) => (0, Some(format!("{:?}", error.code))),
    };
    QuerySample {
        latency_us,
        window: planned.window,
        purpose: planned.purpose,
        points,
        error,
    }
}

#[derive(Serialize)]
struct RunDetail {
    queries: Option<LatencySummary>,
    queries_by_window: BTreeMap<&'static str, Option<LatencySummary>>,
    queries_by_purpose: BTreeMap<&'static str, Option<LatencySummary>>,
    errors: BTreeMap<String, u64>,
    over_deadline: usize,
    max_points: usize,
    append_commit: Option<LatencySummary>,
    append_maintain: Option<LatencySummary>,
    paced_appends_during_queries: u64,
    query_phase_seconds: f64,
}

fn run_detail(samples: &RunSamples, config: &Config) -> RunDetail {
    let group = |pick: fn(&QuerySample) -> &'static str| {
        let mut grouped: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
        for sample in &samples.queries {
            grouped
                .entry(pick(sample))
                .or_default()
                .push(sample.latency_us);
        }
        grouped
            .into_iter()
            .map(|(name, latencies)| (name, summarize_us(&latencies)))
            .collect()
    };
    let mut errors = BTreeMap::new();
    for error in samples
        .queries
        .iter()
        .filter_map(|sample| sample.error.clone())
    {
        *errors.entry(error).or_insert(0) += 1;
    }
    let deadline_us = u64::from(config.runtime.history_deadline_ms) * 1000;
    let latencies: Vec<u64> = samples
        .queries
        .iter()
        .map(|sample| sample.latency_us)
        .collect();
    RunDetail {
        queries: summarize_us(&latencies),
        queries_by_window: group(|sample| sample.window),
        queries_by_purpose: group(|sample| sample.purpose),
        errors,
        over_deadline: latencies
            .iter()
            .filter(|latency| **latency > deadline_us)
            .count(),
        max_points: samples
            .queries
            .iter()
            .map(|sample| sample.points)
            .max()
            .unwrap_or(0),
        append_commit: summarize_us(&samples.append_us),
        append_maintain: summarize_us(&samples.maintain_us),
        paced_appends_during_queries: samples.paced_appends,
        query_phase_seconds: samples.duration_s,
    }
}

fn write_run_evidence(
    evidence: &mut EvidenceWriter,
    index: usize,
    samples: &RunSamples,
) -> BenchResult<()> {
    let latencies: Vec<u64> = samples
        .queries
        .iter()
        .map(|sample| sample.latency_us)
        .collect();
    common::write_samples(
        evidence,
        &format!("run{index}/history_read_us.json"),
        &RawSamples {
            stream: "history_read",
            unit: "us",
            definition: "OverviewReader::read submit -> answer (160 bars of one size, or the 1/6/12/24 h windows), 2 concurrent clients, 1 Hz writer",
            samples: &latencies,
        },
    )?;
    common::write_samples(
        evidence,
        &format!("run{index}/append_commit_us.json"),
        &RawSamples {
            stream: "append_commit",
            unit: "us",
            definition: "StateWriter::commit_collection of one 1 s cycle of 512 history series",
            samples: &samples.append_us,
        },
    )?;
    common::write_samples(
        evidence,
        &format!("run{index}/append_maintain_us.json"),
        &RawSamples {
            stream: "append_maintain",
            unit: "us",
            definition: "StateWriter::maintain after each append (rollup finalization, eviction)",
            samples: &samples.maintain_us,
        },
    )
}
