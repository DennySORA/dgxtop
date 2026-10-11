//! The REF composition: the same components and wiring as the monitor's
//! composition root (`crates/dgxtop/src/monitor.rs`) in the replay
//! namespace — HotStore actor, collector pool, history pool, config and
//! audit (SQLite) actors, the adapter supervisor with seven *child-process*
//! replay hosts (`dgxtop --adapter-host replay-N`, one fixture slice each)
//! and the Manager on its own thread — minus the terminal.
//!
//! Differences from the monitor, all on the edges:
//! * probes observe commits (projection latency, coverage) and history jobs
//!   (submit → result) after the real work;
//! * history queries go through a switchable reader so the bench can serve
//!   them from a preloaded 24 h store (replay data is never history-eligible);
//! * process actions use a port that refuses everything, so no signal can
//!   ever reach a process the bench did not spawn (the workload only sends
//!   view commands anyway).

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded};
use dgxtop::audit_unavailable::UnavailableAudit;
use dgxtop::control_ops::{run_action_op, run_audit_op};
use dgxtop::live_store::LiveStore;
use dgxtop::startup::{real_filesystem, value_origins};
use dgxtop::version::BuildInfo;
use dgxtop_collectors::registry::default_collectors;
use dgxtop_core::action::{
    ActionError, ActionHandle, ActionOp, AuditOp, CapabilityContext, DurableReceipt,
    ExitObservation, PreparedProcess, Signal, SignalOutcome,
};
use dgxtop_core::budget::{ByteLedger, Partition, QueueGate};
use dgxtop_core::canonical::CollectionPlan;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::config::Config;
use dgxtop_core::diagnostics::Diagnostics;
use dgxtop_core::errors::ErrorDetail;
use dgxtop_core::history::{HistoryQuery, HistoryResult};
use dgxtop_core::identity::{AdapterId, MetricKey, ProcessKey};
use dgxtop_core::ports::{
    AuditRepository, CancelFlag, Collector, ConfigJob, ConfigRepository, ControlCompletion,
    Dispatch, DispatchError, HistoryJob, HistoryReader, IngestionCompletion, LogLevel, Logger,
    ProcessActionPort, StoreJob, StoreWriter,
};
use dgxtop_core::quality::Namespace;
use dgxtop_manager::presentation::HostFacts;
use dgxtop_manager::{
    Manager, ManagerChannels, ManagerPorts, ManagerSetup, SessionHandle, ShutdownSummary,
    ShutdownTrigger,
};
use dgxtop_runtime::clock::{
    BootClock, current_euid, host_identity, kernel_release, pid_namespace_of,
};
use dgxtop_runtime::logger::FileLogger;
use dgxtop_runtime::queues::{PoolHandle, spawn_actor, spawn_pool};
use dgxtop_runtime::supervisor::{
    HostLaunch, HostSpec, Supervisor, SupervisorConfig, SupervisorHandle,
};
use dgxtop_state::{SharedSnapshot, StateWriter};
use dgxtop_store::audit::{AuditLimits, SqliteAuditRepository};
use dgxtop_store::config::FileConfigRepository;
use dgxtop_store::paths::{ProcessEnv, StatePaths};
use dgxtop_store::sqlite::{StatvfsFreeSpace, ensure_sqlite_version};

use super::probes::{Probes, reads_of};
use crate::common::BenchResult;

/// Bounded wait for a worker to hand a completion to the Manager (as the monitor).
const COMPLETION_SEND_TIMEOUT: Duration = Duration::from_secs(1);
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(3);

/// History reader the bench can swap once the preloaded store is ready.
pub struct SwitchableHistory {
    current: RwLock<Arc<dyn HistoryReader>>,
}

impl SwitchableHistory {
    pub fn new(initial: Arc<dyn HistoryReader>) -> Self {
        Self {
            current: RwLock::new(initial),
        }
    }

    pub fn switch_to(&self, reader: Arc<dyn HistoryReader>) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = reader;
    }
}

