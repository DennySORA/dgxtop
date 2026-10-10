//! REF end-to-end benchmark (`PERFORMANCE.md` §1–2, AT28, AT35).
//!
//! Each run starts the full replay composition (`composition`): seven
//! resident child-process replay hosts serving slices of the seeded REF
//! fixture with 1 ms simulated API latency, the real collectors, hot store,
//! history/config/audit workers and the Manager, plus the real terminal
//! runner on a headless 160×50 terminal (`frontend`) and a 10 Hz script of
//! view commands (`script`). After warm-up it measures, over the window:
//! process-tree CPU (parent + hosts, harness threads excluded) and RSS,
//! managed bytes, command / frame / projection / history latencies,
//! coverage of scheduled sampling slots, drops, throttling, crashes and
//! stalls — three independent runs, never averaged.
//!
//! `--smoke` (default): short warm-up and window, never acceptance.
//! `--full`: 60 s warm-up and the scenario duration (1800 s) per run.
//! `--soak-results FILE`: real soak results to include (otherwise the
//! report carries zero-length soak placeholders and cannot be `measured`).

#[path = "../common/mod.rs"]
mod common;
mod composition;
mod frontend;
mod metrics;
mod probes;
mod sampler;
mod script;
mod shadow;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::headless::isolate_terminal_input;
use common::{BenchOutcome, BenchResult, RawSamples, RunDirectory};
use composition::{CompositionInputs, RefComposition};
use dgxtop::perf::coverage::{ScheduledGroup, scheduled_replay_groups};
use dgxtop::perf::evidence::EvidenceWriter;
use dgxtop::perf::fixture::{
    GeneratedFixture, slice_read_groups, write_fixture, write_host_slices,
};
use dgxtop::perf::history_load::HistorySeries;
use dgxtop::perf::latency::summarize_us;
use dgxtop::perf::report::{BenchMode, RunRecord};
use dgxtop::perf::threads::HarnessCpu;
use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::diagnostics::names;
use dgxtop_core::ports::{SessionPort, SnapshotReader};
use frontend::{Frontend, FrontendInputs, FrontendReport};
use metrics::{
    WindowData, counter_delta, cpu_breakdown, run_record, unexplained_drops, usage_by_process,
    usage_by_thread,
};
use probes::Probes;
use sampler::{Sampler, SamplerInputs};
use script::{ResultRouter, ScriptDriver, ScriptHints, ScriptInputs, ScriptReport};
use serde_json::{Value, json};
use shadow::ShadowHistory;

const BENCH: &str = "ref";
const RUNS: usize = 3;
/// Fixed per-read API delay of a replay host (`PERFORMANCE.md` §1).
const SIMULATED_HOST_LATENCY_US: u32 = 1000;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const READY_TIMEOUT: Duration = Duration::from_secs(90);
/// How long the history-capable key set must stay unchanged before preload.
const STABLE_KEYS_FOR: Duration = Duration::from_secs(3);
const MINIMUM_HISTORY_SPAN_NS: u64 = 86_400 * NANOS_PER_SECOND;

struct Sizes {
    warmup: Duration,
    window: Duration,
}

/// Engineering knob: a longer smoke window (10..=600 s) gives steadier CPU
/// numbers when comparing changes; reports stay smoke, never acceptance.
const SMOKE_SECONDS_ENV: &str = "DGXTOP_BENCH_SMOKE_SECONDS";

fn smoke_window() -> Duration {
    let seconds = std::env::var(SMOKE_SECONDS_ENV)
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .map_or(10, |seconds| seconds.clamp(10, 600));
    Duration::from_secs(seconds)
}

fn sizes(mode: BenchMode, scenario_seconds: u64) -> Sizes {
    match mode {
        BenchMode::Smoke => Sizes {
            warmup: Duration::from_secs(5),
            window: smoke_window(),
        },
        BenchMode::Full => Sizes {
            warmup: Duration::from_secs(60),
            window: Duration::from_secs(scenario_seconds),
        },
    }
}

