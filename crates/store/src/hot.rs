//! The single-writer hot store (`STORAGE.md` §1, `INTERFACES.md` §8).
//!
//! [`HotStoreWriter`] is owned by the HotStore actor thread. Each commit is
//! validated completely before anything changes (epoch, base revision,
//! values, namespace, units, memory admission), then samples, history,
//! candidate evidence and collector state are applied and exactly one new
//! [`ImmutableSnapshot`] revision is published. Readers get the latest
//! snapshot through [`SharedSnapshot`] and history through [`HistoryIndex`];
//! neither ever holds a mutable handle.

mod archive;
mod counters;
mod dedup;
mod estimate;
mod ingest;
mod retained;
mod settings;
mod snapshot;
mod validate;
mod wall_clock;

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use dgxtop_core::budget::{ByteLedger, BytePermit, Partition};
use dgxtop_core::canonical::{
    AckStatus, ArchiveState, CollectionBatch, CollectorState, CommitGuard, DomainSnapshot,
    GapRecord, ImmutableSnapshot, ProcessTable, StoreAck, StoreHealth,
};
use dgxtop_core::config::{RuntimeConfig, SamplingConfig, StoreConfig};
use dgxtop_core::identity::{CollectorId, MetricKey};
use dgxtop_core::metric::Domain;
use dgxtop_core::ports::{ArchiveJournal, IngressBatch, StoreWriter};
use dgxtop_core::quality::Namespace;
use dgxtop_core::sampling::SamplingTiming;
use uuid::Uuid;

pub use counters::StoreCounters;
pub use snapshot::SharedSnapshot;

use crate::candidates::{CandidateReport, CandidateStore};
use crate::history::output::HistoryReport;
use crate::history::state::{HistoryAppends, HistoryState};
use crate::history::{HistoryIndex, HistoryWriter};
use archive::ArchiveQueue;
use dedup::{REMEMBERED_BATCHES, Recall, RememberedBatches};
use retained::RetainedAllocations;
use settings::StoreSettings;
use snapshot::RecentGaps;
use validate::{CheckedBatch, ContentRules, DescriptorIndex, EpochState, Rejection};
use wall_clock::WallClock;

/// The three handles of one hot store, built together.
///
/// `writer` belongs to the HotStore actor, `snapshots` to presentation and
/// `history` to the history workers.
#[derive(Debug)]
pub struct HotStore {
    pub writer: HotStoreWriter,
    pub snapshots: SharedSnapshot,
    pub history: HistoryIndex,
}

impl HotStore {
    /// Builds an empty store (snapshot revision 0) on the shared ledger.
    ///
    /// `namespace` is the only data namespace accepted (live, or replay in
    /// `--replay`); `archive_enabled` makes the store queue finished minute
    /// buckets, bridges and gaps for [`HotStoreWriter::take_archive_journal`].
    pub fn new(
        store: &StoreConfig,
        sampling: &SamplingConfig,
        runtime: &RuntimeConfig,
        ledger: ByteLedger,
        namespace: Namespace,
        archive_enabled: bool,
    ) -> Self {
        let timing = SamplingTiming::from_sampling(sampling);
        let settings = StoreSettings::new(store, &timing, runtime, namespace, archive_enabled);
        let shared_history = Arc::new(RwLock::new(HistoryState::new(settings.history, &ledger)));
        let candidates = Arc::new(RwLock::new(CandidateStore::new(
            settings.candidates,
            &ledger,
        )));
        let initial = Arc::new(ImmutableSnapshot {
            health: StoreHealth {
                ledger: Some(ledger.snapshot()),
                ..StoreHealth::default()
            },
            ..ImmutableSnapshot::default()
        });
        let mut retained = RetainedAllocations::default();
        if let Ok(permit) = ledger.try_reserve(Partition::HotState, estimate::frame_bytes(0, 0)) {
            retained.track(&initial, permit);
        }
        let snapshots = SharedSnapshot::new(Arc::clone(&initial), Arc::clone(&candidates));
        let history =
            HistoryIndex::new(Arc::clone(&shared_history), ledger.clone(), settings.limits);
        let writer = HotStoreWriter {
            settings,
            timing,
            namespace,
            archive_enabled,
            descriptors: DescriptorIndex::new(),
            shared: snapshots.clone(),
            current: initial,
            key_owners: BTreeMap::new(),
            retained,
            remembered: RememberedBatches::new(&ledger, REMEMBERED_BATCHES),
            epochs: EpochState::default(),
            history: HistoryWriter::new(shared_history),
            candidates,
            archive: ArchiveQueue::new(&ledger),
            clock: WallClock::default(),
            recent_gaps: RecentGaps::new(timing.fast_period_ns),
            counters: StoreCounters::default(),
            series_count: 0,
            sources_retained: 0,
            truncated: false,
            revision: 0,
            last_now_ns: 0,
            ledger,
        };
        Self {
            writer,
            snapshots,
            history,
        }
    }

