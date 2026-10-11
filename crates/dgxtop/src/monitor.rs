//! Monitor mode: wires every component and runs the terminal frontend.
//!
//! Startup order (spec/dgxtop-v2/OPERATIONS.md §1): CLI → safe paths and the
//! instance lock → configuration and admission → SQLite gate → store workers
//! → Manager and adapter hosts → frontend (published before slow sources
//! warm up). Shutdown is bounded: frontend, Manager, workers, hosts.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded};
use dgxtop_adapters::common::RootedFiles;
use dgxtop_adapters::process_action::PidfdActionAdapter;
use dgxtop_adapters::procfs::ProcfsProcessProbe;
use dgxtop_adapters::replay::ReplayAdapter;
use dgxtop_collectors::process::ProcessProbeDeriver;
use dgxtop_collectors::registry::default_collectors;
use dgxtop_core::action::{ActionOp, AuditOp};
use dgxtop_core::budget::{ByteLedger, Partition, QueueGate};
use dgxtop_core::canonical::CollectionPlan;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::config::Config;
use dgxtop_core::diagnostics::Diagnostics;
use dgxtop_core::identity::{AdapterId, BootId, HostId};
use dgxtop_core::naming;
use dgxtop_core::observation::AdapterManifest;
use dgxtop_core::overview_history::{HistoryTierCompletion, HistoryTierJob};
use dgxtop_core::ports::{
    ArchiveGapCause, ArchiveJournal, ArchiveOutcome, ArchiveReader, ArchiveSink, AuditRepository,
    Collector, ConfigJob, ConfigRepository, ControlCompletion, Dispatch, HistoryJob,
    HistoryTierStore, IngestionCompletion, LogLevel, Logger, OverviewReader,
    ProcessHistoryCompletion, ProcessHistoryInput, ProcessHistoryJob, ProcessHistoryReader,
    ProcessHistoryStore, ProcessProbe, ProcessProbeJob, StoreJob, StoreWriter, TelemetryPort,
};
use dgxtop_core::quality::Namespace;
use dgxtop_history::{OverviewTiers, ProcessHistory, ProcessHistoryPorts, ProcessHistorySettings};
use dgxtop_manager::presentation::HostFacts;
use dgxtop_manager::{
    Manager, ManagerChannels, ManagerPorts, ManagerSetup, ProcessHistoryHandles, SessionHandle,
    ShutdownTrigger,
};
use dgxtop_runtime::clock::{
    BootClock, current_euid, host_identity, host_name, kernel_release, pid_namespace_of,
};
use dgxtop_runtime::logger::FileLogger;
use dgxtop_runtime::queues::{PoolHandle, spawn_actor, spawn_pool};
use dgxtop_runtime::signals::install_termination_flag;
use dgxtop_runtime::supervisor::{
    HostLaunch, HostSpec, Supervisor, SupervisorConfig, SupervisorHandle,
};
use dgxtop_runtime::terminal::{NonBlockingTerminal, TtyInputGate};
use dgxtop_runtime::timezone::local_utc_offset_s;
use dgxtop_store::archive::{ArchiveLimits, SqliteArchive, SqliteArchiveReader};
use dgxtop_store::audit::{AuditLimits, SqliteAuditRepository};
use dgxtop_store::config::FileConfigRepository;
use dgxtop_store::history_tiers::SqliteHistoryTiers;
use dgxtop_store::lock::InstanceLock;
use dgxtop_store::paths::StatePaths;
use dgxtop_store::process_history::ProcessHistoryFiles;
use dgxtop_store::sqlite::{StatvfsFreeSpace, ensure_sqlite_version};

use crate::cli::Cli;
use crate::control_ops::{run_action_op, run_audit_op};
use crate::fail_stop::FailStop;
use crate::modes::{EXIT_FAILURE, EXIT_INVALID, fail};
use crate::startup::{self, LoadedConfiguration};
use crate::version::BuildInfo;

/// Bounded wait for a worker to hand a completion to the Manager.
const COMPLETION_SEND_TIMEOUT: Duration = Duration::from_secs(1);
/// Bound of all application-controlled waits at shutdown together
/// (RUNTIME.md §8), after the frontend exits.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(3);
/// Part of the budget kept for flushing the log after everything else.
const LOG_FLUSH_RESERVE: Duration = Duration::from_millis(250);
/// Longest wait for one idle worker pool.
const WORKER_STOP_CAP: Duration = Duration::from_millis(500);
/// Longest wait for the terminal frontend after a termination signal before
/// the process exits anyway (AT39: never wait without bound).
const FRONTEND_EXIT_GRACE: Duration = Duration::from_secs(5);
/// Cadence of the signal watcher while nothing happens. The handler may
/// only store into an atomic (async-signal-safe, `dgxtop_runtime::signals`),
/// so there is nothing to block on; 200 ms keeps a signal's shutdown well
/// inside the 3 s budget at five wakes a second.
const SIGNAL_POLL: Duration = Duration::from_millis(200);
/// Cadence of the bounded wait for the frontend once a signal arrived.
const FRONTEND_EXIT_POLL: Duration = Duration::from_millis(50);
/// History tier jobs waiting (D-82): the history sends one startup read at
/// a time and a journal of closed buckets per finished minute.
const HISTORY_TIERS_QUEUE: usize = 16;
/// Probe jobs waiting (D-76); the process actor keeps one in flight.
const PROCESS_PROBE_QUEUE: usize = 2;
/// Inputs waiting for the history's process actor (D-79): a table each
/// process period, a tick each commit, the probe's and the stores' answers.
const PROCESS_HISTORY_INPUTS: usize = 64;
/// Process history store jobs waiting (D-76): a journal every 10 s, the
/// short-term file every 30 s, a run's read now and then.
const PROCESS_HISTORY_QUEUE: usize = 8;

