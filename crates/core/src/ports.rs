//! Ports between the six components.
//!
//! * Telemetry: `TelemetryPort` (adapters, inside adapter hosts) is read-only.
//! * Action: `ProcessActionPort` is a separate trait; only the Manager's
//!   ProcessService ever receives one.
//! * Process history (D-76): `ProcessProbe` (read-only, beside the
//!   adapters) and `ProcessHistoryStore`.
//! * Collectors are pure (`Collector::collect`).
//! * Store: `StoreWriter` (HotStore actor only), `SnapshotReader`,
//!   `HistoryReader`, `ConfigRepository`, `AuditRepository`, `ArchiveSink`,
//!   `ArchiveReader` (D-51).
//! * Runtime: `Dispatch<J>` submits jobs to fixed workers without blocking;
//!   results come back as `IngestionCompletion` / `ControlCompletion`.
//! * UI: `SessionPort` (commands, projections, results) and
//!   `TerminalTransport` (non-blocking output owned by the runtime).

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::action::{
    ActionAttempt, ActionCompletion, ActionError, ActionHandle, ActionIntent, ActionOutcomeRecord,
    ActionRecordSummary, AuditCompletion, AuditFailure, CapabilityContext, DurableReceipt,
    ExitObservation, PersistenceStatus, PreparedProcess, RecoveredIntent, Signal, SignalOutcome,
};
use crate::budget::{BytePermit, CapacityError};
use crate::canonical::{
    CollectionBatch, CollectionPlan, CommitGuard, GapRecord, ImmutableSnapshot, StoreAck,
};
use crate::clock::TimeProvider;
use crate::commands::{CommandEnvelope, CommandRejection, CommandResult};
use crate::config::{Config, ConfigError, ValidationIssue};
use crate::errors::{ErrorDetail, InfraError};
use crate::history::{ArchiveQuery, ArchiveResult, HistoryQuery, HistoryResult};
use crate::identity::{AdapterId, CollectorId, MetricKey, ProcessKey, SessionId, SourceId};
use crate::observation::{
    AdapterHealth, AdapterManifest, CatalogDelta, DiscoverRequest, ObservationBatch, ReadPlan,
};
use crate::process_history::{
    ProcessHistoryJournal, ProcessHistoryLoad, RecentHistory, RunBucketRow,
};
use crate::process_probe::{ProbeReading, ProbeReport, ProbeState, ProbeTarget};
use crate::view::{HostStatus, ViewModel};

/// Read-only telemetry adapter. Owns its driver handles; returns native values
/// with provenance; never writes stores, never renders, never signals.
pub trait TelemetryPort: Send {
    fn manifest(&self) -> &AdapterManifest;
    fn discover(&mut self, request: &DiscoverRequest) -> CatalogDelta;
    fn read(&mut self, plan: &ReadPlan) -> ObservationBatch;
    fn health(&self) -> AdapterHealth;
    fn shutdown(&mut self);
}

/// Pure domain collector: normalization, validation, origin de-duplication,
/// arbitration and derivation over bounded committed state.
pub trait Collector: Send + Sync {
    fn id(&self) -> &CollectorId;
    /// Whether batches of `(adapter_id, read_group)` feed this collector.
    /// `read_group` is the base group name (page suffix removed).
    fn consumes(&self, adapter_id: &AdapterId, read_group: &str) -> bool;
    fn collect(&self, plan: &CollectionPlan) -> CollectionBatch;
}

/// Single writer of the hot store (HotStore actor only).
pub trait StoreWriter: Send {
    /// Validates epoch/base/namespace/units, then commits samples, history and
    /// collector state atomically, or rejects the whole batch.
    fn commit_collection(
        &mut self,
        batch: CollectionBatch,
        guard: &CommitGuard,
        now_boot_ns: u64,
    ) -> StoreAck;
    fn record_gap(&mut self, gap: GapRecord);
    /// Retention and quota enforcement (minute rollover, eviction).
    fn maintain(&mut self, now_boot_ns: u64);
}

/// Cheap access to the latest immutable snapshot.
pub trait SnapshotReader: Send + Sync {
    fn latest(&self) -> Arc<ImmutableSnapshot>;
}

/// Cooperative cancellation flag shared with a running job.
#[derive(Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl fmt::Debug for CancelFlag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "CancelFlag({})", self.is_cancelled())
    }
}