    /// Splits the store into its writer, snapshot reader and history reader.
    pub fn into_parts(self) -> (HotStoreWriter, SharedSnapshot, HistoryIndex) {
        (self.writer, self.snapshots, self.history)
    }
}

/// Byte permits reserved for a commit before anything is built.
struct Admission {
    domain: BytePermit,
    state: BytePermit,
    process_table: Option<BytePermit>,
    frame: BytePermit,
}

/// A validated, admitted commit that cannot fail any more.
struct PreparedCommit {
    batch_id: Uuid,
    collector_id: CollectorId,
    epoch: u64,
    revision: u64,
    state_revision: u64,
    domain: (DomainSnapshot, BytePermit),
    state: (CollectorState, BytePermit),
    process_table: Option<(ProcessTable, BytePermit)>,
    frame: BytePermit,
    appends: HistoryAppends,
    inputs: Vec<Arc<IngressBatch>>,
    new_keys: Vec<MetricKey>,
}

/// What one publish replaces.
#[derive(Default)]
struct SnapshotChange {
    domains: Vec<(CollectorId, DomainSnapshot, BytePermit)>,
    state: Option<(CollectorId, u64, CollectorState, BytePermit)>,
    process_table: Option<(ProcessTable, BytePermit)>,
}

/// Write side of the hot store (`StoreWriter`), owned by the HotStore actor.
pub struct HotStoreWriter {
    settings: StoreSettings,
    timing: SamplingTiming,
    namespace: Namespace,
    archive_enabled: bool,
    ledger: ByteLedger,
    descriptors: DescriptorIndex,
    shared: SharedSnapshot,
    current: Arc<ImmutableSnapshot>,
    key_owners: BTreeMap<MetricKey, CollectorId>,
    retained: RetainedAllocations,
    remembered: RememberedBatches,
    epochs: EpochState,
    history: HistoryWriter,
    candidates: Arc<RwLock<CandidateStore>>,
    archive: ArchiveQueue,
    clock: WallClock,
    recent_gaps: RecentGaps,
    counters: StoreCounters,
    series_count: u32,
    sources_retained: u32,
    truncated: bool,
    revision: u64,
    last_now_ns: u64,
}

impl std::fmt::Debug for HotStoreWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HotStoreWriter")
            .field("revision", &self.revision)
            .field("namespace", &self.namespace)
            .field("archive_enabled", &self.archive_enabled)
            .finish_non_exhaustive()
    }
}

impl HotStoreWriter {
    /// Tells the store the Manager's current scheduler epoch. Optional: the
    /// commit guard alone already rejects stale epochs; once set, a guard
    /// expecting another epoch is rejected too. Epochs only move forward.
    pub fn set_epoch(&mut self, epoch: u64) {
        self.epochs.announced = Some(
            self.epochs
                .announced
                .map_or(epoch, |current| current.max(epoch)),
        );
    }

    /// Applies a new sampling configuration to future samples only; stored
    /// stale deadlines and totals never change.
    pub fn set_sampling(&mut self, sampling: &SamplingConfig) {
        self.timing = SamplingTiming::from_sampling(sampling);
        self.history
            .set_finalize_grace(self.timing.finalize_grace_ns);
    }