/// Runs the monitor until the user quits, a signal arrives or the terminal hangs up.
pub fn run(cli: &Cli) -> ExitCode {
    let mut application = match Application::start(cli) {
        Ok(application) => application,
        Err(error) => return fail(error.exit_code, &error.message),
    };
    let frontend = application.run_frontend();
    let summary = application.shutdown();
    // The terminal may be gone (hang-up), so stderr failures are ignored.
    let mut stderr = std::io::stderr();
    for warning in summary {
        let _ = writeln!(
            stderr,
            "dgxtop: {}",
            dgxtop_core::text::sanitize_message(&warning)
        );
    }
    match application.fail_stop.reason() {
        Some(reason) => fail(EXIT_FAILURE, &format!("fail-stop: {reason}")),
        None => frontend,
    }
}

/// A startup failure with its exit status.
#[derive(Debug)]
struct StartError {
    exit_code: u8,
    message: String,
}

impl StartError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            exit_code: EXIT_INVALID,
            message: message.into(),
        }
    }

    fn failure(message: impl Into<String>) -> Self {
        Self {
            exit_code: EXIT_FAILURE,
            message: message.into(),
        }
    }
}

/// Workers whose bounded stop is part of shutdown.
/// Where the process probe and the process history's stores send what
/// they did: the history's process actor, set once it is spawned (it is
/// spawned after them, since it holds their handles).
type ProcessInbox = Arc<OnceLock<PoolHandle<ProcessHistoryInput>>>;

/// Hands a probe report or a store answer to the process actor, waiting a
/// bounded time while its queue is full: the actor never waits on these
/// workers, so this cannot deadlock, and a lost answer would leave a probe
/// or a write unanswered.
fn deliver_process_input(
    inbox: &ProcessInbox,
    input: ProcessHistoryInput,
    logger: &Arc<dyn Logger>,
) {
    let delivered = inbox.get().is_some_and(|actor| {
        actor
            .dispatch_within(input, COMPLETION_SEND_TIMEOUT)
            .is_ok()
    });
    if !delivered {
        logger.log(
            LogLevel::Error,
            "composition",
            "process history actor full or stopped; probe or store answer dropped",
        );
    }
}

trait StopOnShutdown {
    fn stop_within(&self, timeout: Duration) -> Option<String>;
}

impl<J: Send + 'static> StopOnShutdown for (String, PoolHandle<J>) {
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

/// Completion senders shared by every worker; sends are bounded in time.
#[derive(Clone)]
struct Completions {
    ingestion: Sender<IngestionCompletion>,
    control: Sender<ControlCompletion>,
    logger: Arc<dyn Logger>,
}

impl Completions {
    fn ingestion(&self, completion: IngestionCompletion) {
        if self
            .ingestion
            .send_timeout(completion, COMPLETION_SEND_TIMEOUT)
            .is_err()
        {
            self.logger.log(
                LogLevel::Warn,
                "composition",
                "manager ingestion inbox full or closed; completion dropped",
            );
        }
    }

    fn control(&self, completion: ControlCompletion) {
        if self
            .control
            .send_timeout(completion, COMPLETION_SEND_TIMEOUT)
            .is_err()
        {
            self.logger.log(
                LogLevel::Error,
                "composition",
                "manager control inbox full or closed; completion dropped",
            );
        }
    }
}

struct Application {
    session: SessionHandle,
    trigger: ShutdownTrigger,
    frontend_finished: Arc<AtomicBool>,
    fail_stop: FailStop,
    /// `None` result: the control thread panicked (fail-stop).
    manager_thread: Option<std::thread::JoinHandle<Option<dgxtop_manager::ShutdownSummary>>>,
    supervisor: SupervisorHandle,
    workers: Vec<Box<dyn StopOnShutdown>>,
    logger: Arc<FileLogger>,
    fps: u32,
    replay_root: Option<PathBuf>,
    _lock: InstanceLock,
}

impl Application {
    fn start(cli: &Cli) -> Result<Self, StartError> {
        let namespace = startup::namespace_of(cli);
        let replay_root = cli
            .replay
            .as_ref()
            .map(|_| replay_state_root())
            .transpose()?;
        let paths = resolve_paths(cli, replay_root.as_deref())?;
        let filesystem = startup::real_filesystem();
        paths
            .ensure_directories(filesystem.as_ref())
            .map_err(|error| StartError::failure(error.to_string()))?;
        let lock = InstanceLock::acquire(&paths.state_dir)
            .map_err(|error| StartError::failure(error.to_string()))?;
        let (loaded, config_repository) =
            startup::load_configuration(cli, &paths, Arc::clone(&filesystem))
                .map_err(|error| StartError::invalid(error.to_string()))?;
        let clock: Arc<dyn TimeProvider> =
            Arc::new(BootClock::new().map_err(|error| StartError::failure(error.to_string()))?);
        let logger = Arc::new(
            FileLogger::start(
                &paths.log_file,
                usize::try_from(loaded.effective.queues.log_bytes).unwrap_or(1 << 20),
                10 * 1024 * 1024,
                3,
                Arc::clone(&clock),
            )
            .map_err(|error| StartError::failure(error.to_string()))?,
        );
        let sqlite_ok = match ensure_sqlite_version() {
            Ok(_) => true,
            Err(error) => {
                logger.log(
                    LogLevel::Error,
                    "composition",
                    &format!("SQLite gate failed; audit and archive disabled: {error}"),
                );
                false
            }
        };
        let wiring = Wiring::new(
            &loaded,
            Arc::clone(&clock),
            Arc::clone(&logger) as Arc<dyn Logger>,
            namespace,
        );
        wiring.assemble(
            cli,
            &paths,
            &loaded,
            config_repository,
            sqlite_ok,
            lock,
            logger,
            replay_root,
        )
    }

