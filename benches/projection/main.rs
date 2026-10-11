//! Projection micro benchmark: the Manager's PresentationService builders
//! (`build_view_model_measured`) over a store snapshot of the seeded REF
//! workload, and the Viewer turning each immutable projection into a frame
//! through the real terminal runner (`TuiLoop`) at the REF 160×50 size.
//!
//! * projection build: snapshot + catalog + session → `ViewModel` (with its
//!   byte estimate), for all seven views, the snapshot moving one sampling
//!   cycle before every frame;
//! * frame: the runner step that accepts the projection and draws it —
//!   layout, module render, Ratatui diff and ANSI encoding into memory; the
//!   terminal transport is a null sink (`PERFORMANCE.md`: transport excluded).
//!
//! Budget mapping: frames are `frame_p95_ms` / `frame_p99_ms`. Projection
//! building is one stage of `projection_p95_ms` and is only compared with
//! it as a necessary condition. Publishing (permit + slot swap) is internal
//! to the Manager and covered end to end by the `ref` bench.

#[path = "../common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use common::headless::{IdleGate, ManualLoopClock, NullTransport, isolate_terminal_input};
use common::{BenchOutcome, BenchResult, RawSamples};
use dgxtop::live_store::LiveStore;
use dgxtop::perf::evidence::EvidenceWriter;
use dgxtop::perf::fixture::FixtureCensus;
use dgxtop::perf::latency::{LatencySummary, duration_us, summarize_us};
use dgxtop::perf::pipeline::ReplayPipeline;
use dgxtop::perf::report::{BenchMode, RunRecord};
use dgxtop::startup::value_origins;
use dgxtop_adapters::replay::generate_ref_fixture;
use dgxtop_core::action::{PersistenceKind, PersistenceStatus};
use dgxtop_core::budget::ByteLedger;
use dgxtop_core::canonical::{CommitGuard, ImmutableSnapshot};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::commands::{CommandEnvelope, CommandRejection, CommandResult, SortKey, ViewKind};
use dgxtop_core::config::{Config, ValueOrigin};
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::identity::{EntityId, MetricKey, SessionId};
use dgxtop_core::metric::{CanonicalUnit, Scalar, descriptor, ids};
use dgxtop_core::naming::entity;
use dgxtop_core::observation::EntityKind;
use dgxtop_core::ports::{
    LoadOrigin, ProcessHistoryInput, ProcessHistoryReader, SessionPort, SnapshotReader, StoreWriter,
};
use dgxtop_core::process_model::{ProcessHistoryModel, ProcessTableSample};
use dgxtop_core::quality::Namespace;
use dgxtop_core::view::{
    ActionsView, DiskHistory, HistoryChartKind, HistoryChartView, HistoryPointView, HistoryView,
    HostState, HostStatus, IntervalView, OverloadState, SessionState, ViewModel,
};
use dgxtop_history::{
    ProcessHistory, ProcessHistoryPorts, ProcessHistorySettings, PublishedProcessHistory,
};
use dgxtop_manager::presentation::{
    HostFacts, PresentationContext, SettingsInputs, TrendBook, build_view_model_measured,
};
use dgxtop_state::{SharedSnapshot, StateWriter};
use dgxtop_ui::terminal::GatedInput;
use dgxtop_ui::{TuiLoop, TuiOptions};
use serde::Serialize;
use serde_json::json;

const BENCH: &str = "projection";
const RUNS: usize = 3;
const START_BOOT_NS: u64 = 1_000_000_000_000;
/// The Manager's sparkline depth (4 min at 1 s).
const TREND_POINTS: usize = 240;
/// Loop-clock step per frame: above the 4 fps frame interval.
const FRAME_STEP: Duration = Duration::from_millis(300);
/// Runner steps allowed to flush one frame before giving up.
const MAX_FLUSH_STEPS: usize = 4096;

struct Sizes {
    warmup_cycles: u32,
    frames_per_view: u32,
}