    /// Records the boot-to-UTC relation (call it with every maintenance
    /// tick). A wall clock step becomes a clock discontinuity in history
    /// results; archive rows need at least one observation.
    pub fn observe_wall_clock(&mut self, boot_ns: u64, utc_ns: i128) {
        if let Some(discontinuity) = self.clock.observe(boot_ns, utc_ns) {
            self.history.note_discontinuity(discontinuity);
        }
    }

    /// Finished rows (minute buckets, bridges, their segments, settled gaps)
    /// queued since the last call, at most about `max_bytes`. `None` when the
    /// archive is disabled, nothing is queued, or no UTC mapping is known.
    pub fn take_archive_journal(&mut self, max_bytes: u64) -> Option<ArchiveJournal> {
        if !self.archive_enabled {
            return None;
        }
        self.archive.take(max_bytes, &self.clock, self.revision)
    }

    /// Store counters as of now.
    pub fn counters(&self) -> StoreCounters {
        self.counters
    }

    /// Sheds droppable data under memory pressure (overload `throttled`):
    /// raw candidates beyond half their retention first, then raw history
    /// beyond half its retention (minutes that already have rollups).
    pub fn relieve_pressure(&mut self, now_boot_ns: u64) {
        let now = self.advance_clock(now_boot_ns);
        let shed = self
            .candidates
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .expire_within(now, self.settings.candidates.retention_ns / 2);
        self.counters.absorb_candidates(&CandidateReport {
            evicted: shed,
            ..CandidateReport::default()
        });
        self.refresh_candidate_count();
        let report = self
            .history
            .shed_raw(now, self.settings.history.raw_history_ns / 2);
        self.absorb_history(report);
        self.publish_now(SnapshotChange::default(), now);
    }

    fn advance_clock(&mut self, now_boot_ns: u64) -> u64 {
        self.last_now_ns = self.last_now_ns.max(now_boot_ns);
        self.last_now_ns
    }

    fn committed_state_revision(&self, collector_id: &CollectorId) -> u64 {
        self.current
            .collector_states
            .get(collector_id)
            .map_or(0, |(revision, _)| *revision)
    }

    fn archive_state(&self) -> ArchiveState {
        if self.archive_enabled {
            ArchiveState::Pending
        } else {
            ArchiveState::Disabled
        }
    }

    /// Rejection Ack: nothing changed; it reports the current revision and
    /// the collector's committed state revision for context.
    fn reject(
        &mut self,
        (batch_id, collector_id): (Uuid, CollectorId),
        rejection: Rejection,
    ) -> StoreAck {
        self.counters.count_rejection();
        self.shared.publish_counters(self.counters);
        StoreAck {
            state_revision: self.committed_state_revision(&collector_id),
            batch_id,
            collector_id,
            status: rejection.status,
            committed_revision: self.revision,
            archive: self.archive_state(),
            reason: Some(rejection.reason),
        }
    }

    fn reserve(
        &self,
        partition: Partition,
        bytes: u64,
        what: &str,
    ) -> Result<BytePermit, Rejection> {
        self.ledger
            .try_reserve(partition, bytes)
            .map_err(|error| Rejection::capacity(format!("{what}: {error}")))
    }

    /// Reserves every byte the commit will publish, before building it.
    fn admit(
        &mut self,
        batch: &CollectionBatch,
        previous: Option<&DomainSnapshot>,
    ) -> Result<Admission, Rejection> {
        self.retained.sweep();
        let domain = self.reserve(
            Partition::HotState,
            estimate::domain_upper_bound(previous, batch),
            "hot_state (domain snapshot)",
        )?;
        let state = self.reserve(
            Partition::Catalog,
            estimate::collector_state_bytes(&batch.next_state),
            "catalog (collector state)",
        )?;
        let process_table = batch
            .process_table
            .as_ref()
            .map(|table| {
                self.reserve(
                    Partition::HotState,
                    estimate::process_table_bytes(table),
                    "hot_state (process table)",
                )
            })
            .transpose()?;
        let frame = self.reserve(
            Partition::HotState,
            estimate::frame_bytes(
                self.current.domains.len().saturating_add(1),
                self.current.collector_states.len().saturating_add(1),
            ),
            "hot_state (snapshot frame)",
        )?;
        Ok(Admission {
            domain,
            state,
            process_table,
            frame,
        })
    }