    fn run_frontend(&mut self) -> ExitCode {
        dgxtop_ui::install_panic_hook();
        let terminal = NonBlockingTerminal::from_borrowed(std::io::stdout())
            .and_then(|transport| Ok((transport, TtyInputGate::controlling_terminal()?)));
        let (transport, input_gate) = match terminal {
            Ok(terminal) => terminal,
            Err(error) => {
                self.frontend_finished.store(true, Ordering::Release);
                return fail(EXIT_FAILURE, &format!("terminal unavailable: {error}"));
            }
        };
        let options = dgxtop_ui::TuiOptions {
            fps: self.fps,
            ..dgxtop_ui::TuiOptions::default()
        };
        let outcome = dgxtop_ui::run_tui(
            Box::new(self.session.clone()),
            Box::new(transport),
            Box::new(input_gate),
            options,
        );
        self.frontend_finished.store(true, Ordering::Release);
        dgxtop_ui::restore_terminal();
        match outcome {
            Ok(_) => ExitCode::SUCCESS,
            Err(error) => fail(EXIT_FAILURE, &error.to_string()),
        }
    }

    /// Bounded shutdown; returns warnings about anything left behind.
    fn shutdown(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        let total = ShutdownDeadline::start(SHUTDOWN_BUDGET);
        let control = ShutdownDeadline::start(SHUTDOWN_BUDGET.saturating_sub(LOG_FLUSH_RESERVE));
        self.trigger.trigger("frontend exited");
        if let Some(thread) = self.manager_thread.take() {
            match join_within(thread, control.remaining(SHUTDOWN_BUDGET)) {
                Some(Some(summary)) => warnings.extend(
                    summary
                        .unfinished
                        .into_iter()
                        .map(|item| format!("unfinished at shutdown: {item}")),
                ),
                Some(None) => warnings.push("manager stopped by a fail-stop".to_owned()),
                None => {
                    warnings.push("manager did not stop within the 3 s shutdown budget".to_owned())
                }
            }
        }
        for worker in &self.workers {
            warnings.extend(worker.stop_within(control.remaining(WORKER_STOP_CAP)));
        }
        let report = self.supervisor.shutdown(control.remaining(SHUTDOWN_BUDGET));
        if !report.completed_within_deadline {
            warnings
                .push("adapter hosts did not all stop within the 3 s shutdown budget".to_owned());
        }
        for host in report.unreaped() {
            warnings.push(format!("adapter host may still be running: {host:?}"));
        }
        let _ = self.logger.flush(total.remaining(LOG_FLUSH_RESERVE));
        let _ = self.logger.stop(total.remaining(LOG_FLUSH_RESERVE));
        if let Some(root) = self.replay_root.take() {
            let _ = std::fs::remove_dir_all(root);
        }
        warnings
    }
}

/// One deadline shared by consecutive shutdown waits, so their sum stays
/// within the budget instead of each wait getting the whole budget.
struct ShutdownDeadline {
    until: Instant,
}

impl ShutdownDeadline {
    fn start(budget: Duration) -> Self {
        Self {
            until: Instant::now() + budget,
        }
    }

    /// Time left before the deadline, at most `cap` (zero once it passed).
    fn remaining(&self, cap: Duration) -> Duration {
        self.until
            .saturating_duration_since(Instant::now())
            .min(cap)
    }
}

/// Joins a thread or gives up after `timeout` (the thread is left detached).
fn join_within<T: Send + 'static>(
    thread: std::thread::JoinHandle<T>,
    timeout: Duration,
) -> Option<T> {
    let (sender, receiver) = bounded(1);
    let joiner = std::thread::Builder::new()
        .name("dgxtop-join".to_owned())
        .spawn(move || {
            let _ = sender.send(thread.join());
        })
        .ok()?;
    let joined = receiver.recv_timeout(timeout).ok()?.ok();
    drop(joiner);
    joined
}

/// Private temporary state root for `--replay` (never the live state).
fn replay_state_root() -> Result<PathBuf, StartError> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let root = std::env::temp_dir().join(format!("dgxtop-replay-{}-{unique}", std::process::id()));
    std::fs::create_dir(&root)
        .map_err(|error| StartError::failure(format!("replay state directory: {error}")))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| StartError::failure(format!("replay state directory: {error}")))?;
    Ok(root)
}

fn resolve_paths(cli: &Cli, replay_root: Option<&Path>) -> Result<StatePaths, StartError> {
    match replay_root {
        Some(root) => {
            let mut replay_cli = cli.clone();
            replay_cli.config = Some(root.join("config/config.json"));
            replay_cli.state_dir = Some(root.join("state"));
            startup::resolve_paths(&replay_cli)
                .map_err(|error| StartError::invalid(error.to_string()))
        }
        None => startup::resolve_paths(cli).map_err(|error| StartError::invalid(error.to_string())),
    }
}

/// Shared pieces built before the workers.
struct Wiring {
    clock: Arc<dyn TimeProvider>,
    logger: Arc<dyn Logger>,
    diagnostics: Diagnostics,
    ledger: ByteLedger,
    ingress: QueueGate,
    namespace: Namespace,
    fail_stop: FailStop,
}