const fn sizes(mode: BenchMode) -> Sizes {
    match mode {
        BenchMode::Smoke => Sizes {
            warmup_cycles: 10,
            frames_per_view: 15,
        },
        BenchMode::Full => Sizes {
            warmup_cycles: 60,
            frames_per_view: 150,
        },
    }
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
    let _isolation = isolate_terminal_input()?;
    let environment = common::environment(
        "headless 160x50 TuiLoop, ANSI encoded into memory, null transport",
        "single-threaded; projection build and frame step timed separately",
    )?;
    let viewport = (spec.profile.terminal_columns, spec.profile.terminal_rows);
    let profile = spec.profile.replay_profile(0);
    let fixture = generate_ref_fixture(spec.seed, &profile);
    let census = FixtureCensus::of(&fixture);
    let mut world = SnapshotWorld::new(&fixture, profile.adapter_hosts, &config)?;
    drop(fixture);
    for _ in 0..sizes.warmup_cycles {
        world.advance();
    }
    let statics = StaticInputs::new(&config, &world)?;
    let mut rig = FrameRig::new(viewport)?;
    let mut evidence = common::open_evidence(&paths)?;
    let mut frame_revision = 0u64;
    let mut runs = Vec::new();
    let mut run_details = Vec::new();
    for index in 1..=RUNS {
        let started = Instant::now();
        let mut samples = RunSamples::default();
        for view in ViewKind::ALL {
            for _ in 0..sizes.frames_per_view {
                world.advance();
                frame_revision += 1;
                let session = statics.session_for(view);
                let snapshot = world.snapshots.latest();
                let context =
                    statics.context(&world, &snapshot, &session, viewport, frame_revision);
                let build_started = Instant::now();
                let (model, model_bytes) = build_view_model_measured(&context);
                let build_us = duration_us(build_started.elapsed());
                let frame = rig.render(model)?;
                samples.record(view, build_us, frame, model_bytes);
            }
        }
        let duration = started.elapsed().as_secs_f64();
        write_run_evidence(&mut evidence, index, &samples)?;
        let mut record = RunRecord {
            duration_seconds: duration,
            frames: u64::try_from(samples.frame_us.len()).unwrap_or(u64::MAX),
            ..RunRecord::default()
        };
        if let Some(summary) = summarize_us(&samples.frame_us) {
            record.set("frame_p95_ms", summary.p95_ms, summary.samples)?;
            record.set("frame_p99_ms", summary.p99_ms, summary.samples)?;
        }
        run_details.push(RunDetail::from(&samples));
        runs.push(record);
    }
    let worst_build_p95 = run_details
        .iter()
        .filter_map(|detail| detail.build.as_ref().map(|summary| summary.p95_ms))
        .fold(f64::NAN, f64::max);
    let projection_budget = spec
        .budgets
        .limits
        .get("projection_p95_ms")
        .and_then(|bound| bound.max);
    let outcome = BenchOutcome {
        bench: BENCH,
        mode: args.mode,
        runs,
        soak: None,
        details: json!({
            "fixture": {"seed": spec.seed, "census": census, "simulated_latency_us": 0},
            "viewport": viewport,
            "warmup_cycles": sizes.warmup_cycles,
            "frames_per_view_per_run": sizes.frames_per_view,
            "store_rejections": world.rejections,
            "runs": run_details,
            "component_check": {
                "stage": "build_view_model_measured for one session",
                "end_to_end_budget": "projection_p95_ms",
                "end_to_end_max_ms": projection_budget,
                "worst_run_build_p95_ms": worst_build_p95,
                "necessary_condition_holds": projection_budget.map(|max| worst_build_p95 < max),
            },
            "history_view_input": "the HistoryView handed to the builder holds synthetic 161-point charts of the fixture's CPU, memory, GPUs, one disk and one interface (the history data path is measured by the history bench)",
        }),
        extra_blockers: vec![
            "micro benchmark: measures projection build and frame compute only; other REF metrics are not measured here".to_owned(),
        ],
        notes: vec![
            "frame = TuiLoop step that accepted and drew the projection (includes the first <= 8 KiB copy to the null transport)".to_owned(),
            format!("worst projection build p95 {worst_build_p95:.3} ms (component of projection_p95_ms)"),
        ],
    };
    common::finish(outcome, &spec, &config, environment, &mut evidence, &paths)
}

