//! Ports between the six components.
//!
//! * Telemetry: `TelemetryPort` (adapters, inside adapter hosts) is read-only.
//! * Action: `ProcessActionPort` is a separate trait; only the Manager's
//!   ProcessService ever receives one.
//! * Process history (D-76): `ProcessProbe` (read-only, beside the
//!   adapters) and `ProcessHistoryStore`; the history's process actor takes
//!   `ProcessHistoryInput`s and publishes through `ProcessHistoryReader` and
//!   `ProcessMemoryReader` (D-79).
//! * Collectors are pure (`Collector::collect`).
//! * State (D-79): `StoreWriter` (HotStore actor only), `SnapshotReader`
//!   and `TrendReader` (the short trends it fills at commit, D-99).
//! * History (D-79): `HistoryIngest`, through which the live state hands
//!   each commit to the history, and `OverviewReader` (D-82), through
//!   which the Manager reads it (D-90); `HistoryTierStore` keeps the
//!   Overview's sizes.
//! * Store: `ConfigRepository`, `AuditRepository`, `ArchiveSink`,
//!   `ArchiveReader` (D-51), `HistoryTierStore` (D-82).
//! * Runtime: `Dispatch<J>` submits jobs to fixed workers without blocking;
//!   results come back as `IngestionCompletion` / `ControlCompletion`.
//! * UI: `SessionPort` (commands, projections, results) and
//!   `TerminalTransport` (non-blocking output owned by the runtime).

use std::collections::BTreeSet;
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
    AuditCompletion, AuditExpiryReport, AuditFailure, CapabilityContext, DurableReceipt,
    ExitObservation, PersistenceStatus, PreparedProcess, RecoveredIntent, Signal, SignalOutcome,
};
use crate::budget::{BytePermit, CapacityError};
use crate::canonical::{
    CollectionBatch, CollectionPlan, CommitGuard, GapRecord, ImmutableSnapshot, StoreAck,
};
use crate::clock::TimeProvider;
use crate::commands::{CommandEnvelope, CommandRejection, CommandResult};
use crate::config::{Config, ConfigError, SamplingConfig, ValidationIssue};
use crate::errors::{ErrorDetail, InfraError};
use crate::identity::{
    AdapterId, CollectorId, EntityId, MetricKey, ProcessKey, SessionId, SourceId,
};
use crate::observation::{
    AdapterManifest, CatalogDelta, DiscoverRequest, ObservationBatch, ReadPlan,
};
use crate::overview_history::{
    ArchivePrune, ArchivedMinutes, ArchivedMinutesRead, ArchivedSeriesList, OverviewAnswer,
    OverviewRequest, StoredTierSeries, TierJournal, TierStoreUsage,
};
use crate::process_history::{
    BucketRequest, ProcessHistoryJournal, ProcessHistoryLoad, ProcessHistoryLoadRequest,
    RecentHistory, RunBucketRow, StoreUsage,
};
use crate::process_model::{MemoryPart, ProcessHistoryModel, ProcessTableSample, RunRequest};
use crate::process_probe::{ProbeReading, ProbeReport, ProbeState, ProbeTarget};
use crate::view::{HostStatus, Trend, ViewModel};