/// Inputs shared by the three runs.
struct RunPlan {
    sizes: Sizes,
    /// The fixture split into one slice file per replay host.
    host_fixtures: Vec<PathBuf>,
    groups: Vec<ScheduledGroup>,
    adapter_hosts: u32,
    seed: u64,
    viewport: (u16, u16),
    clock_ticks_per_second: u64,
}

/// Raw latency samples of one stream (an evidence file).
struct RawStream {
    name: &'static str,
    definition: &'static str,
    samples: Vec<u64>,
}

/// One finished run.
struct RunResult {
    record: RunRecord,
    resources: Vec<sampler::ResourceSample>,
    raw: Vec<RawStream>,
    details: Value,
    blockers: Vec<String>,
}

fn main() -> ExitCode {
    common::run_bench(BENCH, run)
}

fn run() -> BenchResult<PathBuf> {
    let args = common::parse_args(BENCH, true)?;
    let spec = common::load_spec()?;
    let config = common::reference_config()?;
    let paths = common::output_paths(BENCH, &args);
    let _isolation = isolate_terminal_input()?;
    let environment = common::environment(
        "headless 160x50 TuiLoop: ANSI encoded in memory, null transport, no terminal I/O",
        "in-process harness threads (1 Hz sampler, 10 Hz command script, 1 Hz history feeder) excluded from dgxtop CPU",
    )?;
    let soak = args
        .soak_results
        .as_deref()
        .map(common::load_soak_results)
        .transpose()?;
    let root = RunDirectory::create(BENCH)?;
    let generator_profile = spec.profile.replay_profile(SIMULATED_HOST_LATENCY_US);
    let fixture = write_fixture(&root.path().join("fixture"), spec.seed, &generator_profile)
        .map_err(|error| error.to_string())?;
    let adapter_ids: Vec<String> = (1..=spec.profile.adapter_hosts)
        .map(|index| format!("replay-{index}"))
        .collect();
    let groups = scheduled_replay_groups(
        &adapter_ids,
        &slice_read_groups(&fixture.fixture, spec.profile.adapter_hosts),
        &config.sampling,
    );
    let host_fixtures = write_host_slices(&fixture, spec.profile.adapter_hosts)
        .map_err(|error| error.to_string())?;
    let plan = RunPlan {
        sizes: sizes(args.mode, spec.profile.duration_seconds),
        host_fixtures,
        groups,
        adapter_hosts: spec.profile.adapter_hosts,
        seed: spec.seed,
        viewport: (spec.profile.terminal_columns, spec.profile.terminal_rows),
        clock_ticks_per_second: environment.1.clock_ticks_per_second.unwrap_or(100),
    };
    let mut evidence = common::open_evidence(&paths)?;
    write_fixture_evidence(&mut evidence, &fixture, &plan)?;
    let mut runs = Vec::new();
    let mut run_details = Vec::new();
    let mut extra_blockers = Vec::new();
    for index in 1..=RUNS {
        println!(
            "ref run {index}/{RUNS}: warm-up {:?}, window {:?}",
            plan.sizes.warmup, plan.sizes.window
        );
        let result = run_once(&plan)?;
        write_run_evidence(&mut evidence, index, &result)?;
        extra_blockers.extend(
            result
                .blockers
                .iter()
                .map(|blocker| format!("run {index}: {blocker}")),
        );
        run_details.push(result.details);
        runs.push(result.record);
    }
    let outcome = BenchOutcome {
        bench: BENCH,
        mode: args.mode,
        runs,
        soak,
        details: json!({
            "warmup_seconds": plan.sizes.warmup.as_secs_f64(),
            "window_seconds": plan.sizes.window.as_secs_f64(),
            "scheduled_groups": plan.groups.len(),
            "runs": run_details,
        }),
        extra_blockers,
        notes: notes(),
    };
    common::finish(outcome, &spec, &config, environment, &mut evidence, &paths)
}