/// Bounded history queries over immutable chunks.
pub trait HistoryReader: Send + Sync {
    /// Must return `QueryTooLarge` instead of scanning more than the
    /// configured records, `TimedOut` past the deadline and `Cancelled` when
    /// the flag is set.
    fn query(
        &self,
        query: &HistoryQuery,
        deadline_boot_ns: u64,
        cancel: &CancelFlag,
        clock: &dyn TimeProvider,
    ) -> Result<HistoryResult, ErrorDetail>;

    /// Series currently held (for the history module's series picker).
    fn series(&self) -> Vec<MetricKey>;
}

/// Reader of the history archive (D-51): the minutes earlier runs and
/// boots left on disk, for the history view. Bounded like the history
/// reader: `TimedOut` past the deadline, `Cancelled` when the flag is set; an
/// archive that does not exist yet answers with no points.
pub trait ArchiveReader: Send {
    fn read(
        &mut self,
        query: &ArchiveQuery,
        deadline_boot_ns: u64,
        cancel: &CancelFlag,
        clock: &dyn TimeProvider,
    ) -> Result<ArchiveResult, ErrorDetail>;
}

/// Where the effective configuration came from at load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoadOrigin {
    /// No file existed; built-in defaults (not written until a commit).
    Defaults,
    File {
        path: String,
        sha256: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedConfig {
    pub config: Config,
    pub origin: LoadOrigin,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ConfigLoadError {
    #[error("{path}: {error}")]
    Invalid { path: String, error: ConfigError },
    #[error("{path}: schema_version {found} is newer than supported; not rewritten")]
    FutureSchema { path: String, found: u64 },
    #[error("unsafe config path: {0}")]
    UnsafePath(String),
    #[error(transparent)]
    Io(#[from] InfraError),
}

/// Successful durable configuration commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistReceipt {
    pub revision: u32,
    pub sha256: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum PersistError {
    #[error("revision conflict: current revision is {current}")]
    Conflict { current: u32 },
    #[error("candidate invalid")]
    Invalid(Vec<ValidationIssue>),
    /// Failed before the rename: the previous file is intact.
    #[error(transparent)]
    Io(InfraError),
    /// The rename happened but durability is unknown (e.g. directory fsync
    /// failed); the repository reloaded what is on disk.
    #[error("commit outcome uncertain: {message}")]
    Uncertain {
        message: String,
        on_disk: Option<Box<Config>>,
    },
}

/// Versioned atomic JSON configuration store.
pub trait ConfigRepository: Send {
    fn load(&mut self) -> Result<LoadedConfig, ConfigLoadError>;
    /// CAS on `revision`: stage -> temp 0600 -> fsync -> rename -> dir fsync.
    /// The candidate's revision must equal `expected_revision`; the stored
    /// document gets `expected_revision + 1`.
    fn compare_and_swap(
        &mut self,
        expected_revision: u32,
        candidate: &Config,
    ) -> Result<PersistReceipt, PersistError>;
    fn status(&self) -> PersistenceStatus;
}

/// Action audit store (SQLite WAL, synchronous=FULL).
pub trait AuditRepository: Send {
    fn append_intent(&mut self, intent: &ActionIntent) -> Result<DurableReceipt, AuditFailure>;
    fn append_outcome(&mut self, outcome: &ActionOutcomeRecord) -> Result<(), AuditFailure>;
    fn append_attempt(&mut self, attempt: &ActionAttempt) -> Result<(), AuditFailure>;
    /// Marks intents without a provable outcome as `unknown` (never resent).
    fn recover_unknown(
        &mut self,
        now_boot_ns: u64,
        now_utc: &str,
    ) -> Result<Vec<RecoveredIntent>, AuditFailure>;
    fn lookup(&mut self, request_id: Uuid) -> Result<Option<ActionRecordSummary>, AuditFailure>;
    fn status(&mut self) -> PersistenceStatus;
}

/// One archived segment (`history.sql` `history_segments`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchivedSegment {
    pub segment_id: String,
    pub key: MetricKey,
    pub source_id: SourceId,
    pub source_generation: u32,
    pub policy_revision: u32,
    pub unit: crate::metric::CanonicalUnit,
    pub sample_kind: crate::quality::SampleKind,
    pub started_utc: String,
}

/// One finished minute bucket (`history_buckets`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchivedBucket {
    pub segment_id: String,
    pub start_boot_ns: u64,
    pub end_boot_ns: u64,
    pub start_utc: String,
    pub end_utc: String,
    pub covered_ns: u64,
    pub weighted_sum: Option<f64>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    /// Exact counter total inside the bucket (u128 decimal text in SQL).
    pub counter_delta: Option<u128>,
    pub sample_count: u32,
    pub method: crate::history::HistoryMethod,
    pub reason_code: Option<String>,
}

/// One cross-bucket counter interval (`counter_bridges`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedBridge {
    pub segment_id: String,
    pub start_boot_ns: u64,
    pub end_boot_ns: u64,
    pub delta: u128,
}