/// Read-only telemetry adapter. Owns its driver handles; returns native values
/// with provenance; never writes stores, never renders, never signals.
pub trait TelemetryPort: Send {
    fn manifest(&self) -> &AdapterManifest;
    fn discover(&mut self, request: &DiscoverRequest) -> CatalogDelta;
    fn read(&mut self, plan: &ReadPlan) -> ObservationBatch;
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

/// Single writer of the live state (HotStore actor only).
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

/// The state's short trends (D-79, D-99): the latest points of the
/// overview's sparkline series, one per new read, filled at commit.
pub trait TrendReader: Send + Sync {
    /// The latest `width` points of `key`, oldest first; empty when the
    /// state keeps none.
    fn trend(&self, key: &MetricKey, width: usize) -> Trend;
    /// Beside each of the latest `width` points of the `memory.used`
    /// series `used`: what the processes of the GPUs `gpus` held (their
    /// `gpu.memory.process_allocated` summed), a gap where any of them had
    /// no value; empty without a GPU.
    fn gpu_memory_beside(&self, used: &MetricKey, gpus: &BTreeSet<EntityId>, width: usize)
    -> Trend;
    /// Beside the same points: what `process` held on `gpus`, a gap where
    /// it held none, from the first point it held some (D-64); empty when
    /// it held none in the points kept.
    fn process_memory_beside(
        &self,
        used: &MetricKey,
        process: &ProcessKey,
        gpus: &BTreeSet<EntityId>,
        width: usize,
    ) -> Trend;
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

/// The Overview History's reads (D-82): the bars of a size and the window
/// statistics of one series, answered from the history's own sizes.
/// Bounded: `TimedOut` past the deadline, `Cancelled` when the flag is
/// set; a series the history does not hold answers with no bars.
pub trait OverviewReader: Send + Sync {
    fn read(
        &self,
        request: &OverviewRequest,
        deadline_boot_ns: u64,
        cancel: &CancelFlag,
        clock: &dyn TimeProvider,
    ) -> Result<OverviewAnswer, ErrorDetail>;
}

/// Everything whose change cuts a history segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentIdentity {
    pub source_id: SourceId,
    pub source_generation: u32,
    pub policy_revision: u32,
    pub sample_kind: crate::quality::SampleKind,
    pub method: crate::history::HistoryMethod,
}

/// One committed gauge value entering history.
#[derive(Debug, Clone)]
pub struct GaugeAppend {
    pub key: MetricKey,
    pub identity: SegmentIdentity,
    pub unit: crate::metric::CanonicalUnit,
    pub time_ns: u64,
    pub stale_deadline_ns: u64,
    pub value: crate::metric::Scalar,
}

/// A history-eligible key committed without a value at `time_ns`.
#[derive(Debug, Clone)]
pub struct CutAppend {
    pub key: MetricKey,
    pub time_ns: u64,
    /// End of the visible gap recorded for the missing value.
    pub gap_end_ns: u64,
    pub reason: String,
}

/// One committed exact counter interval entering history.
#[derive(Debug, Clone)]
pub struct IntervalAppend {
    pub key: MetricKey,
    pub identity: SegmentIdentity,
    pub unit: crate::metric::CanonicalUnit,
    pub start_ns: u64,
    pub end_ns: u64,
    pub delta: crate::metric::Scalar,
}

/// Everything one committed batch adds to history.
#[derive(Debug, Clone, Default)]
pub struct HistoryAppends {
    pub gauges: Vec<GaugeAppend>,
    pub cuts: Vec<CutAppend>,
    pub intervals: Vec<IntervalAppend>,
}

/// What one history mutation means for the live state: the counters, the
/// health and the recent gaps of the snapshot it publishes next.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryOutcome {
    /// Gauge values that did not enter history (any reason).
    pub values_dropped: u64,
    /// Counter intervals that did not enter history (any reason).
    pub intervals_dropped: u64,
    /// New series refused because `store.max_series` series exist.
    pub series_rejected: u64,
    /// Records removed because a partition was full (not normal retention).
    pub evicted_records: u64,
    /// Gaps that became visible (new or extended) in this mutation.
    pub new_gaps: Vec<GapRecord>,
    /// Distinct gap entries created (extensions of a merged gap excluded).
    pub gaps_added: u64,
    pub series_count: u32,
    pub truncated: bool,
    /// Finished rows the archive queue could not hold.
    pub archive_rows_dropped: u64,
}

impl HistoryOutcome {
    /// Whether the mutation changed what a snapshot shows (evictions, gaps).
    pub fn changed_visible_state(&self) -> bool {
        self.evicted_records > 0 || !self.new_gaps.is_empty()
    }
}