fn notes() -> Vec<String> {
    vec![
        "replay namespace: replay data never enters history (ADR08), so dgxtop CPU excludes live history appends; the 1 Hz shadow feeder doing that work is reported as harness CPU".to_owned(),
        "commands are submitted to the session as the Controller would (benches cannot build terminal key events); latency = submit -> Manager result, polled every 0.5 ms".to_owned(),
        "history queries are generated by the Manager's HistoryService and served from a preloaded live-shape history store (synthetic, private, in memory)".to_owned(),
        "managed_peak_mib = byte ledger used + the preloaded history's raw/aggregate/query partitions".to_owned(),
    ]
}

/// Starts a composition, warms it up, measures one window, shuts it down.
fn run_once(plan: &RunPlan) -> BenchResult<RunResult> {
    let run_dir = RunDirectory::create("ref-run")?;
    let probes = Arc::new(Probes::new());
    let composition = RefComposition::start(&CompositionInputs {
        run_dir: run_dir.path(),
        host_fixtures: &plan.host_fixtures,
        probes: Arc::clone(&probes),
    })?;
    let harness = HarnessCpu::default();
    let router = Arc::new(ResultRouter::new(
        composition.session.clone(),
        Arc::clone(&probes),
    ));
    let hints = Arc::new(Mutex::new(ScriptHints::default()));
    let frontend = Frontend::start(FrontendInputs {
        router: Arc::clone(&router),
        probes: Arc::clone(&probes),
        hints: Arc::clone(&hints),
        viewport: plan.viewport,
        fps: composition.config.viewer.fps,
    })?;
    let outcome = warm_and_measure(
        plan,
        &composition,
        &probes,
        &router,
        &hints,
        &harness,
        frontend,
    );
    let warnings = composition.shutdown();
    drop(run_dir);
    let mut result = outcome?;
    if let Value::Object(details) = &mut result.details {
        details.insert("shutdown_warnings".to_owned(), json!(warnings));
    }
    Ok(result)
}

fn warm_and_measure(
    plan: &RunPlan,
    composition: &RefComposition,
    probes: &Arc<Probes>,
    router: &Arc<ResultRouter>,
    hints: &Arc<Mutex<ScriptHints>>,
    harness: &HarnessCpu,
    frontend: Frontend,
) -> BenchResult<RunResult> {
    let ready_after = wait_until_ready(plan, composition, probes)?;
    let script = ScriptDriver::start(ScriptInputs {
        router: Arc::clone(router),
        hints: Arc::clone(hints),
        session_id: composition.session.session_id().clone(),
        harness: harness.clone(),
        probes: Arc::clone(probes),
    })?;
    let shadow = ShadowHistory::start(
        &composition.snapshots.latest(),
        &composition.config,
        plan.seed,
        Arc::clone(&composition.clock),
        harness,
    )?;
    composition
        .history
        .switch_to(Arc::new(shadow.index.clone()));
    std::thread::sleep(plan.sizes.warmup);
    let window = measure_window(plan, composition, probes, router, harness, &shadow);
    let script_report = script.stop();
    let frontend_report = frontend.stop();
    let preload = shadow.report.clone();
    let preload_seconds = shadow.preload_seconds;
    let feeder_appends = shadow.stop();
    let mut window = window?;
    window.data.stalls = window
        .data
        .stalls
        .saturating_add(script_report.stalls)
        .saturating_add(frontend_report.stalls);
    window.data.frontend_failed = frontend_report.exit.is_some();
    window.data.frames_us.clone_from(&frontend_report.frame_us);
    let record = run_record(&window.data)?;
    let details = run_details(
        &window,
        &frontend_report,
        &script_report,
        (&preload, preload_seconds, feeder_appends),
        ready_after,
        plan,
    );
    Ok(RunResult {
        record,
        resources: window.data.resources.clone(),
        raw: raw_streams(&window.data, frontend_report.frame_us),
        details,
        blockers: preload_blockers(&preload, history_key_count(composition)),
    })
}