/// Bounded archive journal copy of one revision (≤ `queues.archive_bytes`).
#[derive(Debug)]
pub struct ArchiveJournal {
    pub revision: u64,
    pub host_id: String,
    pub boot_id: String,
    pub segments: Vec<ArchivedSegment>,
    pub buckets: Vec<ArchivedBucket>,
    pub bridges: Vec<ArchivedBridge>,
    pub gaps: Vec<GapRecord>,
    pub permit: Option<BytePermit>,
}

/// Archive commit receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveReceipt {
    pub revision: u64,
    pub rows: u64,
}

/// Optional history archive (SQLite WAL, synchronous=NORMAL, off by default).
pub trait ArchiveSink: Send {
    fn append(&mut self, journal: ArchiveJournal) -> Result<ArchiveReceipt, InfraError>;
    fn status(&mut self) -> PersistenceStatus;
}

/// What became of one archive journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveOutcome {
    /// The journal's rows are durable in the archive.
    Committed { rows: u64 },
    /// The journal was not archived: those rows are an archive gap.
    Gap {
        cause: ArchiveGapCause,
        detail: String,
    },
}

/// Bounded cause of an archive gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveGapCause {
    /// The archive worker's mailbox was full; the journal was dropped.
    WorkerBusy,
    /// The archive refused or failed the write (disk low, WAL cap, I/O).
    WriteFailed,
}

impl ArchiveGapCause {
    /// Bounded diagnostics label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::WorkerBusy => "worker_busy",
            Self::WriteFailed => "write_failed",
        }
    }
}

/// The only interface able to signal user processes. Never implemented by a
/// telemetry adapter; never visible to collectors or the viewer.
pub trait ProcessActionPort: Send {
    /// Opens a pidfd (flags 0), re-verifies identity, uid, namespace and
    /// liveness against `key`, and keeps the descriptor for the confirmation.
    fn prepare(
        &mut self,
        key: &ProcessKey,
        signal: Signal,
        context: &CapabilityContext,
    ) -> Result<PreparedProcess, ActionError>;
    /// Exactly one `pidfd_send_signal` on the held descriptor. No PID fallback.
    fn send(
        &mut self,
        handle: ActionHandle,
        signal: Signal,
        receipt: &DurableReceipt,
    ) -> SignalOutcome;
    /// Non-blocking readiness check of the held pidfd.
    fn observe_exit(&mut self, handle: ActionHandle) -> ExitObservation;
    /// Closes the held descriptor.
    fn release(&mut self, handle: ActionHandle);
}

/// Reads what the process history follows of a few GPU processes (D-76):
/// liveness, how a zombie ended, IO counters and, when asked, the command
/// line and working folder. Read-only: it never signals.
pub trait ProcessProbe: Send {
    fn probe(&mut self, targets: &[ProbeTarget]) -> ProbeReport;
}

/// Turns a probe reading of a running process into the values its run's
/// samples take (D-79: the process collector's transformation, kept apart
/// from the process history that follows the runs). Pure: `previous` is
/// what the last reading of the same process left; the result is the
/// state the next reading starts from.
pub trait ProbeDeriver: Send + Sync {
    /// `read_boot_ns` is when the probe read `reading`.
    fn derive(
        &self,
        previous: &ProbeState,
        reading: &ProbeReading,
        read_boot_ns: u64,
    ) -> ProbeState;
}