/// The replay pipeline committing into a real (replay-namespace) hot store,
/// plus the Manager-equivalent sparkline feed and process memory record.
struct SnapshotWorld {
    pipeline: ReplayPipeline,
    writer: StateWriter,
    snapshots: SharedSnapshot,
    trends: TrendFeed,
    /// The history's process actor, fed each cycle's table (D-79).
    processes: ProcessHistory,
    published: PublishedProcessHistory,
    /// What it published after the latest cycle.
    model: Arc<ProcessHistoryModel>,
    process_period_ms: u64,
    rejections: BTreeMap<String, u64>,
}

impl SnapshotWorld {
    fn new(
        fixture: &dgxtop_adapters::replay::ReplayFixture,
        hosts: u32,
        config: &Config,
    ) -> BenchResult<Self> {
        let pipeline = ReplayPipeline::new(fixture, hosts, config, START_BOOT_NS)
            .map_err(|error| error.to_string())?;
        let (writer, snapshots, _history) = LiveStore::new(
            &config.store,
            &config.sampling,
            ByteLedger::for_memory_limit_mib(u64::from(config.store.memory_limit_mib)),
            Namespace::Replay,
            false,
        )
        .into_parts();
        let (processes, published) = ProcessHistory::new(
            ProcessHistorySettings {
                boot_id: dgxtop_core::testing::boot_id("bench"),
                utc_offset_s: 0,
            },
            ProcessHistoryPorts {
                probe: None,
                probe_deriver: Arc::new(dgxtop_core::testing::IdleProbeDeriver),
                store: None,
                clock: Arc::clone(pipeline.clock()) as Arc<dyn TimeProvider>,
                logger: Arc::new(dgxtop_core::ports::NullLogger),
            },
        );
        Ok(Self {
            pipeline,
            writer,
            snapshots,
            trends: TrendFeed::new(),
            processes,
            model: published.latest(),
            published,
            process_period_ms: u64::from(config.sampling.interval_ms),
            rejections: BTreeMap::new(),
        })
    }

    /// One sampling cycle: read, collect, commit each batch, maintain.
    fn advance(&mut self) {
        let batches = self.pipeline.read_next_cycle();
        let now = self.pipeline.clock().now_boot_ns();
        for (collector, plan) in self.pipeline.plans(&batches) {
            let batch = collector.collect(&plan);
            let next_state = batch.next_state.clone();
            let guard = CommitGuard {
                expected_epoch: plan.epoch,
                collector_id: plan.collector_id.clone(),
                state_base_revision: plan.state_revision,
            };
            let ack = self.writer.commit_collection(batch, &guard, now);
            if ack.status.is_committed() {
                self.pipeline.adopt_state(
                    plan.collector_id.clone(),
                    next_state,
                    ack.state_revision,
                );
            } else {
                let reason = format!(
                    "{}: {}",
                    plan.collector_id,
                    ack.reason.unwrap_or_else(|| format!("{:?}", ack.status))
                );
                *self.rejections.entry(reason).or_insert(0) += 1;
            }
        }
        // As the monitor's maintenance step: the pairing fixes the minute
        // grid, so minutes are finalized and raw history is summarized.
        let clock = self.pipeline.clock();
        self.writer
            .observe_wall_clock(clock.now_boot_ns(), clock.now_utc_ns());
        self.writer.maintain(now);
        let snapshot = self.snapshots.latest();
        self.processes
            .handle(ProcessHistoryInput::Table(Box::new(ProcessTableSample {
                table: Arc::clone(&snapshot.process_table),
                clk_tck: None,
                gpu_labels: BTreeMap::new(),
                host_network: [None, None],
                process_period_ms: self.process_period_ms,
            })));
        self.model = self.published.latest();
        self.trends.record(&snapshot);
    }
}