/// The history metric's definition needs a 24 h preload of every series
/// the HistoryService can query.
fn preload_blockers(
    preload: &dgxtop::perf::history_load::PreloadReport,
    history_keys_at_end: usize,
) -> Vec<String> {
    let mut blockers = Vec::new();
    let span_ns = preload.end_ns.saturating_sub(preload.start_ns);
    if span_ns < MINIMUM_HISTORY_SPAN_NS {
        #[allow(clippy::cast_precision_loss)]
        let hours = (span_ns / NANOS_PER_SECOND) as f64 / 3600.0;
        blockers.push(format!(
            "history preload spans only {hours:.1} h (boot uptime); the 24 h query definition needs >= 24 h uptime"
        ));
    }
    if preload.series < history_keys_at_end {
        blockers.push(format!(
            "history preload covers {} of {history_keys_at_end} history-capable series",
            preload.series
        ));
    }
    blockers
}

fn raw_streams(data: &WindowData, frames_us: Vec<u64>) -> Vec<RawStream> {
    vec![
        RawStream {
            name: "commands_us",
            definition: "UpdateViewSession submit -> Manager result (10 Hz script)",
            samples: data.commands_us.clone(),
        },
        RawStream {
            name: "frames_us",
            definition: "TuiLoop step that drew a frame minus its input wait (160x50, null transport)",
            samples: frames_us,
        },
        RawStream {
            name: "projection_us",
            definition: "adapter read finished -> Manager generated the first projection holding that snapshot revision",
            samples: data.projection.latencies_us.clone(),
        },
        RawStream {
            name: "history_us",
            definition: "HistoryService job dispatch -> history worker result (ok or deadline timeout)",
            samples: data.history.latencies_us.clone(),
        },
    ]
}