impl HistoryReader for SwitchableHistory {
    fn query(
        &self,
        query: &HistoryQuery,
        deadline_boot_ns: u64,
        cancel: &CancelFlag,
        clock: &dyn TimeProvider,
    ) -> Result<HistoryResult, ErrorDetail> {
        let reader = Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner));
        reader.query(query, deadline_boot_ns, cancel, clock)
    }

    fn series(&self) -> Vec<MetricKey> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .series()
    }
}

/// Inputs of one composition.
pub struct CompositionInputs<'a> {
    pub run_dir: &'a Path,
    /// One fixture slice per replay host (one live adapter family each).
    pub host_fixtures: &'a [PathBuf],
    pub probes: Arc<Probes>,
}

/// A running REF composition.
pub struct RefComposition {
    pub session: SessionHandle,
    pub supervisor: SupervisorHandle,
    pub ledger: ByteLedger,
    pub diagnostics: Diagnostics,
    pub clock: Arc<dyn TimeProvider>,
    pub snapshots: SharedSnapshot,
    pub config: Config,
    pub adapter_ids: Vec<String>,
    pub sqlite_runtime_ok: bool,
    pub history: Arc<SwitchableHistory>,
    trigger: ShutdownTrigger,
    manager_thread: Option<JoinHandle<ShutdownSummary>>,
    workers: Vec<Box<dyn StopWorker>>,
    logger: Arc<FileLogger>,
}

/// Completion senders shared by every worker; sends are bounded in time and
/// a send that cannot complete is counted as a drop.
#[derive(Clone)]
struct Completions {
    ingestion: Sender<IngestionCompletion>,
    control: Sender<ControlCompletion>,
    logger: Arc<dyn Logger>,
    probes: Arc<Probes>,
}

impl Completions {
    fn ingestion(&self, completion: IngestionCompletion) {
        if self
            .ingestion
            .send_timeout(completion, COMPLETION_SEND_TIMEOUT)
            .is_err()
        {
            self.dropped("manager ingestion inbox full or closed; completion dropped");
        }
    }

    fn control(&self, completion: ControlCompletion) {
        if self
            .control
            .send_timeout(completion, COMPLETION_SEND_TIMEOUT)
            .is_err()
        {
            self.dropped("manager control inbox full or closed; completion dropped");
        }
    }

    fn dropped(&self, message: &str) {
        self.probes
            .completion_drops
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.logger
            .log(LogLevel::Warn, "bench.composition", message);
    }
}

/// Workers whose bounded stop is part of shutdown.
trait StopWorker: Send {
    fn stop_within(&self, timeout: Duration) -> Option<String>;
}

struct NamedPool<J>(&'static str, PoolHandle<J>);

impl<J: Send + 'static> StopWorker for NamedPool<J> {
    fn stop_within(&self, timeout: Duration) -> Option<String> {
        let report = self.1.stop(timeout);
        (report.detached > 0).then(|| {
            format!(
                "worker {} still busy at shutdown ({} detached)",
                self.0, report.detached
            )
        })
    }
}

/// Shared pieces built before the workers.
struct Wiring {
    clock: Arc<dyn TimeProvider>,
    completions: Completions,
    probes: Arc<Probes>,
}