impl Wiring {
    fn new(
        loaded: &LoadedConfiguration,
        clock: Arc<dyn TimeProvider>,
        logger: Arc<dyn Logger>,
        namespace: Namespace,
    ) -> Self {
        let effective = &loaded.effective;
        let ledger = ByteLedger::new(effective.partition_limits());
        let ingress = QueueGate::new(
            "ingress",
            usize::try_from(effective.queues.collection_batches).unwrap_or(32),
            u64::from(effective.queues.ingress_bytes),
            ledger.clone(),
            Partition::Ingress,
        );
        Self {
            clock,
            fail_stop: FailStop::new(Arc::clone(&logger)),
            logger,
            diagnostics: Diagnostics::new(),
            ledger,
            ingress,
            namespace,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        self,
        cli: &Cli,
        paths: &StatePaths,
        loaded: &LoadedConfiguration,
        config_repository: FileConfigRepository,
        sqlite_ok: bool,
        lock: InstanceLock,
        logger: Arc<FileLogger>,
        replay_root: Option<PathBuf>,
    ) -> Result<Application, StartError> {
        let effective = &loaded.effective;
        let host_id = host_identity(Path::new("/"))
            .map_err(|error| StartError::failure(error.to_string()))?;
        let boot_id = self.clock.boot_id();
        let self_pid = i32::try_from(std::process::id())
            .map_err(|_| StartError::failure("pid out of range"))?;
        let pid_namespace =
            pid_namespace_of(self_pid).map_err(|error| StartError::failure(error.to_string()))?;
        let channels = ManagerChannels::create();
        let inboxes = channels.inboxes();
        let completions = Completions {
            ingestion: inboxes.ingestion.clone(),
            control: inboxes.control.clone(),
            logger: Arc::clone(&self.logger),
        };
        let mut workers: Vec<Box<dyn StopOnShutdown>> = Vec::new();

        let (writer, snapshots, history_index) = crate::live_store::LiveStore::new(
            &effective.store,
            &effective.sampling,
            &effective.runtime,
            self.ledger.clone(),
            self.namespace,
            effective.store.archive_enabled && sqlite_ok,
        )
        .into_parts();
        // The Overview's sizes follow the local clock and are kept with the
        // archive (D-82); both before the hot store feeds them.
        history_index
            .overview()
            .set_utc_offset(local_utc_offset_s());
        self.spawn_history_tiers(
            effective,
            paths,
            sqlite_ok,
            (history_index.overview().clone(), host_id.clone()),
            &mut workers,
        )?;
        let archive = self.spawn_archive(
            effective,
            paths,
            sqlite_ok,
            completions.clone(),
            &mut workers,
        )?;
        let store = self.spawn_store(
            writer,
            archive,
            completions.clone(),
            u64::from(effective.queues.archive_bytes),
        )?;
        workers.push(Box::new(("hot-store".to_owned(), store.clone())));
        let collector_registry = default_collectors();
        let collectors =
            spawn_collectors(effective, collector_registry.clone(), completions.clone())?;
        workers.push(Box::new(("collector".to_owned(), collectors.clone())));
        let history =
            self.spawn_history(effective, Arc::new(history_index), completions.clone())?;
        workers.push(Box::new(("history".to_owned(), history.clone())));
        let config = spawn_config(config_repository, completions.clone())?;
        workers.push(Box::new(("config".to_owned(), config.clone())));
        let audit = self.spawn_audit(effective, paths, sqlite_ok, completions.clone())?;
        workers.push(Box::new(("audit".to_owned(), audit.clone())));
        let actions = self.spawn_actions(completions.clone())?;
        workers.push(Box::new(("action".to_owned(), actions.clone())));
        let facts = host_facts(self.namespace);
        let process_inbox: ProcessInbox = Arc::new(OnceLock::new());
        let process_probe = self.spawn_process_probe(&process_inbox, &mut workers)?;
        let process_store = self.spawn_process_history_store(
            effective,
            paths,
            sqlite_ok,
            &process_inbox,
            &mut workers,
        )?;
        let process_history = self.spawn_process_history(
            (process_probe, process_store),
            (boot_id.clone(), facts.utc_offset_s),
            &process_inbox,
            &mut workers,
        )?;

        let (specs, manifests) = host_specs(cli, effective, self.namespace, &self.clock, &host_id)?;
        let supervisor = Supervisor::start(
            SupervisorConfig::from_config(effective),
            specs,
            self.ledger.clone(),
            self.ingress.clone(),
            inboxes.ingestion.clone(),
            Arc::clone(&self.clock),
            Arc::clone(&self.logger),
            self.diagnostics.clone(),
        )
        .map_err(|error| StartError::failure(error.to_string()))?;

        let setup = ManagerSetup {
            saved_config: loaded.saved.clone(),
            effective_config: effective.clone(),
            value_origins: loaded.origins.clone(),
            host_id: host_id.clone(),
            boot_id: boot_id.clone(),
            pid_namespace,
            euid: current_euid(),
            self_pid,
            adapters: manifests,
            facts,
            namespace: self.namespace,
        };
        let ports = ManagerPorts {
            clock: Arc::clone(&self.clock),
            adapters: Box::new(supervisor.clone()),
            collectors: Box::new(collectors),
            collector_registry,
            store: Box::new(store),
            snapshots: Arc::new(snapshots),
            history: Box::new(history),
            config: Box::new(config),
            audit: Box::new(audit),
            actions: Box::new(actions),
            process_history: Some(process_history),
            logger: Arc::clone(&self.logger),
            diagnostics: self.diagnostics.clone(),
            ledger: self.ledger.clone(),
            ingress: self.ingress.clone(),
        };
        let mut manager = Manager::with_channels(setup, ports, channels)
            .map_err(|error| StartError::failure(error.to_string()))?;
        let session = manager
            .open_session()
            .map_err(|error| StartError::failure(error.to_string()))?;
        let trigger = manager.shutdown_trigger();
        let shutdown = trigger.clone();
        self.fail_stop
            .connect(move |reason| shutdown.trigger(reason));
        let frontend_finished = Arc::new(AtomicBool::new(false));
        SignalWatch {
            trigger: trigger.clone(),
            session: session.clone(),
            frontend_finished: Arc::clone(&frontend_finished),
            logger: Arc::clone(&logger),
        }
        .spawn()?;
        let guard = self.fail_stop.clone();
        let manager_thread = std::thread::Builder::new()
            .name("dgxtop-manager".to_owned())
            .spawn(move || guard.guard("manager", move || manager.run()))
            .map_err(|error| StartError::failure(error.to_string()))?;
        Ok(Application {
            session,
            trigger,
            frontend_finished,
            fail_stop: self.fail_stop,
            manager_thread: Some(manager_thread),
            supervisor,
            workers,
            logger,
            fps: effective.viewer.fps,
            replay_root,
            _lock: lock,
        })
    }

    fn spawn_archive(
        &self,
        effective: &Config,
        paths: &StatePaths,
        sqlite_ok: bool,
        completions: Completions,
        workers: &mut Vec<Box<dyn StopOnShutdown>>,
    ) -> Result<Option<PoolHandle<ArchiveJournal>>, StartError> {
        if !(effective.store.archive_enabled && sqlite_ok) {
            return Ok(None);
        }
        let archive = SqliteArchive::open(
            &paths.history_database,
            ArchiveLimits::from_config(effective),
            Arc::clone(&self.clock),
            Arc::new(StatvfsFreeSpace),
        )
        .map_err(|error| StartError::failure(error.to_string()))?;
        let logger = Arc::clone(&self.logger);
        let handle = spawn_actor(
            "archive",
            8,
            archive,
            move |archive: &mut SqliteArchive, journal: ArchiveJournal| {
                let revision = journal.revision;
                let outcome = match archive.append(journal) {
                    Ok(receipt) => ArchiveOutcome::Committed { rows: receipt.rows },
                    Err(error) => {
                        logger.log(
                            LogLevel::Warn,
                            "archive",
                            &format!(
                                "archive append failed (in-memory history continues): {error}"
                            ),
                        );
                        ArchiveOutcome::Gap {
                            cause: ArchiveGapCause::WriteFailed,
                            detail: error.to_string(),
                        }
                    }
                };
                completions.control(ControlCompletion::Archive {
                    revision,
                    outcome,
                    status: Some(archive.status()),
                });
            },
        )
        .map_err(|error| StartError::failure(error.to_string()))?;
        workers.push(Box::new(("archive".to_owned(), handle.clone())));
        Ok(Some(handle))
    }

    fn spawn_store(
        &self,
        writer: dgxtop_state::StateWriter,
        archive: Option<PoolHandle<ArchiveJournal>>,
        completions: Completions,
        archive_bytes: u64,
    ) -> Result<PoolHandle<StoreJob>, StartError> {
        let clock = Arc::clone(&self.clock);
        let fail_stop = self.fail_stop.clone();
        spawn_actor(
            "hot-store",
            64,
            writer,
            move |writer: &mut dgxtop_state::StateWriter, job: StoreJob| {
                // After a fail-stop the writer's state is not trusted again.
                if fail_stop.is_halted() {
                    return;
                }
                fail_stop.guard("hot store", || match job {
                    StoreJob::Commit { batch, guard } => {
                        let ack = writer.commit_collection(*batch, &guard, clock.now_boot_ns());
                        completions.ingestion(IngestionCompletion::Committed(ack));
                    }
                    StoreJob::Gap(gap) => writer.record_gap(gap),
                    StoreJob::Maintain => {
                        // The archive needs the boot/UTC pairing to stamp finished rows.
                        writer.observe_wall_clock(clock.now_boot_ns(), clock.now_utc_ns());
                        writer.maintain(clock.now_boot_ns());
                    }
                });
                if fail_stop.is_halted() {
                    return;
                }
                if let Some(archive) = &archive
                    && let Some(journal) = writer.take_archive_journal(archive_bytes)
                {
                    let revision = journal.revision;
                    if archive.try_dispatch(journal).is_err() {
                        completions.logger.log(
                            LogLevel::Warn,
                            "archive",
                            "archive worker busy; journal dropped and reported as an archive gap",
                        );
                        completions.control(ControlCompletion::Archive {
                            revision,
                            outcome: ArchiveOutcome::Gap {
                                cause: ArchiveGapCause::WorkerBusy,
                                detail: "archive worker busy".to_owned(),
                            },
                            status: None,
                        });
                    }
                }
            },
        )
        .map_err(|error| StartError::failure(error.to_string()))
    }

    /// The history's store of the Overview's sizes (D-82), on with the
    /// archive: one actor with the tiers database and a read-only reader of
    /// the archive, answering the history directly (the Manager never sees
    /// these jobs). Attached before the hot store starts, so live minutes
    /// wait for the startup load.
    fn spawn_history_tiers(
        &self,
        effective: &Config,
        paths: &StatePaths,
        sqlite_ok: bool,
        (overview, host_id): (OverviewTiers, HostId),
        workers: &mut Vec<Box<dyn StopOnShutdown>>,
    ) -> Result<(), StartError> {
        if !(effective.store.archive_enabled && sqlite_ok) {
            return Ok(());
        }
        let files = HistoryTierFiles {
            tiers: SqliteHistoryTiers::open(&paths.history_tiers_database, Arc::clone(&self.clock)),
            archive: SqliteArchiveReader::new(&paths.history_database),
        };
        let answer = overview.clone();
        let handle = spawn_actor(
            "history-tiers",
            HISTORY_TIERS_QUEUE,
            files,
            move |files: &mut HistoryTierFiles, job: HistoryTierJob| {
                answer.on_store(files.run(job));
            },
        )
        .map_err(|error| StartError::failure(error.to_string()))?;
        workers.push(Box::new(("history-tiers".to_owned(), handle.clone())));
        let run_start_utc_ns = i64::try_from(self.clock.now_utc_ns()).unwrap_or(i64::MAX);
        overview.attach_store(Box::new(handle), host_id, run_start_utc_ns);
        Ok(())
    }

    fn spawn_history(
        &self,
        effective: &Config,
        history: Arc<dyn OverviewReader>,
        completions: Completions,
    ) -> Result<PoolHandle<HistoryJob>, StartError> {
        let clock = Arc::clone(&self.clock);
        spawn_pool(
            "history",
            usize::try_from(effective.runtime.history_workers).unwrap_or(1),
            usize::try_from(effective.queues.history_requests).unwrap_or(4),
            Arc::new(move |job: HistoryJob| {
                let result = history.read(
                    &job.request,
                    job.deadline_boot_ns,
                    &job.cancel,
                    clock.as_ref(),
                );
                completions.control(ControlCompletion::History {
                    request_id: job.request_id,
                    session_id: job.session_id,
                    session_generation: job.session_generation,
                    result,
                });
            }),
        )
        .map_err(|error| StartError::failure(error.to_string()))
    }

    fn spawn_audit(
        &self,
        effective: &Config,
        paths: &StatePaths,
        sqlite_ok: bool,
        completions: Completions,
    ) -> Result<PoolHandle<AuditOp>, StartError> {
        let repository: Box<dyn AuditRepository> = if sqlite_ok {
            Box::new(
                SqliteAuditRepository::open(
                    &paths.audit_database,
                    AuditLimits::from_config(effective),
                    Arc::clone(&self.clock),
                    Arc::new(StatvfsFreeSpace),
                )
                .map_err(|error| StartError::failure(error.to_string()))?,
            )
        } else {
            Box::new(crate::audit_unavailable::UnavailableAudit::new(
                "SQLite runtime gate failed",
            ))
        };
        spawn_actor(
            "audit",
            64,
            repository,
            move |repository: &mut Box<dyn AuditRepository>, op: AuditOp| {
                let completion = run_audit_op(repository.as_mut(), op);
                completions.control(ControlCompletion::Audit(completion));
            },
        )
        .map_err(|error| StartError::failure(error.to_string()))
    }

    /// The process history's probe (D-76): live processes only, so none in
    /// a replay. Its reports go to the history's process actor.
    fn spawn_process_probe(
        &self,
        inbox: &ProcessInbox,
        workers: &mut Vec<Box<dyn StopOnShutdown>>,
    ) -> Result<Option<PoolHandle<ProcessProbeJob>>, StartError> {
        if self.namespace != Namespace::Live {
            return Ok(None);
        }
        let probe = ProcfsProcessProbe::new(Arc::new(RootedFiles::host()), Arc::clone(&self.clock));
        let inbox = Arc::clone(inbox);
        let logger = Arc::clone(&self.logger);
        let handle = spawn_actor(
            "process-probe",
            PROCESS_PROBE_QUEUE,
            probe,
            move |probe: &mut ProcfsProcessProbe, job: ProcessProbeJob| {
                let report = probe.probe(&job.targets);
                deliver_process_input(&inbox, ProcessHistoryInput::Probe(report), &logger);
            },
        )
        .map_err(|error| StartError::failure(error.to_string()))?;
        workers.push(Box::new(("process-probe".to_owned(), handle.clone())));
        Ok(Some(handle))
    }

    /// The history's process actor (D-79): the GPU process runs and
    /// memory, fed by the Manager, answered by the probe and the stores,
    /// read by the Manager through what it publishes.
    fn spawn_process_history(
        &self,
        (probe, store): (
            Option<PoolHandle<ProcessProbeJob>>,
            Option<PoolHandle<ProcessHistoryJob>>,
        ),
        (boot_id, utc_offset_s): (BootId, i32),
        inbox: &ProcessInbox,
        workers: &mut Vec<Box<dyn StopOnShutdown>>,
    ) -> Result<ProcessHistoryHandles, StartError> {
        let (actor, published) = ProcessHistory::new(
            ProcessHistorySettings {
                boot_id,
                utc_offset_s,
            },
            ProcessHistoryPorts {
                probe: probe.map(|port| Box::new(port) as Box<dyn Dispatch<ProcessProbeJob>>),
                probe_deriver: Arc::new(ProcessProbeDeriver),
                store: store.map(|port| Box::new(port) as Box<dyn Dispatch<ProcessHistoryJob>>),
                clock: Arc::clone(&self.clock),
                logger: Arc::clone(&self.logger),
            },
        );
        let handle = spawn_actor(
            "process-history",
            PROCESS_HISTORY_INPUTS,
            actor,
            |actor: &mut ProcessHistory, input: ProcessHistoryInput| actor.handle(input),
        )
        .map_err(|error| StartError::failure(error.to_string()))?;
        workers.push(Box::new(("process-history".to_owned(), handle.clone())));
        // The probe and the stores answer only jobs the actor sends, so the
        // inbox is set before any answer can come.
        let _ = inbox.set(handle.clone());
        let published = Arc::new(published);
        Ok(ProcessHistoryHandles {
            inputs: Box::new(handle),
            runs: Arc::clone(&published) as Arc<dyn ProcessHistoryReader>,
            memory: published,
        })
    }

    /// The process history's stores (D-76), on with
    /// `store.process_history_enabled`, unset following the history archive
    /// (D-80): without them the runs live only while dgxtop runs. What a
    /// job did goes to the history's process actor.
    fn spawn_process_history_store(
        &self,
        effective: &Config,
        paths: &StatePaths,
        sqlite_ok: bool,
        inbox: &ProcessInbox,
        workers: &mut Vec<Box<dyn StopOnShutdown>>,
    ) -> Result<Option<PoolHandle<ProcessHistoryJob>>, StartError> {
        // Its own switch, unset following the archive (D-80).
        if !(effective.store.process_history_on_disk() && sqlite_ok) {
            return Ok(None);
        }
        let store = ProcessHistoryFiles::open(
            &paths.process_history_database,
            &paths.process_recent_file,
            Arc::clone(&self.clock),
        );
        let inbox = Arc::clone(inbox);
        let logger = Arc::clone(&self.logger);
        let handle = spawn_actor(
            "process-history-store",
            PROCESS_HISTORY_QUEUE,
            store,
            move |store: &mut ProcessHistoryFiles, job: ProcessHistoryJob| {
                let completion = match job {
                    ProcessHistoryJob::Load => ProcessHistoryCompletion::Loaded(store.load()),
                    ProcessHistoryJob::Append(journal) => {
                        ProcessHistoryCompletion::Appended(store.append(&journal))
                    }
                    ProcessHistoryJob::SaveRecent(recent) => {
                        ProcessHistoryCompletion::Saved(store.save_recent(&recent))
                    }
                    ProcessHistoryJob::ReadRun(key) => {
                        let result = store.read_run(&key);
                        ProcessHistoryCompletion::RunRead { key, result }
                    }
                };
                deliver_process_input(&inbox, ProcessHistoryInput::Store(completion), &logger);
            },
        )
        .map_err(|error| StartError::failure(error.to_string()))?;
        workers.push(Box::new((
            "process-history-store".to_owned(),
            handle.clone(),
        )));
        Ok(Some(handle))
    }

    fn spawn_actions(&self, completions: Completions) -> Result<PoolHandle<ActionOp>, StartError> {
        let clock = Arc::clone(&self.clock);
        let adapter = PidfdActionAdapter::new(Arc::clone(&clock));
        let fail_stop = self.fail_stop.clone();
        spawn_actor(
            "action",
            16,
            adapter,
            move |port: &mut PidfdActionAdapter, op: ActionOp| {
                let halted = fail_stop.reason();
                let completion = run_action_op(port, op, clock.as_ref(), halted.as_deref());
                completions.control(ControlCompletion::Action(completion));
            },
        )
        .map_err(|error| StartError::failure(error.to_string()))
    }
}

fn spawn_collectors(
    effective: &Config,
    registry: Vec<Arc<dyn Collector>>,
    completions: Completions,
) -> Result<PoolHandle<CollectionPlan>, StartError> {
    spawn_pool(
        "collector",
        usize::try_from(effective.runtime.compute_workers).unwrap_or(2),
        usize::try_from(effective.queues.collector_jobs).unwrap_or(8),
        Arc::new(move |plan: CollectionPlan| {
            let collector = registry
                .iter()
                .find(|collector| collector.id() == &plan.collector_id)
                .cloned();
            let collector_id = plan.collector_id.clone();
            let batch_id = plan.batch_id;
            let outcome = collector.map(|collector| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| collector.collect(&plan)))
            });
            match outcome {
                Some(Ok(batch)) => {
                    completions.ingestion(IngestionCompletion::Collected(Box::new(batch)))
                }
                Some(Err(_)) => completions.ingestion(IngestionCompletion::CollectorFailed {
                    collector_id,
                    batch_id,
                    message: "collector panicked".to_owned(),
                }),
                None => completions.ingestion(IngestionCompletion::CollectorFailed {
                    collector_id,
                    batch_id,
                    message: "unknown collector".to_owned(),
                }),
            }
        }),
    )
    .map_err(|error| StartError::failure(error.to_string()))
}