/// Mirror of the Manager's sparkline recorder: tracked series only, one
/// point per new read, gaps kept as `None`.
struct TrendFeed {
    book: TrendBook,
    last_read: BTreeMap<MetricKey, u64>,
}

impl TrendFeed {
    fn new() -> Self {
        Self {
            book: TrendBook::new(TREND_POINTS),
            last_read: BTreeMap::new(),
        }
    }

    fn record(&mut self, snapshot: &ImmutableSnapshot) {
        for domain in snapshot.domains.values() {
            for (key, entry) in &domain.samples {
                if !is_trend_key(key)
                    || self.last_read.get(key) == Some(&entry.sample.read_finished_boot_ns)
                {
                    continue;
                }
                self.last_read
                    .insert(key.clone(), entry.sample.read_finished_boot_ns);
                let value = entry
                    .sample
                    .selection_state
                    .has_value()
                    .then_some(entry.sample.value)
                    .flatten()
                    .map(Scalar::to_f64_lossy);
                self.book.push(key, value);
            }
        }
    }
}

fn is_trend_key(key: &MetricKey) -> bool {
    match key.metric_id.as_str() {
        ids::CPU_UTILIZATION_TOTAL => key.scope.as_str() == entity::SCOPE_HOST,
        ids::MEMORY_USED => true,
        ids::GPU_UTILIZATION_COMPUTE | ids::GPU_TEMPERATURE | ids::GPU_POWER_DRAW => {
            entity::parse_gpu(key.entity_id.as_str()).is_some()
        }
        _ => false,
    }
}

/// Inputs that do not change between frames.
struct StaticInputs {
    config: Config,
    origins: BTreeMap<String, ValueOrigin>,
    config_status: PersistenceStatus,
    hosts: Vec<HostStatus>,
    intervals: Vec<IntervalView>,
    actions: Arc<ActionsView>,
    history: Arc<HistoryView>,
    facts: HostFacts,
    first_gpu: Option<EntityId>,
    session_id: SessionId,
}

