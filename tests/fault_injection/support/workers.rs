//! Real worker wiring for the scenarios, mirroring the private assembly in
//! `crates/dgxtop/src/monitor.rs`: the config, audit, action, collector,
//! history and LiveState workers on the runtime's fixed pools and actors, and
//! the archive actor the LiveState feeds. The only deviations are test seams:
//! a [`Gate`] in front of storage workers, probes recording what the archive
//! did, and a recording action port that never signals anything. The audit
//! and action workers run the production `dgxtop::control_ops` functions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crossbeam_channel::Sender;
use dgxtop::control_ops::{run_action_op, run_audit_op};
use dgxtop::store_actor::run_store_job;
use dgxtop_core::action::{
    ActionError, ActionHandle, ActionOp, AuditOp, CapabilityContext, DurableReceipt,
    ExitObservation, PersistenceStatus, PreparedProcess, Signal, SignalOutcome, VerifiedIdentity,
};
use dgxtop_core::canonical::CollectionPlan;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::config::Config;
use dgxtop_core::errors::InfraError;
use dgxtop_core::identity::ProcessKey;
use dgxtop_core::ports::{
    ArchiveJournal, ArchiveReceipt, ArchiveSink, AuditRepository, Collector, ConfigJob,
    ConfigRepository, ControlCompletion, Dispatch, HistoryJob, IngestionCompletion, OverviewReader,
    ProcessActionPort, StoreJob,
};
use dgxtop_manager::ManagerInboxes;
use dgxtop_runtime::queues::{PoolHandle, StopReport, spawn_actor, spawn_pool};
use dgxtop_state::StateWriter;
use dgxtop_store::archive::{ArchiveStats, SqliteArchive};
use dgxtop_store::config::FileConfigRepository;

use super::probes::Gate;

/// Bounded wait for a worker to hand a completion over (as in monitor.rs).
const COMPLETION_SEND_TIMEOUT: Duration = Duration::from_secs(1);
/// Mailbox sizes used by the composition root.
const CONFIG_MAILBOX: usize = 8;
const AUDIT_MAILBOX: usize = 64;
const ACTION_MAILBOX: usize = 16;
const STORE_MAILBOX: usize = 64;
const ARCHIVE_MAILBOX: usize = 8;

/// Completion senders shared by every worker; sends are bounded in time.
#[derive(Clone)]
pub struct Completions {
    ingestion: Sender<IngestionCompletion>,
    control: Sender<ControlCompletion>,
}

impl Completions {
    pub fn from_inboxes(inboxes: &ManagerInboxes) -> Self {
        Self {
            ingestion: inboxes.ingestion.clone(),
            control: inboxes.control.clone(),
        }
    }

    pub fn new(ingestion: Sender<IngestionCompletion>, control: Sender<ControlCompletion>) -> Self {
        Self { ingestion, control }
    }

    fn ingestion(&self, completion: IngestionCompletion) {
        let _ = self
            .ingestion
            .send_timeout(completion, COMPLETION_SEND_TIMEOUT);
    }

    fn control(&self, completion: ControlCompletion) {
        let _ = self
            .control
            .send_timeout(completion, COMPLETION_SEND_TIMEOUT);
    }
}

/// Bounded stop of one worker (its `PoolHandle::stop`).
type Stopper = Box<dyn Fn(Duration) -> StopReport + Send>;

/// Every worker of one rig, stopped (bounded) together.
#[derive(Default)]
pub struct WorkerSet {
    workers: Vec<(String, Stopper)>,
}

impl WorkerSet {
    pub fn track<J: Send + 'static>(&mut self, handle: &PoolHandle<J>) {
        let handle = handle.clone();
        let name = handle.name().to_owned();
        self.workers
            .push((name, Box::new(move |timeout| handle.stop(timeout))));
    }

    /// Stops every worker, each within `timeout` (busy ones are detached).
    pub fn stop_all(&mut self, timeout: Duration) -> Vec<(String, StopReport)> {
        self.workers
            .drain(..)
            .map(|(name, stop)| (name, stop(timeout)))
            .collect()
    }
}

impl Drop for WorkerSet {
    fn drop(&mut self) {
        self.stop_all(Duration::from_millis(500));
    }
}