    /// Validates and admits a batch without changing any store state.
    fn prepare(
        &mut self,
        batch: CollectionBatch,
        guard: &CommitGuard,
        now: u64,
    ) -> Result<PreparedCommit, Rejection> {
        validate::check_guard(
            &batch,
            guard,
            &self.epochs,
            self.committed_state_revision(&batch.collector_id),
        )?;
        let rules = ContentRules {
            namespace: self.namespace,
            descriptors: &self.descriptors,
            key_owners: &self.key_owners,
            max_processes: self.settings.max_processes,
            max_process_gpu_links: self.settings.max_process_gpu_links,
        };
        let checked = validate::check_contents(&batch, &rules)?;
        let previous = self.current.domains.get(&batch.collector_id).cloned();
        let admission = self.admit(&batch, previous.as_deref())?;
        Ok(self.build(batch, checked, previous.as_deref(), admission, now))
    }

    /// Builds the commit's structures (memory is already reserved).
    fn build(
        &self,
        batch: CollectionBatch,
        checked: CheckedBatch,
        previous: Option<&DomainSnapshot>,
        admission: Admission,
        now: u64,
    ) -> PreparedCommit {
        let revision = self.revision.saturating_add(1);
        let state_revision = batch.state_base_revision.saturating_add(1);
        let periods = ingest::source_periods(&batch.inputs, &self.timing);
        let appends = ingest::history_appends(&batch, &checked, &periods, &self.timing);
        let CollectionBatch {
            batch_id,
            collector_id,
            cycle_id,
            epoch,
            canonical,
            explanations,
            inputs,
            process_table,
            mut next_state,
            ..
        } = batch;
        let windows = snapshot::StaleWindows {
            periods: &periods,
            stale_factor: self.timing.stale_factor,
            default_period_ns: self.timing.fast_period_ns,
        };
        let domain = snapshot::build_domain(
            previous,
            canonical,
            explanations,
            &windows,
            (revision, cycle_id, now),
        );
        let mut domain_permit = admission.domain;
        domain_permit.shrink_to(estimate::domain_bytes(&domain));
        next_state.revision = state_revision;
        PreparedCommit {
            batch_id,
            collector_id,
            epoch,
            revision,
            state_revision,
            domain: (domain, domain_permit),
            state: (next_state, admission.state),
            process_table: process_table.zip(admission.process_table),
            frame: admission.frame,
            appends,
            inputs,
            new_keys: checked.new_keys,
        }
    }

    /// Applies an admitted commit; cannot fail.
    fn apply(&mut self, prepared: PreparedCommit, now: u64) -> StoreAck {
        let PreparedCommit {
            batch_id,
            collector_id,
            epoch,
            revision,
            state_revision,
            domain,
            state,
            process_table,
            frame,
            appends,
            inputs,
            new_keys,
        } = prepared;
        let report = self.history.apply(appends, now, revision);
        self.counters.count_gaps(report.gaps_added);
        self.absorb_history(report);
        let candidate_report = self
            .candidates
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(&inputs, now);
        drop(inputs);
        self.counters.absorb_candidates(&candidate_report);
        self.refresh_candidate_count();
        for key in new_keys {
            self.key_owners.insert(key, collector_id.clone());
        }
        let change = SnapshotChange {
            domains: vec![(collector_id.clone(), domain.0, domain.1)],
            state: Some((collector_id.clone(), state_revision, state.0, state.1)),
            process_table,
        };
        self.epochs.highest_committed = Some(
            self.epochs
                .highest_committed
                .map_or(epoch, |highest| highest.max(epoch)),
        );
        self.counters.count_commit();
        self.publish(change, frame, revision, now);
        self.shared.publish_counters(self.counters);
        StoreAck {
            batch_id,
            collector_id,
            status: AckStatus::Committed,
            committed_revision: revision,
            state_revision,
            archive: self.archive_state(),
            reason: None,
        }
    }