impl StaticInputs {
    fn new(config: &Config, world: &SnapshotWorld) -> BenchResult<Self> {
        let catalog = world.pipeline.catalog();
        let adapters: Vec<String> = catalog
            .sources
            .values()
            .map(|source| source.adapter_id.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let hosts = adapters
            .iter()
            .map(|adapter_id| HostStatus {
                adapter_id: adapter_id.clone(),
                state: HostState::Healthy,
                generation: 1,
                pid: None,
                last_success_boot_ns: Some(START_BOOT_NS),
                timeouts_total: 0,
                consecutive_failures: 0,
                restarts_total: 0,
                backoff_until_boot_ns: None,
                detail: None,
            })
            .collect();
        let intervals = catalog
            .sources
            .values()
            .map(|source| {
                (
                    source.adapter_id.to_string(),
                    source.read_group.as_str().to_owned(),
                )
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|(adapter_id, read_group)| {
                let period =
                    dgxtop::perf::coverage::replay_group_period_ms(&read_group, &config.sampling);
                IntervalView {
                    adapter_id,
                    read_group,
                    configured_ms: period,
                    effective_ms: period,
                    reason: "configured".to_owned(),
                }
            })
            .collect();
        let snapshot = world.snapshots.latest();
        Ok(Self {
            origins: value_origins(config, &LoadOrigin::Defaults, &[]),
            config_status: PersistenceStatus::healthy(PersistenceKind::Config),
            hosts,
            intervals,
            actions: Arc::new(ActionsView {
                confirmation: None,
                audit: PersistenceStatus::healthy(PersistenceKind::Audit),
                unknown_outcomes: 0,
            }),
            history: Arc::new(history_view(
                &snapshot,
                world.pipeline.clock().now_boot_ns(),
            )),
            facts: HostFacts {
                host_name: world.pipeline.host_id().to_string(),
                kernel: Some("synthetic".to_owned()),
                cpu_model: Some("synthetic REF host".to_owned()),
                version: dgxtop::version::BuildInfo::current().version.to_owned(),
                namespace_label: Some("replay (simulated data)".to_owned()),
                utc_offset_s: 0,
            },
            first_gpu: catalog
                .entities
                .values()
                .find(|entity| entity.kind == EntityKind::Gpu)
                .map(|entity| entity.entity_id.clone()),
            session_id: SessionId::new("bench-session").map_err(|error| error.to_string())?,
            config: config.clone(),
        })
    }

    fn session_for(&self, view: ViewKind) -> SessionState {
        SessionState {
            session_id: self.session_id.clone(),
            view,
            selected_entity_id: (view == ViewKind::GpuDetail)
                .then(|| self.first_gpu.clone())
                .flatten(),
            selected_process: None,
            filter: if view == ViewKind::History {
                "1h".to_owned()
            } else {
                String::new()
            },
            sort: SortKey::GpuMemory,
            ascending: false,
            overview_bar: dgxtop_core::tiers::BarSize::OneMinute,
            process_bar: dgxtop_core::tiers::BarSize::Auto,
        }
    }

    fn context<'a>(
        &'a self,
        world: &'a SnapshotWorld,
        snapshot: &'a ImmutableSnapshot,
        session: &'a SessionState,
        viewport: (u16, u16),
        frame_revision: u64,
    ) -> PresentationContext<'a> {
        PresentationContext {
            frame_revision,
            now_boot_ns: world.pipeline.clock().now_boot_ns(),
            snapshot,
            catalog: world.pipeline.catalog(),
            session,
            viewport,
            settings: SettingsInputs {
                saved: &self.config,
                effective: &self.config,
                origins: &self.origins,
                restart_pending: &[],
                pending: None,
                last_result: None,
                status: &self.config_status,
            },
            hosts: &self.hosts,
            intervals: &self.intervals,
            overload: OverloadState::Normal,
            actions: Arc::clone(&self.actions),
            history: Arc::clone(&self.history),
            trends: &world.trends.book,
            processes: &world.model,
            process_memory: &world.published,
            facts: &self.facts,
            enabled_modules: &self.config.viewer.enabled_modules,
            archive: None,
        }
    }
}