impl RefComposition {
    pub fn start(inputs: &CompositionInputs<'_>) -> BenchResult<Self> {
        let paths = state_paths(inputs.run_dir)?;
        let filesystem = real_filesystem();
        paths
            .ensure_directories(filesystem.as_ref())
            .map_err(|error| error.to_string())?;
        let mut repository = FileConfigRepository::new(paths.config_file.clone(), filesystem);
        let loaded = repository.load().map_err(|error| error.to_string())?;
        let config = loaded.config.clone();
        let clock: Arc<dyn TimeProvider> =
            Arc::new(BootClock::new().map_err(|error| error.to_string())?);
        let logger = Arc::new(
            FileLogger::start(
                &paths.log_file,
                usize::try_from(config.queues.log_bytes).unwrap_or(1 << 20),
                10 * 1024 * 1024,
                3,
                Arc::clone(&clock),
            )
            .map_err(|error| error.to_string())?,
        );
        let sqlite_runtime_ok = ensure_sqlite_version().is_ok();
        let ledger = ByteLedger::new(config.partition_limits());
        let ingress = QueueGate::new(
            "ingress",
            usize::try_from(config.queues.collection_batches).unwrap_or(32),
            u64::from(config.queues.ingress_bytes),
            ledger.clone(),
            Partition::Ingress,
        );
        let diagnostics = Diagnostics::new();
        let channels = ManagerChannels::create();
        let inboxes = channels.inboxes();
        let wiring = Wiring {
            clock: Arc::clone(&clock),
            completions: Completions {
                ingestion: inboxes.ingestion.clone(),
                control: inboxes.control.clone(),
                logger: Arc::clone(&logger) as Arc<dyn Logger>,
                probes: Arc::clone(&inputs.probes),
            },
            probes: Arc::clone(&inputs.probes),
        };
        let (writer, snapshots, history_index) = LiveStore::new(
            &config.store,
            &config.sampling,
            &config.runtime,
            ledger.clone(),
            Namespace::Replay,
            false,
        )
        .into_parts();
        let history = Arc::new(SwitchableHistory::new(Arc::new(history_index)));
        let mut workers: Vec<Box<dyn StopWorker>> = Vec::new();
        let store = wiring.spawn_store(writer)?;
        workers.push(Box::new(NamedPool("hot-store", store.clone())));
        let collector_registry = default_collectors();
        let collectors = wiring.spawn_collectors(&config, collector_registry.clone())?;
        workers.push(Box::new(NamedPool("collector", collectors.clone())));
        let history_pool =
            wiring.spawn_history(&config, Arc::clone(&history) as Arc<dyn HistoryReader>)?;
        workers.push(Box::new(NamedPool("history", history_pool.clone())));
        let config_actor = wiring.spawn_config(repository)?;
        workers.push(Box::new(NamedPool("config", config_actor.clone())));
        let audit = wiring.spawn_audit(&config, &paths, sqlite_runtime_ok)?;
        workers.push(Box::new(NamedPool("audit", audit.clone())));
        let actions = wiring.spawn_actions()?;
        workers.push(Box::new(NamedPool("action", actions.clone())));

        let host_id = host_identity(Path::new("/")).map_err(|error| error.to_string())?;
        let (specs, manifests, adapter_ids) = replay_hosts(inputs)?;
        let supervisor = Supervisor::start(
            SupervisorConfig::from_config(&config),
            specs,
            ledger.clone(),
            ingress.clone(),
            inboxes.ingestion.clone(),
            Arc::clone(&clock),
            Arc::clone(&logger) as Arc<dyn Logger>,
            diagnostics.clone(),
        )
        .map_err(|error| error.to_string())?;
        let self_pid =
            i32::try_from(std::process::id()).map_err(|_| "pid out of range".to_owned())?;
        let setup = ManagerSetup {
            saved_config: config.clone(),
            effective_config: config.clone(),
            value_origins: value_origins(&config, &loaded.origin, &[]),
            host_id: host_id.clone(),
            boot_id: clock.boot_id(),
            pid_namespace: pid_namespace_of(self_pid).map_err(|error| error.to_string())?,
            euid: current_euid(),
            self_pid,
            adapters: manifests,
            facts: HostFacts {
                host_name: "replay-ref".to_owned(),
                kernel: kernel_release().ok(),
                cpu_model: None,
                version: BuildInfo::current().version.to_owned(),
                namespace_label: Some("replay (simulated data)".to_owned()),
                utc_offset_s: 0,
            },
            namespace: Namespace::Replay,
        };
        let ports = ManagerPorts {
            clock: Arc::clone(&clock),
            adapters: Box::new(supervisor.clone()),
            collectors: Box::new(collectors),
            collector_registry,
            store: Box::new(store),
            snapshots: Arc::new(snapshots.clone()),
            history: Box::new(TimedHistoryDispatch {
                pool: history_pool,
                probes: Arc::clone(&inputs.probes),
            }),
            archive_reads: None,
            config: Box::new(config_actor),
            audit: Box::new(audit),
            actions: Box::new(actions),
            process_probe: None,
            process_history: None,
            logger: Arc::clone(&logger) as Arc<dyn Logger>,
            diagnostics: diagnostics.clone(),
            ledger: ledger.clone(),
            ingress,
        };
        let mut manager =
            Manager::with_channels(setup, ports, channels).map_err(|error| error.to_string())?;
        let session = manager.open_session().map_err(|error| error.to_string())?;
        let trigger = manager.shutdown_trigger();
        let manager_thread = std::thread::Builder::new()
            .name("dgxtop-manager".to_owned())
            .spawn(move || manager.run())
            .map_err(|error| error.to_string())?;
        Ok(Self {
            session,
            supervisor,
            ledger,
            diagnostics,
            clock,
            snapshots,
            config,
            adapter_ids,
            sqlite_runtime_ok,
            history,
            trigger,
            manager_thread: Some(manager_thread),
            workers,
            logger,
        })
    }