    /// Folds a history report: drops, evictions, health, recent gaps and
    /// archive rows (gap totals are counted by the caller).
    fn absorb_history(&mut self, report: HistoryReport) {
        self.counters.absorb_history(&report);
        self.series_count = report.series_count;
        self.truncated = report.truncated;
        for gap in &report.new_gaps {
            self.recent_gaps.note(gap.clone());
        }
        if self.archive_enabled {
            let dropped = self.archive.enqueue(report.archive_rows);
            self.counters.count_archive_drops(dropped);
        }
    }

    fn refresh_candidate_count(&mut self) {
        let count = self
            .candidates
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .source_count();
        self.sources_retained = u32::try_from(count).unwrap_or(u32::MAX);
    }

    fn health(&self) -> StoreHealth {
        StoreHealth {
            ledger: Some(self.ledger.snapshot()),
            series: self.series_count,
            sources_retained: self.sources_retained,
            gaps_total: self.counters.gaps_total,
            evicted_total: self.counters.evicted_records_total,
            rejected_batches_total: self.counters.rejected_batches_total,
            truncated_history: self.truncated,
        }
    }

    /// The recent-gap list to publish (re-published only when it changed
    /// and its memory fits).
    fn publish_recent_gaps(&mut self) -> Arc<Vec<GapRecord>> {
        if let Some(gaps) = self.recent_gaps.pending()
            && let Ok(permit) = self
                .ledger
                .try_reserve(Partition::HotState, estimate::gaps_bytes(&gaps))
        {
            let shared = Arc::new(gaps);
            self.retained.track(&shared, permit);
            self.recent_gaps.mark_published(Arc::clone(&shared));
        }
        self.recent_gaps.shared()
    }

    /// Publishes one new snapshot revision. Unchanged domains, states and
    /// the process table are shared with the previous revision.
    fn publish(&mut self, change: SnapshotChange, frame: BytePermit, revision: u64, now: u64) {
        let mut domains = self.current.domains.clone();
        for (collector_id, domain, permit) in change.domains {
            let shared = Arc::new(domain);
            self.retained.track(&shared, permit);
            domains.insert(collector_id, shared);
        }
        let mut collector_states = self.current.collector_states.clone();
        if let Some((collector_id, state_revision, state, permit)) = change.state {
            let shared = Arc::new(state);
            self.retained.track(&shared, permit);
            collector_states.insert(collector_id, (state_revision, shared));
        }
        let process_table = match change.process_table {
            Some((table, permit)) => {
                let shared = Arc::new(table);
                self.retained.track(&shared, permit);
                shared
            }
            None => Arc::clone(&self.current.process_table),
        };
        let recent_gaps = self.publish_recent_gaps();
        let snapshot = Arc::new(ImmutableSnapshot {
            revision,
            committed_boot_ns: now,
            domains,
            process_table,
            collector_states,
            health: self.health(),
            recent_gaps,
        });
        self.retained.track(&snapshot, frame);
        self.current = Arc::clone(&snapshot);
        self.shared.publish(snapshot);
        self.revision = revision;
        self.history.set_revision(revision);
    }

    /// Publishes a revision outside a commit (gaps, maintenance) when its
    /// frame fits; counters are shared either way.
    fn publish_now(&mut self, change: SnapshotChange, now: u64) {
        self.retained.sweep();
        let bytes = estimate::frame_bytes(
            self.current.domains.len(),
            self.current.collector_states.len(),
        );
        if let Ok(frame) = self.ledger.try_reserve(Partition::HotState, bytes) {
            let revision = self.revision.saturating_add(1);
            self.publish(change, frame, revision, now);
        }
        self.shared.publish_counters(self.counters);
    }

    fn key_ttl(&self, key: &MetricKey) -> u64 {
        let process = self
            .descriptors
            .get(key.metric_id.as_str())
            .is_some_and(|descriptor| descriptor.domain == Domain::Process);
        if process {
            self.settings.process_key_ttl_ns
        } else {
            self.settings.key_ttl_ns
        }
    }