/// A 1 h history as the HistoryService would hand it over for the fixture
/// (D-46): charts of the CPU, the memory, every GPU's utilization and the
/// rates of one disk and one interface, 161 points each with two gaps.
#[allow(clippy::cast_precision_loss)]
fn history_view(snapshot: &ImmutableSnapshot, now_ns: u64) -> HistoryView {
    const SPAN_NS: u64 = 3_600_000_000_000;
    const POINTS: u64 = 161;
    const MAX_GPU_CHARTS: usize = 18;
    let step = SPAN_NS / POINTS;
    let start = now_ns.saturating_sub(SPAN_NS);
    let points: Vec<HistoryPointView> = (0..POINTS)
        .map(|index| {
            let gap = (40..44).contains(&index) || (120..122).contains(&index);
            let mean = 40.0 + 30.0 * ((index as f64) / 20.0).sin();
            HistoryPointView {
                start_utc_ns: start + index * step,
                end_utc_ns: start + (index + 1) * step,
                mean: (!gap).then_some(mean),
                min: None,
                max: (!gap).then_some(mean + 5.0),
            }
        })
        .collect();
    let keys_of = |metric: &str| -> Vec<MetricKey> {
        snapshot
            .domains
            .values()
            .flat_map(|domain| domain.samples.keys())
            .filter(|key| key.metric_id.as_str() == metric)
            .cloned()
            .collect()
    };
    let charted = [
        (
            HistoryChartKind::CpuUtilization,
            ids::CPU_UTILIZATION_TOTAL,
            1,
            Some(100.0),
        ),
        (
            HistoryChartKind::MemoryUsed,
            ids::MEMORY_USED,
            1,
            Some(68_719_476_736.0),
        ),
        (
            HistoryChartKind::GpuUtilization,
            ids::GPU_UTILIZATION_COMPUTE,
            MAX_GPU_CHARTS,
            Some(100.0),
        ),
        (HistoryChartKind::DiskRead, ids::DISK_READ_BPS, 1, None),
        (HistoryChartKind::DiskWrite, ids::DISK_WRITE_BPS, 1, None),
        (HistoryChartKind::NetworkReceive, ids::NET_RX_BPS, 1, None),
        (HistoryChartKind::NetworkTransmit, ids::NET_TX_BPS, 1, None),
    ];
    let mut charts = Vec::new();
    for (kind, metric, limit, scale_max) in charted {
        for key in keys_of(metric).into_iter().take(limit) {
            charts.push(HistoryChartView {
                kind,
                subject: key.entity_id.to_string(),
                unit: descriptor(metric).map_or(CanonicalUnit::Count, |known| known.unit),
                series: key,
                scale_max,
                points: points.clone(),
                mean: Some(41.5),
                peak: Some(75.0),
                coverage_ratio: Some(0.97),
                covered_ns: Some(3_492_000_000_000),
                display_downsampled: true,
                pending: false,
                error: None,
                parts: Vec::new(),
            });
        }
    }
    HistoryView {
        window_label: "1h".to_owned(),
        requested_span_ns: SPAN_NS,
        start_utc_ns: start,
        end_utc_ns: now_ns,
        utc_offset_s: 0,
        charts,
        summaries: Vec::new(),
        disk: DiskHistory::Off,
        error: None,
    }
}

/// One rendered frame.
#[derive(Debug, Clone, Copy)]
struct FrameSample {
    step_us: u64,
    output_bytes: u64,
}

/// Session that hands the runner exactly the projection it was given.
struct FrameSession {
    session_id: SessionId,
    slot: Arc<Mutex<Option<Arc<ViewModel>>>>,
}

impl SessionPort for FrameSession {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    fn submit(&self, command: CommandEnvelope) -> Result<(), CommandRejection> {
        Err(CommandRejection {
            request_id: command.request_id,
            code: ErrorCode::CapabilityDenied,
            message: "the projection bench takes no commands".to_owned(),
        })
    }

    fn take_projection(&self) -> Option<Arc<ViewModel>> {
        self.slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn drain_results(&self, _max: usize) -> Vec<CommandResult> {
        Vec::new()
    }

    fn request_shutdown(&self, _reason: &str) {}

    fn shutdown_requested(&self) -> bool {
        false
    }

    fn set_viewport(&self, _columns: u16, _rows: u16) {}
}

/// The real terminal runner behind a null transport and a manual clock.
struct FrameRig {
    tui: TuiLoop,
    slot: Arc<Mutex<Option<Arc<ViewModel>>>>,
    clock_ns: Arc<AtomicU64>,
}

impl FrameRig {
    fn new(viewport: (u16, u16)) -> BenchResult<Self> {
        let slot = Arc::new(Mutex::new(None));
        let clock_ns = Arc::new(AtomicU64::new(0));
        let session = FrameSession {
            session_id: SessionId::new("bench-session").map_err(|error| error.to_string())?,
            slot: Arc::clone(&slot),
        };
        let tui = TuiLoop::new(
            Box::new(session),
            Box::new(NullTransport::new(Arc::new(AtomicU64::new(0)))),
            Box::new(GatedInput::crossterm(Box::new(IdleGate::new(
                false,
                Arc::new(AtomicU64::new(0)),
            )))),
            Box::new(ManualLoopClock::new(Arc::clone(&clock_ns))),
            TuiOptions {
                mouse: false,
                ..TuiOptions::default()
            },
            viewport,
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            tui,
            slot,
            clock_ns,
        })
    }