/// The stores of the process history (D-76): a long-term database of runs
/// and their minute and ten-minute buckets, and a short-term file of the
/// live runs' recent buckets.
pub trait ProcessHistoryStore: Send {
    /// What both stores kept, read at startup.
    fn load(&mut self) -> Result<ProcessHistoryLoad, InfraError>;
    fn append(&mut self, journal: &ProcessHistoryJournal) -> Result<(), InfraError>;
    /// Replaces the short-term file.
    fn save_recent(&mut self, recent: &RecentHistory) -> Result<(), InfraError>;
    /// One run's buckets, for a run whose buckets are not in memory.
    fn read_run(&mut self, key: &ProcessKey) -> Result<Vec<RunBucketRow>, InfraError>;
}

/// Process probe work (D-76).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessProbeJob {
    pub targets: Vec<ProbeTarget>,
}

/// Process history store work (D-76).
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessHistoryJob {
    Load,
    Append(ProcessHistoryJournal),
    SaveRecent(RecentHistory),
    ReadRun(ProcessKey),
}

/// What became of a [`ProcessHistoryJob`].
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessHistoryCompletion {
    Loaded(Result<ProcessHistoryLoad, InfraError>),
    Appended(Result<(), InfraError>),
    Saved(Result<(), InfraError>),
    RunRead {
        key: ProcessKey,
        result: Result<Vec<RunBucketRow>, InfraError>,
    },
}

/// Non-blocking job submission to a fixed worker.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DispatchError {
    #[error("worker queue full")]
    Busy,
    #[error("worker stopped")]
    Closed,
    #[error(transparent)]
    Capacity(#[from] CapacityError),
}

/// Submits `J` to a bounded worker; never blocks the caller.
pub trait Dispatch<J>: Send {
    fn try_dispatch(&self, job: J) -> Result<(), DispatchError>;
}

/// Adapter host work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterJob {
    Read(ReadPlan),
    Discover(DiscoverRequest),
}

/// Store actor work.
#[derive(Debug)]
pub enum StoreJob {
    Commit {
        batch: Box<CollectionBatch>,
        guard: CommitGuard,
    },
    Gap(GapRecord),
    Maintain,
}

/// One history query job.
#[derive(Debug, Clone)]
pub struct HistoryJob {
    pub query: HistoryQuery,
    pub session_id: SessionId,
    /// Session generation; results for an older generation are dropped.
    pub session_generation: u64,
    pub deadline_boot_ns: u64,
    pub cancel: CancelFlag,
}

/// One archive read job for a session's history view (D-51).
#[derive(Debug, Clone)]
pub struct ArchiveReadJob {
    pub query: ArchiveQuery,
    pub session_id: SessionId,
    /// Session generation; results for an older generation are dropped.
    pub session_generation: u64,
    pub deadline_boot_ns: u64,
    pub cancel: CancelFlag,
}

/// Config worker work.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigJob {
    Persist {
        request_id: Uuid,
        expected_revision: u32,
        candidate: Box<Config>,
    },
}

/// Ingress batch plus the byte permit that keeps it accounted while any
/// pipeline stage still references it.
#[derive(Debug)]
pub struct IngressBatch {
    batch: ObservationBatch,
    permit: Option<BytePermit>,
}

impl IngressBatch {
    pub fn new(batch: ObservationBatch, permit: Option<BytePermit>) -> Self {
        Self { batch, permit }
    }

    pub fn batch(&self) -> &ObservationBatch {
        &self.batch
    }

    pub fn accounted_bytes(&self) -> u64 {
        self.permit.as_ref().map_or(0, BytePermit::bytes)
    }
}

impl Deref for IngressBatch {
    type Target = ObservationBatch;
    fn deref(&self) -> &ObservationBatch {
        &self.batch
    }
}

/// Why an adapter request produced no batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterFailureKind {
    TimedOut,
    HostUnavailable,
    Protocol,
    CapacityExceeded,
    Quarantined,
    CircuitOpen,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterFailure {
    pub kind: AdapterFailureKind,
    pub message: String,
}

/// Data-path completions delivered to the Manager.
#[derive(Debug)]
pub enum IngestionCompletion {
    Batch {
        adapter_id: AdapterId,
        request_id: Uuid,
        host_generation: u32,
        result: Result<Arc<IngressBatch>, AdapterFailure>,
    },
    Catalog {
        adapter_id: AdapterId,
        request_id: Uuid,
        host_generation: u32,
        result: Result<CatalogDelta, AdapterFailure>,
    },
    Collected(Box<CollectionBatch>),
    CollectorFailed {
        collector_id: CollectorId,
        batch_id: Uuid,
        message: String,
    },
    Committed(StoreAck),
    HostStatus(HostStatus),
}