    /// Boot clock of the composition (same clock as the hosts' reads).
    pub fn clock_now(&self) -> u64 {
        self.clock.now_boot_ns()
    }

    /// Whether the Manager thread is still running.
    pub fn manager_alive(&self) -> bool {
        self.manager_thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }

    /// Bounded shutdown in the monitor's order; returns what was left behind.
    pub fn shutdown(mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        self.trigger.trigger("bench run finished");
        if let Some(thread) = self.manager_thread.take() {
            match join_within(thread, SHUTDOWN_BUDGET) {
                Some(summary) => warnings.extend(
                    summary
                        .unfinished
                        .into_iter()
                        .map(|item| format!("unfinished at shutdown: {item}")),
                ),
                None => warnings.push("manager did not stop within 3 s".to_owned()),
            }
        }
        for worker in &self.workers {
            warnings.extend(worker.stop_within(Duration::from_millis(500)));
        }
        let report = self.supervisor.shutdown(SHUTDOWN_BUDGET);
        if !report.completed_within_deadline {
            warnings.push("adapter hosts did not all stop within 3 s".to_owned());
        }
        for host in report.unreaped() {
            warnings.push(format!("adapter host may still be running: {host:?}"));
        }
        let _ = self.logger.flush(Duration::from_millis(250));
        let _ = self.logger.stop(Duration::from_millis(250));
        warnings
    }
}

fn state_paths(run_dir: &Path) -> BenchResult<StatePaths> {
    StatePaths::resolve(
        &ProcessEnv,
        Some(&run_dir.join("config/config.json")),
        Some(&run_dir.join("state")),
        Namespace::Replay,
    )
    .map_err(|error| error.to_string())
}

type HostPlan = (
    Vec<HostSpec>,
    Vec<dgxtop_core::observation::AdapterManifest>,
    Vec<String>,
);

/// One child-process replay host per fixture slice (the REF's resident
/// hosts); each host loads only its own slice file.
fn replay_hosts(inputs: &CompositionInputs<'_>) -> BenchResult<HostPlan> {
    let program = PathBuf::from(env!("CARGO_BIN_EXE_dgxtop"));
    if !program.is_file() {
        return Err(format!("dgxtop binary not built at {}", program.display()));
    }
    let mut specs = Vec::new();
    let mut manifests = Vec::new();
    let mut ids = Vec::new();
    for (index, fixture) in inputs.host_fixtures.iter().enumerate() {
        let name = format!("replay-{}", index + 1);
        let adapter_id = AdapterId::new(name.clone()).map_err(|error| error.to_string())?;
        specs.push(HostSpec {
            adapter_id: adapter_id.clone(),
            launch: HostLaunch::ChildProcess {
                program: program.clone(),
                args: vec![
                    "--adapter-host".to_owned(),
                    name.clone(),
                    "--replay-fixture".to_owned(),
                    fixture.display().to_string(),
                ],
            },
        });
        manifests.push(dgxtop_adapters::replay::manifest(&adapter_id, false));
        ids.push(name);
    }
    Ok((specs, manifests, ids))
}