    /// Hands `model` to the runner, times the step that draws it, then lets
    /// the runner flush the frame (untimed) so the next one can be accepted.
    fn render(&mut self, model: ViewModel) -> BenchResult<FrameSample> {
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(model));
        self.clock_ns.fetch_add(
            u64::try_from(FRAME_STEP.as_nanos()).unwrap_or(u64::MAX),
            Ordering::AcqRel,
        );
        let drawn_before = self.tui.stats().frames_drawn;
        let bytes_before = self.tui.stats().bytes_written;
        let started = Instant::now();
        self.step()?;
        let step_us = duration_us(started.elapsed());
        if self.tui.stats().frames_drawn <= drawn_before {
            return Err("the runner did not draw the projection it was given".to_owned());
        }
        for _ in 0..MAX_FLUSH_STEPS {
            let written = self.tui.stats().bytes_written;
            self.step()?;
            if self.tui.stats().bytes_written == written {
                break;
            }
        }
        Ok(FrameSample {
            step_us,
            output_bytes: self.tui.stats().bytes_written.saturating_sub(bytes_before),
        })
    }

    fn step(&mut self) -> BenchResult<()> {
        match self.tui.step() {
            Ok(None) => Ok(()),
            Ok(Some(reason)) => Err(format!("terminal runner stopped: {reason:?}")),
            Err(error) => Err(format!("terminal runner failed: {error}")),
        }
    }
}

#[derive(Default)]
struct RunSamples {
    build_us: Vec<u64>,
    frame_us: Vec<u64>,
    per_view: BTreeMap<String, (Vec<u64>, Vec<u64>)>,
    model_bytes: Vec<u64>,
    output_bytes: Vec<u64>,
}

impl RunSamples {
    fn record(&mut self, view: ViewKind, build_us: u64, frame: FrameSample, model_bytes: usize) {
        self.build_us.push(build_us);
        self.frame_us.push(frame.step_us);
        self.model_bytes
            .push(u64::try_from(model_bytes).unwrap_or(u64::MAX));
        self.output_bytes.push(frame.output_bytes);
        let entry = self.per_view.entry(format!("{view:?}")).or_default();
        entry.0.push(build_us);
        entry.1.push(frame.step_us);
    }
}

#[derive(Serialize)]
struct ViewDetail {
    build: Option<LatencySummary>,
    frame: Option<LatencySummary>,
}

#[derive(Serialize)]
struct RunDetail {
    build: Option<LatencySummary>,
    frame: Option<LatencySummary>,
    views: BTreeMap<String, ViewDetail>,
    max_model_bytes: u64,
    max_frame_output_bytes: u64,
}

impl From<&RunSamples> for RunDetail {
    fn from(samples: &RunSamples) -> Self {
        Self {
            build: summarize_us(&samples.build_us),
            frame: summarize_us(&samples.frame_us),
            views: samples
                .per_view
                .iter()
                .map(|(view, (build, frame))| {
                    (
                        view.clone(),
                        ViewDetail {
                            build: summarize_us(build),
                            frame: summarize_us(frame),
                        },
                    )
                })
                .collect(),
            max_model_bytes: samples.model_bytes.iter().copied().max().unwrap_or(0),
            max_frame_output_bytes: samples.output_bytes.iter().copied().max().unwrap_or(0),
        }
    }
}

fn write_run_evidence(
    evidence: &mut EvidenceWriter,
    index: usize,
    samples: &RunSamples,
) -> BenchResult<()> {
    common::write_samples(
        evidence,
        &format!("run{index}/projection_build_us.json"),
        &RawSamples {
            stream: "projection_build",
            unit: "us",
            definition: "build_view_model_measured for one session (all seven views in turn)",
            samples: &samples.build_us,
        },
    )?;
    common::write_samples(
        evidence,
        &format!("run{index}/frame_us.json"),
        &RawSamples {
            stream: "frame",
            unit: "us",
            definition: "TuiLoop step accepting and drawing one projection at 160x50 (render + diff + ANSI encode; null transport)",
            samples: &samples.frame_us,
        },
    )
}
