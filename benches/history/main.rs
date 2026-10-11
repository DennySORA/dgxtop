//! History meso benchmark: the real hot store's bounded history with a
//! realistic-shape 24 h preload of 512 series (`perf::history_load`), then
//!
//! * append: one 1 s commit of every series (gauges + exact counter
//!   intervals that bridge minute boundaries) followed by the per-second
//!   maintenance the Manager requests, back to back;
//! * queries: two concurrent clients (`history_workers = 2`) issuing the
//!   HistoryService's mix — mean/min/max at the 160-column resolution over
//!   5m/1h/6h/12h/24h windows plus window statistics, exact totals for rate
//!   series — while a writer keeps committing at the live 1 Hz cadence.
//!
//! Budget mapping: query latency (submit → result of ≤ 2000 points) is
//! `history_p95_ms` of `bench/budgets.json`. Append and maintenance have no
//! budget entry of their own and are reported as raw stage timings.

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
};
use dgxtop::perf::latency::{LatencySummary, duration_us, summarize_us};
use dgxtop::perf::report::{BenchMode, RunRecord};
use dgxtop_core::budget::ByteLedger;
use dgxtop_core::clock::{NANOS_PER_SECOND, TimeProvider, ms_to_ns};
use dgxtop_core::config::Config;
use dgxtop_core::history::{HISTORY_SCHEMA_VERSION, HistoryQuery, HistoryStat};
use dgxtop_core::identity::{BootId, HostId};
use dgxtop_core::ports::{CancelFlag, HistoryReader, StoreWriter};
use dgxtop_core::quality::Namespace;
use dgxtop_history::HistoryIndex;
use dgxtop_store::hot::HotStoreWriter;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

const BENCH: &str = "history";
const RUNS: usize = 3;
const QUERY_CLIENTS: usize = 2;
/// Display columns of the REF terminal: the HistoryService's points target.
const POINTS_TARGET: u64 = 160;
const WINDOWS: [(&str, u64); 5] = [
    ("5m", 300),
    ("1h", 3_600),
    ("6h", 21_600),
    ("12h", 43_200),
    ("24h", 86_400),
];
/// Boot time at which the preload ends (24 h of history fit before it).
const PRELOAD_END_NS: u64 = 48 * 3_600 * NANOS_PER_SECOND;

/// Each client issues one query per interval (far above the
/// HistoryService's rate, so every run collects enough samples while the
/// writer's commits overlap the queries).
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

/// Continuous boot clock: `offset + real time since creation`. Continuity
/// matters because queries check deadlines and cap open steps at "now";
/// it only jumps forward ([`BenchClock::rebase`]) while no query runs.
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
        1_700_000_000_000_000_000 + i128::from(self.now_boot_ns())
    }
}

/// One query of the HistoryService mix.
struct PlannedQuery {
    window: &'static str,
    purpose: &'static str,
    query: HistoryQuery,
}

fn planned_query(index: usize, series: &[HistorySeries], now_ns: u64) -> PlannedQuery {
    let target = &series[(index * 7) % series.len()];
    let (window, span_s) = WINDOWS[index % WINDOWS.len()];
    let span_ms = span_s * 1000;
    let (purpose, stat, resolution_ms, max_points) = match (index / WINDOWS.len()) % 4 {
        0 => (
            "main_mean",
            HistoryStat::Mean,
            span_ms.div_ceil(POINTS_TARGET),
            POINTS_TARGET + 1,
        ),
        1 => (
            "main_min",
            HistoryStat::Min,
            span_ms.div_ceil(POINTS_TARGET),
            POINTS_TARGET + 1,
        ),
        2 => (
            "main_max",
            HistoryStat::Max,
            span_ms.div_ceil(POINTS_TARGET),
            POINTS_TARGET + 1,
        ),
        _ if target.rate => ("window_total", HistoryStat::Total, span_ms, 2),
        _ => ("window_mean", HistoryStat::Mean, span_ms, 2),
    };
    PlannedQuery {
        window,
        purpose,
        query: HistoryQuery {
            schema_version: HISTORY_SCHEMA_VERSION,
            request_id: Uuid::new_v4(),
            key: target.key.clone(),
            start_boot_ns: now_ns.saturating_sub(span_ms * 1_000_000),
            end_boot_ns: now_ns,
            resolution_ms: u32::try_from(resolution_ms.max(100)).unwrap_or(86_400_000),
            stat,
            max_points: u32::try_from(max_points).unwrap_or(2),
            source_filter: None,
        },
    }
}