/// The history's side of the live state's commits (D-79).
///
/// The state calls it on its own actor thread inside each commit, gap and
/// maintenance step, before it publishes the snapshot, so a committed batch
/// is in history by the time its ack and snapshot are visible (the
/// single-writer atomicity of `StoreWriter` holds across the two crates).
/// The finished rows it queues for the archive are drained on the same
/// thread through [`HistoryIngest::take_archive_journal`].
pub trait HistoryIngest: Send {
    /// Applies one committed batch at snapshot `revision`.
    fn ingest(
        &mut self,
        appends: HistoryAppends,
        now_boot_ns: u64,
        revision: u64,
    ) -> HistoryOutcome;
    /// Records a gap reported from outside the history (Manager or state).
    fn record_gap(&mut self, gap: GapRecord) -> HistoryOutcome;
    /// Retention, finalization, eviction and gap settlement.
    fn maintain(&mut self, now_boot_ns: u64) -> HistoryOutcome;
    /// The snapshot revision queries and journals report; follows every publish.
    fn set_revision(&mut self, revision: u64);
    /// Applies a new finalization horizon to future minutes only.
    fn set_finalize_grace(&mut self, grace_ns: u64);
    /// Records the boot-to-UTC relation; a wall clock step becomes a clock
    /// discontinuity, and archive rows need at least one observation.
    fn observe_wall_clock(&mut self, boot_ns: u64, utc_ns: i128);
    /// Finished rows queued since the last call, at most about `max_bytes`;
    /// `None` when the archive is off, nothing is queued or no UTC mapping is known.
    fn take_archive_journal(&mut self, max_bytes: u64) -> Option<ArchiveJournal>;
}

/// Reader of the history archive (D-51, D-82): the minutes earlier runs
/// and boots left on disk, which the history compresses into the
/// Overview's sizes at startup: which series earlier runs left, and their
/// minutes oldest first, a bounded page at a time. An archive that does not
/// exist yet holds none.
pub trait ArchiveReader: Send {
    fn archived_series(
        &mut self,
        list: &ArchivedSeriesList,
    ) -> Result<Vec<crate::history::SeriesInEveryBoot>, InfraError>;
    fn archived_minutes(
        &mut self,
        read: &ArchivedMinutesRead,
    ) -> Result<ArchivedMinutes, InfraError>;
}

/// The history's own store of the Overview series' buckets of every size
/// (D-82): it executes what the history asks, nothing more.
pub trait HistoryTierStore: Send {
    /// Every series' kept buckets, read once at startup.
    fn load(&mut self) -> Result<Vec<StoredTierSeries>, InfraError>;
    /// Deletes the series the journal forgets, writes closed buckets and
    /// keeps the latest of each (series, size); returns what it holds then.
    fn append(&mut self, journal: &TierJournal) -> Result<TierStoreUsage, InfraError>;
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
    /// Size of the committed file (the Settings view shows it beside the
    /// limit, UI.md §6).
    pub file_bytes: u64,
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
    /// Deletes the rows older than `before_utc` (RFC 3339 UTC), bounded per
    /// call. The only way rows leave the audit: its owner decides the age
    /// and when (D-96).
    fn expire(&mut self, before_utc: &str) -> Result<AuditExpiryReport, AuditFailure>;
    /// A read: never deletes or changes a row.
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
    /// Minutes the history's compression holds in its coarser sizes, to be
    /// deleted (D-82).
    pub prunes: Vec<ArchivePrune>,
    /// Rows the history no longer keeps by age, to be deleted (D-96).
    pub expire: Option<ArchiveExpiry>,
    /// What the archive does when this journal does not fit its budget
    /// (D-96).
    pub when_full: ArchiveWhenFull,
    pub permit: Option<BytePermit>,
}

/// Archive rows past the age the history keeps them (D-96): the store
/// deletes exactly these, then the rows that only existed for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveExpiry {
    /// Minutes that ended before this UTC time (and the segments left
    /// without minutes that started before it).
    pub before_utc_ns: i64,
    /// The current boot's gaps that ended before this boot time.
    pub current_boot_before_ns: u64,
}

/// What the archive does with a journal its budget cannot hold (D-96).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveWhenFull {
    /// Delete the oldest whole minutes until the journal fits.
    EvictOldest,
    /// Refuse the journal (`DiskFull`); nothing is deleted.
    Refuse,
}