impl Wiring {
    fn spawn_store(&self, writer: StateWriter) -> BenchResult<PoolHandle<StoreJob>> {
        let clock = Arc::clone(&self.clock);
        let completions = self.completions.clone();
        let probes = Arc::clone(&self.probes);
        spawn_actor(
            "hot-store",
            64,
            writer,
            move |writer: &mut StateWriter, job: StoreJob| match job {
                StoreJob::Commit { batch, guard } => {
                    let reads = reads_of(&batch);
                    let ack = writer.commit_collection(*batch, &guard, clock.now_boot_ns());
                    probes.on_commit(&reads, &ack);
                    completions.ingestion(IngestionCompletion::Committed(ack));
                }
                StoreJob::Gap(gap) => writer.record_gap(gap),
                StoreJob::Maintain => {
                    writer.observe_wall_clock(clock.now_boot_ns(), clock.now_utc_ns());
                    writer.maintain(clock.now_boot_ns());
                }
            },
        )
        .map_err(|error| error.to_string())
    }

    fn spawn_collectors(
        &self,
        config: &Config,
        registry: Vec<Arc<dyn Collector>>,
    ) -> BenchResult<PoolHandle<CollectionPlan>> {
        let completions = self.completions.clone();
        let probes = Arc::clone(&self.probes);
        spawn_pool(
            "collector",
            usize::try_from(config.runtime.compute_workers).unwrap_or(2),
            usize::try_from(config.queues.collector_jobs).unwrap_or(8),
            Arc::new(move |plan: CollectionPlan| {
                let collector = registry
                    .iter()
                    .find(|collector| collector.id() == &plan.collector_id)
                    .cloned();
                let collector_id = plan.collector_id.clone();
                let batch_id = plan.batch_id;
                let outcome = collector.map(|collector| {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        collector.collect(&plan)
                    }))
                });
                match outcome {
                    Some(Ok(batch)) => {
                        completions.ingestion(IngestionCompletion::Collected(Box::new(batch)))
                    }
                    Some(Err(_)) => {
                        probes
                            .collector_panics
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        completions.ingestion(IngestionCompletion::CollectorFailed {
                            collector_id,
                            batch_id,
                            message: "collector panicked".to_owned(),
                        });
                    }
                    None => completions.ingestion(IngestionCompletion::CollectorFailed {
                        collector_id,
                        batch_id,
                        message: "unknown collector".to_owned(),
                    }),
                }
            }),
        )
        .map_err(|error| error.to_string())
    }

    fn spawn_history(
        &self,
        config: &Config,
        reader: Arc<dyn HistoryReader>,
    ) -> BenchResult<PoolHandle<HistoryJob>> {
        let clock = Arc::clone(&self.clock);
        let completions = self.completions.clone();
        let probes = Arc::clone(&self.probes);
        spawn_pool(
            "history",
            usize::try_from(config.runtime.history_workers).unwrap_or(1),
            usize::try_from(config.queues.history_requests).unwrap_or(4),
            Arc::new(move |job: HistoryJob| {
                let result = reader.query(
                    &job.query,
                    job.deadline_boot_ns,
                    &job.cancel,
                    clock.as_ref(),
                );
                probes.on_history_result(&job.query.request_id, &result);
                completions.control(ControlCompletion::History {
                    request_id: job.query.request_id,
                    session_id: job.session_id,
                    session_generation: job.session_generation,
                    result,
                });
            }),
        )
        .map_err(|error| error.to_string())
    }

    fn spawn_config(&self, repository: FileConfigRepository) -> BenchResult<PoolHandle<ConfigJob>> {
        let completions = self.completions.clone();
        spawn_actor(
            "config",
            8,
            repository,
            move |repository: &mut FileConfigRepository, job: ConfigJob| {
                let ConfigJob::Persist {
                    request_id,
                    expected_revision,
                    candidate,
                } = job;
                let result = repository.compare_and_swap(expected_revision, &candidate);
                completions.control(ControlCompletion::Config { request_id, result });
            },
        )
        .map_err(|error| error.to_string())
    }

    fn spawn_audit(
        &self,
        config: &Config,
        paths: &StatePaths,
        sqlite_ok: bool,
    ) -> BenchResult<PoolHandle<AuditOp>> {
        let repository: Box<dyn AuditRepository> = if sqlite_ok {
            Box::new(
                SqliteAuditRepository::open(
                    &paths.audit_database,
                    AuditLimits::from_config(config),
                    Arc::clone(&self.clock),
                    Arc::new(StatvfsFreeSpace),
                )
                .map_err(|error| error.to_string())?,
            )
        } else {
            Box::new(UnavailableAudit::new("SQLite runtime gate failed"))
        };
        let completions = self.completions.clone();
        spawn_actor(
            "audit",
            64,
            repository,
            move |repository: &mut Box<dyn AuditRepository>, op: AuditOp| {
                completions.control(ControlCompletion::Audit(run_audit_op(
                    repository.as_mut(),
                    op,
                )));
            },
        )
        .map_err(|error| error.to_string())
    }

    fn spawn_actions(&self) -> BenchResult<PoolHandle<ActionOp>> {
        let completions = self.completions.clone();
        let clock = Arc::clone(&self.clock);
        spawn_actor(
            "action",
            16,
            RefusingActions,
            move |port: &mut RefusingActions, op: ActionOp| {
                completions.control(ControlCompletion::Action(run_action_op(
                    port,
                    op,
                    clock.as_ref(),
                    None,
                )));
            },
        )
        .map_err(|error| error.to_string())
    }
}