pub fn spawn_config_worker(
    repository: FileConfigRepository,
    completions: Completions,
) -> PoolHandle<ConfigJob> {
    spawn_actor(
        "config",
        CONFIG_MAILBOX,
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
    .expect("spawn config worker")
}

/// Audit actor. Intent writes first pass `intent_gate`: the audit disk hangs
/// exactly under the write that would authorize a signal, while best-effort
/// attempt records and status probes queued earlier still go through.
pub fn spawn_audit_worker(
    repository: Box<dyn AuditRepository>,
    intent_gate: Gate,
    completions: Completions,
) -> PoolHandle<AuditOp> {
    spawn_actor(
        "audit",
        AUDIT_MAILBOX,
        repository,
        move |repository: &mut Box<dyn AuditRepository>, op: AuditOp| {
            if matches!(op, AuditOp::AppendIntent(_)) {
                intent_gate.pass();
            }
            let completion = run_audit_op(repository.as_mut(), op);
            completions.control(ControlCompletion::Audit(completion));
        },
    )
    .expect("spawn audit worker")
}

/// One call the action worker made on its port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionCall {
    Prepare(ProcessKey),
    Send(ActionHandle),
    ObserveExit(ActionHandle),
    Release(ActionHandle),
}

/// Shared record of every action-port call.
#[derive(Debug, Clone, Default)]
pub struct ActionLog(Arc<Mutex<Vec<ActionCall>>>);

impl ActionLog {
    fn push(&self, call: ActionCall) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
    }

    pub fn calls(&self) -> Vec<ActionCall> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn sends(&self) -> usize {
        self.calls()
            .iter()
            .filter(|call| matches!(call, ActionCall::Send(_)))
            .count()
    }
}

/// `ProcessActionPort` stand-in: verifies every target as owned by
/// `owner_uid` and reports sends as delivered. It never touches a process,
/// so no scenario can signal anything it did not create.
pub struct RecordingActionPort {
    log: ActionLog,
    owner_uid: u32,
    next_handle: u64,
}

impl RecordingActionPort {
    pub fn new(log: ActionLog, owner_uid: u32) -> Self {
        Self {
            log,
            owner_uid,
            next_handle: 100,
        }
    }
}

impl ProcessActionPort for RecordingActionPort {
    fn prepare(
        &mut self,
        key: &ProcessKey,
        _signal: Signal,
        _context: &CapabilityContext,
    ) -> Result<PreparedProcess, ActionError> {
        self.log.push(ActionCall::Prepare(key.clone()));
        self.next_handle += 1;
        Ok(PreparedProcess {
            handle: ActionHandle(self.next_handle),
            verified: VerifiedIdentity {
                key: key.clone(),
                uid: self.owner_uid,
                basename: "train.py".to_owned(),
            },
            display_summary: key.summary(),
        })
    }

    fn send(
        &mut self,
        handle: ActionHandle,
        _signal: Signal,
        _receipt: &DurableReceipt,
    ) -> SignalOutcome {
        self.log.push(ActionCall::Send(handle));
        SignalOutcome::Sent
    }

    fn observe_exit(&mut self, handle: ActionHandle) -> ExitObservation {
        self.log.push(ActionCall::ObserveExit(handle));
        ExitObservation::ExitObserved
    }

    fn release(&mut self, handle: ActionHandle) {
        self.log.push(ActionCall::Release(handle));
    }
}

pub fn spawn_action_worker(
    port: RecordingActionPort,
    clock: Arc<dyn TimeProvider>,
    completions: Completions,
) -> PoolHandle<ActionOp> {
    spawn_actor(
        "action",
        ACTION_MAILBOX,
        port,
        move |port: &mut RecordingActionPort, op: ActionOp| {
            let completion = run_action_op(port, op, clock.as_ref(), None);
            completions.control(ControlCompletion::Action(completion));
        },
    )
    .expect("spawn action worker")
}

pub fn spawn_collector_pool(
    config: &Config,
    registry: Vec<Arc<dyn Collector>>,
    completions: Completions,
) -> PoolHandle<CollectionPlan> {
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
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| collector.collect(&plan)))
            });
            let completion = match outcome {
                Some(Ok(batch)) => IngestionCompletion::Collected(Box::new(batch)),
                Some(Err(_)) => IngestionCompletion::CollectorFailed {
                    collector_id,
                    batch_id,
                    message: "collector panicked".to_owned(),
                },
                None => IngestionCompletion::CollectorFailed {
                    collector_id,
                    batch_id,
                    message: "unknown collector".to_owned(),
                },
            };
            completions.ingestion(completion);
        }),
    )
    .expect("spawn collector pool")
}