fn spawn_config(
    repository: FileConfigRepository,
    completions: Completions,
) -> Result<PoolHandle<ConfigJob>, StartError> {
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
    .map_err(|error| StartError::failure(error.to_string()))
}

/// Adapter hosts of this run plus their manifests.
fn host_specs(
    cli: &Cli,
    effective: &Config,
    namespace: Namespace,
    clock: &Arc<dyn TimeProvider>,
    host_id: &HostId,
) -> Result<(Vec<HostSpec>, Vec<AdapterManifest>), StartError> {
    if namespace == Namespace::Replay {
        return replay_host(cli, clock, host_id);
    }
    let program =
        std::env::current_exe().map_err(|error| StartError::failure(error.to_string()))?;
    let limit = usize::try_from(effective.sampling.max_adapters).unwrap_or(8);
    let mut specs = Vec::new();
    let mut manifests = Vec::new();
    let selected: Vec<&str> = naming::adapters::LIVE
        .iter()
        .copied()
        .filter(|adapter| effective.sampling.gpu_enabled || *adapter != naming::adapters::NVML)
        .take(limit)
        .collect();
    for adapter in selected.iter().copied() {
        let Some(manifest) = dgxtop_adapters::registry::live_manifest(adapter) else {
            continue;
        };
        specs.push(HostSpec {
            adapter_id: AdapterId::new(adapter)
                .map_err(|error| StartError::failure(error.to_string()))?,
            launch: HostLaunch::ChildProcess {
                program: program.clone(),
                args: host_args(adapter, &selected),
            },
        });
        manifests.push(manifest);
    }
    Ok((specs, manifests))
}