    /// Keys of a domain that leave the snapshot: samples not committed for
    /// their TTL and explanations without a sample.
    fn prunable_keys(&self, domain: &DomainSnapshot, now: u64) -> Vec<MetricKey> {
        let idle = domain
            .samples
            .iter()
            .filter(|(key, entry)| {
                entry
                    .sample
                    .committed_boot_ns
                    .saturating_add(self.key_ttl(key))
                    <= now
            })
            .map(|(key, _)| key);
        let orphans = domain
            .explanations
            .keys()
            .filter(|key| !domain.samples.contains_key(*key));
        idle.chain(orphans).cloned().collect()
    }

    /// Removes keys not committed for their TTL (exited processes, removed
    /// devices) so the snapshot stays bounded under churn.
    fn prune_idle_keys(&mut self, now: u64) -> Vec<(CollectorId, DomainSnapshot, BytePermit)> {
        let mut pruned = Vec::new();
        for (collector_id, domain) in &self.current.domains {
            let stale = self.prunable_keys(domain, now);
            if stale.is_empty() {
                continue;
            }
            // Reserved before copying: the copy is never larger than the
            // current domain.
            let Ok(mut permit) = self
                .ledger
                .try_reserve(Partition::HotState, estimate::domain_bytes(domain))
            else {
                continue;
            };
            let mut next = (**domain).clone();
            for key in &stale {
                next.samples.remove(key);
                next.explanations.remove(key);
            }
            permit.shrink_to(estimate::domain_bytes(&next));
            pruned.push((collector_id.clone(), next, permit, stale));
        }
        pruned
            .into_iter()
            .map(|(collector_id, next, permit, stale)| {
                for key in stale {
                    self.key_owners.remove(&key);
                }
                (collector_id, next, permit)
            })
            .collect()
    }
}

impl StoreWriter for HotStoreWriter {
    fn commit_collection(
        &mut self,
        batch: CollectionBatch,
        guard: &CommitGuard,
        now_boot_ns: u64,
    ) -> StoreAck {
        let now = self.advance_clock(now_boot_ns);
        let identity = (batch.batch_id, batch.collector_id.clone());
        let fingerprint = batch.content_fingerprint();
        match self.remembered.recall(batch.batch_id, &fingerprint) {
            Recall::Duplicate(ack) => {
                self.counters.count_duplicate();
                self.shared.publish_counters(self.counters);
                return ack;
            }
            Recall::Conflict(original) => {
                let reason = format!(
                    "batch {} was committed with different content at revision {}",
                    original.batch_id, original.committed_revision
                );
                return self.reject(identity, Rejection::conflict(reason));
            }
            Recall::New => {}
        }
        match self.prepare(batch, guard, now) {
            Ok(prepared) => {
                let ack = self.apply(prepared, now);
                self.remembered
                    .remember(ack.batch_id, fingerprint, ack.clone());
                ack
            }
            Err(rejection) => self.reject(identity, rejection),
        }
    }

    fn record_gap(&mut self, gap: GapRecord) {
        if gap.end_boot_ns <= gap.start_boot_ns {
            self.counters.count_invalid_gap();
            self.shared.publish_counters(self.counters);
            return;
        }
        let report = self.history.record_gap(gap.clone());
        self.counters.count_gaps(report.gaps_added);
        if report.new_gaps.is_empty() {
            // Not stored (no memory left for the index): still counted and listed.
            self.counters.count_gaps(1);
            self.recent_gaps.note(gap);
        }
        self.absorb_history(report);
        let now = self.last_now_ns;
        self.publish_now(SnapshotChange::default(), now);
    }

    fn maintain(&mut self, now_boot_ns: u64) {
        let now = self.advance_clock(now_boot_ns);
        self.retained.sweep();
        let expired = self
            .candidates
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .expire(now);
        self.refresh_candidate_count();
        let report = self.history.maintain(now);
        let visible = report.changed_visible_state() || expired > 0;
        self.counters.count_gaps(report.gaps_added);
        self.absorb_history(report);
        let pruned = self.prune_idle_keys(now);
        if visible || !pruned.is_empty() {
            let change = SnapshotChange {
                domains: pruned,
                ..SnapshotChange::default()
            };
            self.publish_now(change, now);
        } else {
            self.shared.publish_counters(self.counters);
        }
    }
}