pub fn spawn_history_pool(
    config: &Config,
    history: Arc<dyn OverviewReader>,
    clock: Arc<dyn TimeProvider>,
    completions: Completions,
) -> PoolHandle<HistoryJob> {
    spawn_pool(
        "history",
        usize::try_from(config.runtime.history_workers).unwrap_or(1),
        usize::try_from(config.queues.history_requests).unwrap_or(4),
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
    .expect("spawn history pool")
}

/// What the archive actor did with each journal it received.
#[derive(Debug, Clone, Default)]
pub struct ArchiveRecord {
    pub receipts: Vec<ArchiveReceipt>,
    pub failures: Vec<InfraError>,
    pub stats: ArchiveStats,
    pub status: Option<PersistenceStatus>,
}

#[derive(Debug, Default)]
pub struct ArchiveProbe(Mutex<ArchiveRecord>);

impl ArchiveRecord {
    /// Journals the archive finished (persisted or failed).
    pub fn processed(&self) -> u64 {
        u64::try_from(self.receipts.len() + self.failures.len()).unwrap_or(u64::MAX)
    }
}

impl ArchiveProbe {
    pub fn snapshot(&self) -> ArchiveRecord {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(
        &self,
        result: Result<ArchiveReceipt, InfraError>,
        stats: ArchiveStats,
        status: PersistenceStatus,
    ) {
        let mut record = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match result {
            Ok(receipt) => record.receipts.push(receipt),
            Err(error) => record.failures.push(error),
        }
        record.stats = stats;
        record.status = Some(status);
    }
}

/// Archive actor; each journal first passes `gate` (stuck storage).
pub fn spawn_archive_worker(
    archive: SqliteArchive,
    gate: Gate,
    probe: Arc<ArchiveProbe>,
) -> PoolHandle<ArchiveJournal> {
    spawn_actor(
        "archive",
        ARCHIVE_MAILBOX,
        archive,
        move |archive: &mut SqliteArchive, journal: ArchiveJournal| {
            gate.pass();
            let result = archive.append(journal);
            let stats = archive.stats();
            let status = archive.status();
            probe.record(result, stats, status);
        },
    )
    .expect("spawn archive worker")
}

/// The archive side of the HotStore actor: journals go to the archive actor
/// with `try_dispatch`; a refusal (busy archive) is counted, never waited on.
#[derive(Clone)]
pub struct ArchiveFeed {
    worker: PoolHandle<ArchiveJournal>,
    accepted: Arc<AtomicU64>,
    refused: Arc<AtomicU64>,
}

impl ArchiveFeed {
    pub fn new(worker: PoolHandle<ArchiveJournal>) -> Self {
        Self {
            worker,
            accepted: Arc::new(AtomicU64::new(0)),
            refused: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Journals the archive mailbox accepted.
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Journals the busy archive refused (monitor.rs only logs them).
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::SeqCst)
    }

    fn offer(&self, journal: ArchiveJournal) {
        let counter = match self.worker.try_dispatch(journal) {
            Ok(()) => &self.accepted,
            Err(_) => &self.refused,
        };
        counter.fetch_add(1, Ordering::SeqCst);
    }
}

pub fn spawn_store_worker(
    writer: StateWriter,
    archive: Option<ArchiveFeed>,
    clock: Arc<dyn TimeProvider>,
    completions: Completions,
    archive_bytes: u64,
) -> PoolHandle<StoreJob> {
    spawn_actor(
        "hot-store",
        STORE_MAILBOX,
        writer,
        move |writer: &mut StateWriter, job: StoreJob| {
            if let Some(ack) = run_store_job(writer, job, clock.as_ref()) {
                completions.ingestion(IngestionCompletion::Committed(ack));
            }
            if let Some(archive) = &archive
                && let Some(journal) = writer.take_archive_journal(archive_bytes)
            {
                archive.offer(journal);
            }
        },
    )
    .expect("spawn hot-store worker")
}