/// History dispatch that stamps each job's submit time for the probes.
struct TimedHistoryDispatch {
    pool: PoolHandle<HistoryJob>,
    probes: Arc<Probes>,
}

impl Dispatch<HistoryJob> for TimedHistoryDispatch {
    fn try_dispatch(&self, job: HistoryJob) -> Result<(), DispatchError> {
        let request_id = job.query.request_id;
        self.probes.on_history_dispatch(request_id, Instant::now());
        let result = self.pool.try_dispatch(job);
        if result.is_err() {
            self.probes.on_history_refused(&request_id);
        }
        result
    }
}

/// Process-action port that refuses every operation (fail closed).
struct RefusingActions;

const REFUSAL: &str = "the benchmark harness never signals processes";

impl ProcessActionPort for RefusingActions {
    fn prepare(
        &mut self,
        _key: &ProcessKey,
        _signal: Signal,
        _context: &CapabilityContext,
    ) -> Result<PreparedProcess, ActionError> {
        Err(ActionError::Disabled(REFUSAL.to_owned()))
    }

    fn send(
        &mut self,
        _handle: ActionHandle,
        _signal: Signal,
        _receipt: &DurableReceipt,
    ) -> SignalOutcome {
        SignalOutcome::NotSent {
            reason: REFUSAL.to_owned(),
        }
    }

    fn observe_exit(&mut self, _handle: ActionHandle) -> ExitObservation {
        ExitObservation::Unknown
    }

    fn release(&mut self, _handle: ActionHandle) {}
}

/// Joins a thread or gives up after `timeout` (the thread is left detached).
fn join_within<T: Send + 'static>(thread: JoinHandle<T>, timeout: Duration) -> Option<T> {
    let (sender, receiver) = bounded(1);
    let joiner = std::thread::Builder::new()
        .name("bench-join".to_owned())
        .spawn(move || {
            let _ = sender.send(thread.join());
        })
        .ok()?;
    let joined = receiver.recv_timeout(timeout).ok()?.ok();
    drop(joiner);
    joined
}