/// Archive commit receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveReceipt {
    pub revision: u64,
    pub rows: u64,
    /// The deletions the journal asked for failed after its rows were
    /// committed: the rows stay durable, the deletions wait for the next
    /// request (D-96).
    pub deletion_failure: Option<InfraError>,
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
/// and their buckets of a minute and more, and a short-term file of the
/// live runs' recent buckets. They keep what they are told and read what
/// they are asked; what stays is the history's decision (D-94).
pub trait ProcessHistoryStore: Send {
    /// What both stores kept, read at startup, as `request` asks.
    fn load(
        &mut self,
        request: &ProcessHistoryLoadRequest,
    ) -> Result<ProcessHistoryLoad, InfraError>;
    /// Writes the journal and says what the store holds then.
    fn append(&mut self, journal: &ProcessHistoryJournal) -> Result<StoreUsage, InfraError>;
    /// Replaces the short-term file.
    fn save_recent(&mut self, recent: &RecentHistory) -> Result<(), InfraError>;
    /// One run's buckets, for a run whose buckets are not in memory.
    fn read_run(
        &mut self,
        key: &ProcessKey,
        buckets: &BucketRequest,
    ) -> Result<Vec<RunBucketRow>, InfraError>;
}

/// Process probe work (D-76).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessProbeJob {
    pub targets: Vec<ProbeTarget>,
}

/// Process history store work (D-76).
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessHistoryJob {
    Load(ProcessHistoryLoadRequest),
    Append(ProcessHistoryJournal),
    SaveRecent(RecentHistory),
    ReadRun {
        key: ProcessKey,
        buckets: BucketRequest,
    },
}

/// What became of a [`ProcessHistoryJob`].
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessHistoryCompletion {
    Loaded(Result<ProcessHistoryLoad, InfraError>),
    Appended(Result<StoreUsage, InfraError>),
    Saved(Result<(), InfraError>),
    RunRead {
        key: ProcessKey,
        result: Result<Vec<RunBucketRow>, InfraError>,
    },
}

/// What the history's process actor takes (D-79): from the Manager the
/// committed tables, the commits between them, the signals dgxtop sent and
/// the runs sessions show; from the probe and the stores what they did.
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessHistoryInput {
    /// A committed process table new since the last one handed over.
    Table(Box<ProcessTableSample>),
    /// A commit without a new table: a finished probe is followed by the
    /// next one, and the writes leave when due.
    Tick,
    Probe(ProbeReport),
    Store(ProcessHistoryCompletion),
    /// dgxtop sent `signal` to `key` (D-71).
    Signal {
        key: ProcessKey,
        signal: Signal,
    },
    /// A session shows this run.
    Show(RunRequest),
    /// At shutdown: whatever is left goes to the stores.
    Flush,
}

/// The read model of the GPU process runs and colors (D-79), published by
/// the history's process actor.
pub trait ProcessHistoryReader: Send + Sync {
    fn latest(&self) -> Arc<ProcessHistoryModel>;
}

/// What each tracked GPU process held over time (D-64), for the parts of
/// the history's memory charts.
pub trait ProcessMemoryReader: Send + Sync {
    /// Each tracked process's part of a chart of the GPUs in `gpus`, in
    /// each of `cells` even cells of `window` (wall clock; a chart asks
    /// for its bars, D-97), in color order; a process that held nothing
    /// there is left out.
    fn parts(&self, gpus: &BTreeSet<EntityId>, window: (u64, u64), cells: usize)
    -> Vec<MemoryPart>;
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
    /// A durable sampling change: future samples use its periods and the
    /// history its finalize grace (stored deadlines never change).
    ApplySampling(SamplingConfig),
}

/// One Overview History read of a session's history view (D-82).
#[derive(Debug, Clone)]
pub struct HistoryJob {
    pub request_id: Uuid,
    pub request: OverviewRequest,
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
    /// An Overview History read for a session's history view (D-82).
    History {
        request_id: Uuid,
        session_id: SessionId,
        session_generation: u64,
        result: Result<OverviewAnswer, ErrorDetail>,
    },
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