/// Waits until every replay host delivered committed reads and the set of
/// history-capable series in the snapshot stopped growing for
/// [`STABLE_KEYS_FOR`] (the preload must cover every series the
/// HistoryService can be asked for).
fn wait_until_ready(
    plan: &RunPlan,
    composition: &RefComposition,
    probes: &Probes,
) -> BenchResult<f64> {
    let started = Instant::now();
    let mut keys = 0;
    let mut keys_since = Instant::now();
    loop {
        let now = composition.clock_now();
        let recent = probes.adapters_seen(
            &plan.groups,
            now.saturating_sub(5 * NANOS_PER_SECOND),
            now.saturating_add(1),
        );
        let current = history_key_count(composition);
        if current != keys {
            keys = current;
            keys_since = Instant::now();
        }
        let hosts_ready = recent.len() == composition.adapter_ids.len();
        if hosts_ready && keys > 0 && keys_since.elapsed() >= STABLE_KEYS_FOR {
            return Ok(started.elapsed().as_secs_f64());
        }
        if started.elapsed() > READY_TIMEOUT {
            return Err(not_ready(composition, recent.len(), keys));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// History-capable series currently in the snapshot.
fn history_key_count(composition: &RefComposition) -> usize {
    composition
        .snapshots
        .latest()
        .domains
        .values()
        .flat_map(|domain| domain.samples.keys())
        .filter(|key| HistorySeries::for_key((*key).clone()).is_some())
        .count()
}

fn not_ready(composition: &RefComposition, delivering: usize, keys: usize) -> String {
    let hosts: Vec<String> = composition
        .supervisor
        .statuses()
        .iter()
        .map(|status| format!("{}={:?}", status.adapter_id, status.state))
        .collect();
    format!(
        "composition not ready after {READY_TIMEOUT:?}: {delivering} of {} hosts delivered data, {keys} history series; hosts: {hosts:?}",
        composition.adapter_ids.len()
    )
}

/// Raw measurements of one window plus what the details need.
struct MeasuredWindow {
    data: WindowData,
    command_statuses: std::collections::BTreeMap<String, u64>,
    router_evicted: u64,
    statuses_after: Vec<dgxtop_core::view::HostStatus>,
    audit_sqlite_ok: bool,
    thread_usage: Vec<metrics::ThreadUsage>,
}

fn measure_window(
    plan: &RunPlan,
    composition: &RefComposition,
    probes: &Probes,
    router: &ResultRouter,
    harness: &HarnessCpu,
    shadow: &ShadowHistory,
) -> BenchResult<MeasuredWindow> {
    let diagnostics_before = composition.diagnostics.snapshot();
    let restarts_before = restarts(composition);
    let drops_before = probes
        .completion_drops
        .load(std::sync::atomic::Ordering::Relaxed);
    let panics_before = probes
        .collector_panics
        .load(std::sync::atomic::Ordering::Relaxed);
    let start_ns = composition.clock_now();
    probes.begin_window(start_ns, composition.snapshots.latest().revision);
    router.begin_window();
    let sampler = Sampler::start(SamplerInputs {
        supervisor: composition.supervisor.clone(),
        ledger: composition.ledger.clone(),
        shadow_ledger: Some(shadow.ledger.clone()),
        harness: harness.clone(),
        interval: SAMPLE_INTERVAL,
    })?;
    let started = Instant::now();
    let mut manager_died = false;
    while started.elapsed() < plan.sizes.window {
        std::thread::sleep((plan.sizes.window - started.elapsed()).min(Duration::from_millis(500)));
        manager_died |= !composition.manager_alive();
    }
    let sampled = sampler.finish();
    let end_ns = composition.clock_now();
    let wall_seconds = started.elapsed().as_secs_f64();
    let thread_usage = usage_by_thread(
        &sampled.threads_at_start,
        &sampled.threads_at_end,
        plan.clock_ticks_per_second,
        wall_seconds,
    );
    let (projection, history) = probes.end_window();
    let (commands_us, command_statuses, router_evicted) = router.end_window();
    let statuses_after = composition.supervisor.statuses();
    let diagnostics_after = composition.diagnostics.snapshot();
    let completion_drops = probes
        .completion_drops
        .load(std::sync::atomic::Ordering::Relaxed)
        .saturating_sub(drops_before);
    let collector_panics = probes
        .collector_panics
        .load(std::sync::atomic::Ordering::Relaxed)
        .saturating_sub(panics_before);
    let host_restarts = restarts(composition).saturating_sub(restarts_before);
    // Last: waits for the commits of reads that finished inside the window.
    let coverage =
        probes.final_coverage(&plan.groups, start_ns, end_ns, || composition.clock_now());
    Ok(MeasuredWindow {
        data: WindowData {
            wall_seconds,
            resources: sampled.samples,
            clock_ticks_per_second: plan.clock_ticks_per_second,
            commands_us,
            frames_us: Vec::new(),
            projection,
            history,
            coverage,
            diagnostics_before,
            diagnostics_after,
            completion_drops,
            collector_panics,
            host_restarts,
            manager_died,
            frontend_failed: false,
            stalls: 0,
        },
        command_statuses,
        router_evicted,
        statuses_after,
        audit_sqlite_ok: composition.sqlite_runtime_ok,
        thread_usage,
    })
}

fn restarts(composition: &RefComposition) -> u64 {
    composition
        .supervisor
        .statuses()
        .iter()
        .map(|status| status.restarts_total)
        .fold(0, u64::saturating_add)
}

type PreloadFacts<'a> = (&'a dgxtop::perf::history_load::PreloadReport, f64, u64);

fn run_details(
    window: &MeasuredWindow,
    frontend: &FrontendReport,
    script: &ScriptReport,
    (preload, preload_seconds, feeder_appends): PreloadFacts<'_>,
    ready_after_seconds: f64,
    plan: &RunPlan,
) -> Value {
    let data = &window.data;
    let diagnostics =
        |metric| counter_delta(&data.diagnostics_before, &data.diagnostics_after, metric);
    json!({
        "ready_after_seconds": ready_after_seconds,
        "audit_sqlite_runtime_ok": window.audit_sqlite_ok,
        "window_seconds": data.wall_seconds,
        "cpu": cpu_breakdown(&data.resources, plan.clock_ticks_per_second),
        "by_process": usage_by_process(&data.resources, plan.clock_ticks_per_second),
        "parent_by_thread": window.thread_usage,
        "rss_complete_samples": data.resources.iter().filter(|sample| sample.complete).count(),
        "resource_samples": data.resources.len(),
        "latency_summaries": {
            "commands": summarize_us(&data.commands_us),
            "frames": summarize_us(&frontend.frame_us),
            "projection": summarize_us(&data.projection.latencies_us),
            "history": summarize_us(&data.history.latencies_us),
        },
        "commands": {"statuses": window.command_statuses, "evicted_unanswered": window.router_evicted, "script": script},
        "frontend": frontend,
        "projection": {
            "unresolved_revisions": data.projection.unresolved_revisions,
            "evicted_revisions": data.projection.evicted_revisions,
            "clock_skew": data.projection.clock_skew,
            "committed_batches": data.projection.committed_batches,
            "rejected_batches": data.projection.rejected_batches,
            "committed_reads_by_group_and_status": data.projection.read_census,
            "manager_commit_to_projection_histogram": data.diagnostics_after.histograms.get(names::COMMIT_TO_PROJECTION_MS),
        },
        "history": {
            "outcomes": data.history.outcomes,
            "max_points": data.history.max_points,
            "unanswered_at_end": data.history.unanswered,
            "preload": preload,
            "preload_wall_seconds": preload_seconds,
            "feeder_appends": feeder_appends,
        },
        "coverage": data.coverage,
        "drops": {
            "unexplained_total": unexplained_drops(data),
            "queue_dropped_by_label": label_deltas(data, names::QUEUE_DROPPED_TOTAL),
            "dispatch_deferred_by_reason": label_deltas(data, names::ADAPTER_DISPATCH_DEFERRED_TOTAL),
            "completion_send_failures": data.completion_drops,
            "stale_results_dropped": diagnostics(names::STALE_RESULT_DROPPED_TOTAL),
            "gaps_recorded": diagnostics(names::GAP_RECORDED_TOTAL),
            "adapter_timeouts": diagnostics(names::ADAPTER_TIMEOUT_TOTAL),
        },
        "overload_transitions": label_deltas(data, names::OVERLOAD_TRANSITIONS_TOTAL),
        "crashes": {
            "host_restarts": data.host_restarts,
            "collector_panics": data.collector_panics,
            "manager_died": data.manager_died,
            "frontend_failed": data.frontend_failed,
        },
        "stalls": data.stalls,
        "hosts_after": window.statuses_after,
    })
}

fn label_deltas(data: &WindowData, metric: &str) -> std::collections::BTreeMap<String, u64> {
    let before = data.diagnostics_before.counters.get(metric);
    data.diagnostics_after
        .counters
        .get(metric)
        .map(|series| {
            series
                .iter()
                .map(|(label, value)| {
                    let earlier = before
                        .and_then(|series| series.get(label))
                        .copied()
                        .unwrap_or(0);
                    (label.clone(), value.saturating_sub(earlier))
                })
                .filter(|(_, delta)| *delta > 0)
                .collect()
        })
        .unwrap_or_default()
}

fn write_fixture_evidence(
    evidence: &mut EvidenceWriter,
    fixture: &GeneratedFixture,
    plan: &RunPlan,
) -> BenchResult<()> {
    evidence
        .write_json(
            "fixture.json",
            &json!({
                "seed": fixture.seed,
                "sha256": fixture.sha256,
                "bytes": fixture.bytes,
                "census": fixture.census,
                "covers_reference_classes": fixture.census.covers_reference_classes(),
                "adapter_hosts": plan.adapter_hosts,
                "scheduled_groups": plan.groups,
                "regenerate": "dgxtop::perf::fixture::write_fixture(seed, REF profile, simulated_latency_us = 1000)",
            }),
        )
        .map_err(|error| format!("evidence: {error}"))
}

fn write_run_evidence(
    evidence: &mut EvidenceWriter,
    index: usize,
    result: &RunResult,
) -> BenchResult<()> {
    for stream in &result.raw {
        common::write_samples(
            evidence,
            &format!("run{index}/{}.json", stream.name),
            &RawSamples {
                stream: stream.name,
                unit: "us",
                definition: stream.definition,
                samples: &stream.samples,
            },
        )?;
    }
    evidence
        .write_json(&format!("run{index}/resources.json"), &result.resources)
        .map_err(|error| format!("evidence: {error}"))?;
    evidence
        .write_json(&format!("run{index}/details.json"), &result.details)
        .map_err(|error| format!("evidence: {error}"))
}