/// Control-path completions delivered to the Manager (never droppable).
#[derive(Debug)]
pub enum ControlCompletion {
    Config {
        request_id: Uuid,
        result: Result<PersistReceipt, PersistError>,
    },
    Audit(AuditCompletion),
    Action(ActionCompletion),
    /// Fate of the archive journal of `revision` (STORAGE.md §1: the store
    /// acknowledgement only says "archive pending"); `status` is the archive
    /// partition after the attempt, `None` when its worker never saw it.
    Archive {
        revision: u64,
        outcome: ArchiveOutcome,
        status: Option<PersistenceStatus>,
    },
    History {
        request_id: Uuid,
        session_id: SessionId,
        session_generation: u64,
        result: Result<HistoryResult, ErrorDetail>,
    },
    /// An archive read for a session's history view (D-51).
    ArchiveRead {
        request_id: Uuid,
        session_id: SessionId,
        session_generation: u64,
        result: Result<ArchiveResult, ErrorDetail>,
    },
    /// What the process probe read (D-76).
    ProcessProbe(ProbeReport),
    /// What a process history store job did (D-76).
    ProcessHistory(ProcessHistoryCompletion),
}

/// UI-facing session handle implemented by the Manager.
pub trait SessionPort: Send {
    fn session_id(&self) -> &SessionId;
    /// Non-blocking: `Ok` means accepted (a result slot is reserved), never
    /// "completed"; a full queue is rejected immediately with `Busy`.
    fn submit(&self, command: CommandEnvelope) -> Result<(), CommandRejection>;
    /// Latest pending projection (at most one is ever pending).
    fn take_projection(&self) -> Option<Arc<ViewModel>>;
    /// Command results delivered independently of projections.
    fn drain_results(&self, max: usize) -> Vec<CommandResult>;
    /// Out-of-band shutdown request; does not queue behind normal commands.
    fn request_shutdown(&self, reason: &str);
    /// The Manager asked every frontend to stop.
    fn shutdown_requested(&self) -> bool;
    /// Pure viewport change (affects projection windowing only).
    fn set_viewport(&self, columns: u16, rows: u16);
}

/// Non-blocking terminal output, implemented in the runtime with O_NONBLOCK
/// and poll(POLLOUT); restores the original file flags.
pub trait TerminalTransport: Send {
    /// Writes what the terminal accepts now; `Ok(0)` means it would block.
    fn write_nonblocking(&mut self, bytes: &[u8]) -> std::io::Result<usize>;
    /// Waits up to `timeout` for writability.
    fn wait_writable(&mut self, timeout: Duration) -> std::io::Result<bool>;
    /// Restores the original descriptor flags (idempotent).
    fn restore(&mut self) -> std::io::Result<()>;
}

/// Readiness of the terminal input device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputReadiness {
    Readable,
    TimedOut,
    /// The terminal hung up (or errored); no further input will arrive.
    HungUp,
}

/// Waits for terminal input without depending on the input library's own
/// poll loop (which may spin on a hung-up terminal). Implemented in the
/// runtime with poll(2) on the controlling terminal.
pub trait InputGate: Send {
    /// Waits until input is pending, the terminal hangs up, or `timeout`
    /// passes.
    fn wait(&mut self, timeout: Duration) -> std::io::Result<InputReadiness>;

    /// Waits until the terminal hangs up (`Ok(true)`) or `timeout` passes,
    /// ignoring pending input: bytes the input library left unread must not
    /// turn its wait into a busy loop.
    fn wait_for_hang_up(&mut self, timeout: Duration) -> std::io::Result<bool>;
}

/// Log severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

/// Bounded, rate-limited logger; implementations must never block callers.
pub trait Logger: Send + Sync {
    fn log(&self, level: LogLevel, target: &'static str, message: &str);
}

/// Logger that discards everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullLogger;

impl Logger for NullLogger {
    fn log(&self, _level: LogLevel, _target: &'static str, _message: &str) {}
}