/// An adapter host's arguments; hwmon reads the thermal zones' mirrors
/// itself when the thermal adapter is not among the hosts (D-81).
fn host_args(adapter: &str, selected: &[&str]) -> Vec<String> {
    let mut args = vec!["--adapter-host".to_owned(), adapter.to_owned()];
    if adapter == naming::adapters::HWMON && !selected.contains(&naming::adapters::THERMAL) {
        args.push("--hwmon-reads-thermal-zones".to_owned());
    }
    args
}

fn replay_host(
    cli: &Cli,
    clock: &Arc<dyn TimeProvider>,
    host_id: &HostId,
) -> Result<(Vec<HostSpec>, Vec<AdapterManifest>), StartError> {
    let fixture = cli
        .replay
        .clone()
        .ok_or_else(|| StartError::invalid("--replay requires a fixture"))?;
    let adapter_id = AdapterId::new(naming::adapters::REPLAY)
        .map_err(|error| StartError::failure(error.to_string()))?;
    // Validate the fixture up front so a bad file is reported before the UI starts.
    let probe = ReplayAdapter::from_path(
        &fixture,
        adapter_id.clone(),
        Arc::clone(clock),
        host_id.clone(),
        None,
    )
    .map_err(|error| StartError::invalid(format!("replay fixture: {error}")))?
    .with_in_process(true);
    let manifest = probe.manifest().clone();
    drop(probe);
    let factory_clock = Arc::clone(clock);
    let factory_host = host_id.clone();
    let factory_adapter = adapter_id.clone();
    let factory: Arc<dyn Fn() -> Box<dyn TelemetryPort> + Send + Sync> = Arc::new(move || {
        match ReplayAdapter::from_path(
            &fixture,
            factory_adapter.clone(),
            Arc::clone(&factory_clock),
            factory_host.clone(),
            None,
        ) {
            Ok(adapter) => Box::new(adapter.with_in_process(true)),
            Err(error) => Box::new(crate::replay_unavailable::UnavailableReplay::new(
                factory_adapter.clone(),
                error.to_string(),
            )),
        }
    });
    Ok((
        vec![HostSpec {
            adapter_id,
            launch: HostLaunch::InProcess(factory),
        }],
        vec![manifest],
    ))
}