/// Outcome of one query.
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
        &config.runtime,
        ledger.clone(),
        Namespace::Live,
        false,
    )
    .into_parts();
    let clock = BenchClock::new(PRELOAD_END_NS)?;
    let (mut feeder, preload, preload_seconds) =
        preload_history(&config, spec.seed, &clock, &mut writer)?;
    let ledger_after_preload = ledger.snapshot();
    let coverage_probe = probe_day_coverage(&history, feeder.series(), &clock);
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
            &history,
            &clock,
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
        "meso benchmark: measures history queries/appends only; other REF metrics are not measured here".to_owned(),
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
            "history_p95_ms is measured over every query of the run (failed queries included, counted under errors)".to_owned(),
            "bulk history is committed at a 60 s cadence (same rollups/bridges/segments as 1 s sampling), the newest 300 s at 1 s".to_owned(),
        ],
    };
    common::finish(outcome, &spec, &config, environment, &mut evidence, &paths)
}

fn preload_history(
    config: &Config,
    seed: u64,
    clock: &BenchClock,
    writer: &mut HotStoreWriter,
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

/// Mean coverage of a 24 h window over the first gauge series (proves the
/// preload is a full day of aggregates, not a few minutes).
fn probe_day_coverage(
    history: &HistoryIndex,
    series: &[HistorySeries],
    clock: &BenchClock,
) -> serde_json::Value {
    let Some(gauge) = series.iter().find(|series| !series.rate) else {
        return json!(null);
    };
    let now = clock.now_boot_ns();
    let query = HistoryQuery {
        schema_version: HISTORY_SCHEMA_VERSION,
        request_id: Uuid::new_v4(),
        key: gauge.key.clone(),
        start_boot_ns: now.saturating_sub(86_400 * NANOS_PER_SECOND),
        end_boot_ns: now,
        resolution_ms: 540_000,
        stat: HistoryStat::Mean,
        max_points: 161,
        source_filter: None,
    };
    match history.query(&query, u64::MAX, &CancelFlag::new(), clock) {
        Ok(result) => json!({
            "series": gauge.key.series_selector(),
            "coverage_ratio": result.coverage_ratio,
            "points": result.points.len(),
            "truncated": result.truncated,
        }),
        Err(error) => json!({"error": error.message}),
    }
}

/// Back-to-back 1 s appends (each followed by maintenance); returns the
/// boot time of the last append.
fn append_back_to_back(
    feeder: &mut HistoryFeeder,
    writer: &mut HotStoreWriter,
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

/// Two paced query clients run the mix for `phase` while a writer commits
/// once per second.
fn query_under_writes(
    feeder: &mut HistoryFeeder,
    writer: &mut HotStoreWriter,
    history: &HistoryIndex,
    clock: &BenchClock,
    config: &Config,
    phase: Duration,
    samples: &mut RunSamples,
) {
    let finished = AtomicBool::new(false);
    let series = feeder.series().to_vec();
    let deadline_ns = ms_to_ns(u64::from(config.runtime.history_deadline_ms));
    let started = Instant::now();
    let phase_end = started + phase;
    let (results, appends) = std::thread::scope(|scope| {
        let writer_thread = scope.spawn(|| paced_writer(feeder, writer, clock, &finished));
        let clients: Vec<_> = (0..QUERY_CLIENTS)
            .map(|client| {
                let series = &series;
                scope.spawn(move || {
                    paced_client(client, phase_end, |index| {
                        run_query(history, series, clock, deadline_ns, index)
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

/// Issues query `client, client + CLIENTS, ...` every [`QUERY_INTERVAL`]
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
    writer: &mut HotStoreWriter,
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

fn run_query(
    history: &HistoryIndex,
    series: &[HistorySeries],
    clock: &BenchClock,
    deadline_ns: u64,
    index: usize,
) -> QuerySample {
    let planned = planned_query(index, series, clock.now_boot_ns());
    let started = Instant::now();
    let deadline = clock.now_boot_ns().saturating_add(deadline_ns);
    let result = history.query(&planned.query, deadline, &CancelFlag::new(), clock);
    let latency_us = duration_us(started.elapsed());
    let (points, error) = match result {
        Ok(result) => (result.points.len(), None),
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
        &format!("run{index}/history_query_us.json"),
        &RawSamples {
            stream: "history_query",
            unit: "us",
            definition: "HistoryReader::query submit -> result (<= 2000 points), 2 concurrent clients, 1 Hz writer",
            samples: &latencies,
        },
    )?;
    common::write_samples(
        evidence,
        &format!("run{index}/append_commit_us.json"),
        &RawSamples {
            stream: "append_commit",
            unit: "us",
            definition: "HotStoreWriter::commit_collection of one 1 s cycle of 512 history series",
            samples: &samples.append_us,
        },
    )?;
    common::write_samples(
        evidence,
        &format!("run{index}/append_maintain_us.json"),
        &RawSamples {
            stream: "append_maintain",
            unit: "us",
            definition: "HotStoreWriter::maintain after each append (rollup finalization, eviction)",
            samples: &samples.maintain_us,
        },
    )
}