fn host_facts(namespace: Namespace) -> HostFacts {
    HostFacts {
        host_name: host_name().unwrap_or_else(|_| "unknown host".to_owned()),
        kernel: kernel_release().ok(),
        cpu_model: None,
        version: BuildInfo::current().version.to_owned(),
        namespace_label: (namespace == Namespace::Replay)
            .then(|| "replay (simulated data)".to_owned()),
        utc_offset_s: local_utc_offset_s(),
    }
}

/// Turns termination signals (SIGTERM, SIGINT, SIGHUP) into a shutdown
/// request, then bounds the wait for the terminal frontend: the terminal
/// library can spin on a terminal that hangs up in the middle of a read, and
/// nothing else could end the process then.
struct SignalWatch {
    trigger: ShutdownTrigger,
    session: SessionHandle,
    frontend_finished: Arc<AtomicBool>,
    logger: Arc<FileLogger>,
}

impl SignalWatch {
    fn spawn(self) -> Result<(), StartError> {
        let flag =
            install_termination_flag().map_err(|error| StartError::failure(error.to_string()))?;
        std::thread::Builder::new()
            .name("dgxtop-signals".to_owned())
            .spawn(move || self.run(&flag))
            .map(|_| ())
            .map_err(|error| StartError::failure(error.to_string()))
    }

    fn run(self, flag: &AtomicBool) {
        use dgxtop_core::ports::SessionPort;
        while !flag.load(Ordering::Acquire) {
            if self.trigger.is_triggered() {
                return;
            }
            std::thread::sleep(SIGNAL_POLL);
        }
        self.session.request_shutdown("termination signal");
        self.trigger.trigger("termination signal");
        if wait_for_flag(&self.frontend_finished, FRONTEND_EXIT_GRACE) {
            return;
        }
        self.logger.log(
            LogLevel::Error,
            "composition",
            "terminal frontend still running 5 s after a termination signal; exiting",
        );
        let _ = self.logger.flush(Duration::from_millis(250));
        dgxtop_ui::restore_terminal();
        std::process::exit(i32::from(EXIT_FAILURE));
    }
}

/// Waits until `flag` is set or `timeout` passes; returns whether it was set.
fn wait_for_flag(flag: &AtomicBool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if flag.load(Ordering::Acquire) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(FRONTEND_EXIT_POLL));
    }
}

/// The history tier actor's state (D-82): the tiers database and the
/// archive's reader, each answering the job the history sent.
struct HistoryTierFiles {
    tiers: SqliteHistoryTiers,
    archive: SqliteArchiveReader,
}

impl HistoryTierFiles {
    fn run(&mut self, job: HistoryTierJob) -> HistoryTierCompletion {
        match job {
            HistoryTierJob::Load => HistoryTierCompletion::Loaded(self.tiers.load()),
            HistoryTierJob::Append(journal) => {
                HistoryTierCompletion::Appended(self.tiers.append(&journal))
            }
            HistoryTierJob::ListArchived(list) => {
                HistoryTierCompletion::Listed(self.archive.archived_series(&list))
            }
            HistoryTierJob::ReadArchived(read) => {
                let result = self.archive.archived_minutes(&read);
                HistoryTierCompletion::ArchivedRead { read, result }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// hwmon reads the thermal zones' mirrors only when the thermal
    /// adapter is not among the hosts (D-81).
    #[test]
    fn hwmon_reads_the_zones_only_without_the_thermal_host() {
        use dgxtop_core::naming::adapters::{HWMON, PROCFS, THERMAL};
        assert_eq!(
            super::host_args(HWMON, &[PROCFS, HWMON, THERMAL]),
            vec!["--adapter-host", HWMON]
        );
        assert_eq!(
            super::host_args(HWMON, &[PROCFS, HWMON]),
            vec!["--adapter-host", HWMON, "--hwmon-reads-thermal-zones"]
        );
        assert_eq!(
            super::host_args(PROCFS, &[PROCFS]),
            vec!["--adapter-host", PROCFS]
        );
    }

    use super::*;

    #[test]
    fn shutdown_waits_share_one_deadline() {
        let deadline = ShutdownDeadline::start(Duration::from_millis(200));
        assert!(deadline.remaining(Duration::from_secs(3)) <= Duration::from_millis(200));
        assert_eq!(
            deadline.remaining(Duration::from_millis(10)),
            Duration::from_millis(10)
        );
        std::thread::sleep(Duration::from_millis(220));
        assert_eq!(deadline.remaining(Duration::from_secs(3)), Duration::ZERO);
        let spent = ShutdownDeadline::start(Duration::ZERO);
        assert_eq!(spent.remaining(Duration::from_secs(1)), Duration::ZERO);
    }

    /// The signal watcher polls (no handler can wake it), slowly enough to
    /// stay idle and fast enough that a signal's shutdown keeps its budget.
    #[test]
    fn the_signal_poll_is_slow_but_inside_the_shutdown_budget() {
        assert!(SIGNAL_POLL >= Duration::from_millis(200));
        assert!(SIGNAL_POLL * 10 <= SHUTDOWN_BUDGET);
        assert!(FRONTEND_EXIT_POLL < SIGNAL_POLL);
    }

    #[test]
    fn an_unset_flag_gives_up_at_the_deadline() {
        let flag = AtomicBool::new(false);
        let begin = Instant::now();
        assert!(!wait_for_flag(&flag, Duration::from_millis(120)));
        assert!(begin.elapsed() >= Duration::from_millis(110));
        assert!(begin.elapsed() < Duration::from_secs(2));
        assert!(!wait_for_flag(&flag, Duration::ZERO));
    }
}
