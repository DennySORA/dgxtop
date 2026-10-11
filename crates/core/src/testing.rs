//! Deterministic test doubles shared by every crate's unit tests
//! (`dgxtop-core` feature `test-support`). Nothing here touches real drivers,
//! files or the network.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use uuid::Uuid;

use crate::canonical::{
    CanonicalSample, CatalogSnapshot, DomainSnapshot, GpuUsageKind, ImmutableSnapshot,
    ProcessGpuLink, ProcessRow, ProcessTable, SnapshotEntry,
};
use crate::clock::TimeProvider;
use crate::identity::{
    AdapterId, BootId, CollectorId, EntityId, HostId, MetricId, MetricKey, OriginId,
    OriginRelation, Pid, PidNamespace, ProcessKey, ProfileId, ReadGroup, Scope, SourceId,
};
use crate::metric::{CanonicalUnit, NativeUnit, Scalar};
use crate::observation::{
    Attribute, BatchStatus, EntityKind, EntityRecord, IPC_PROTOCOL_VERSION, Observation,
    ObservationBatch, Provenance,
};
use crate::ports::{
    Dispatch, DispatchError, LogLevel, Logger, ProbeDeriver, ProcessHistoryReader,
    ProcessMemoryReader,
};
use crate::process_model::{MemoryPart, ProcessHistoryModel, ProcessTableSample};
use crate::process_probe::{ProbeReading, ProbeState};
use crate::quality::{Freshness, Quality, ReadStatus, SampleKind, SelectionState};

/// UTC on a whole minute (2026-09-21T14:14:00Z), in nanoseconds. Boot 0
/// paired with it puts the history's minutes, which are cut on whole UTC
/// minutes (D-82), on whole minutes of the boot clock; tests and the
/// synthetic history preload count with that.
pub const UTC_ON_A_WHOLE_MINUTE_NS: i128 = 1_790_000_040_000_000_000;

/// Manually advanced clock. UTC can be stepped independently of boot time.
#[derive(Debug)]
pub struct FakeClock {
    boot_id: Mutex<BootId>,
    boot_ns: AtomicU64,
    utc_ns: AtomicI64,
}

impl FakeClock {
    pub fn new(boot_ns: u64) -> Arc<Self> {
        Arc::new(Self {
            boot_id: Mutex::new(boot_id("fake-boot")),
            boot_ns: AtomicU64::new(boot_ns),
            utc_ns: AtomicI64::new(1_790_000_000_000_000_000),
        })
    }

    pub fn advance_ms(&self, milliseconds: u64) {
        self.advance_ns(milliseconds.saturating_mul(1_000_000));
    }

    pub fn advance_ns(&self, nanoseconds: u64) {
        self.boot_ns.fetch_add(nanoseconds, Ordering::SeqCst);
        let step = i64::try_from(nanoseconds).unwrap_or(i64::MAX);
        self.utc_ns.fetch_add(step, Ordering::SeqCst);
    }

    pub fn set_boot_ns(&self, boot_ns: u64) {
        self.boot_ns.store(boot_ns, Ordering::SeqCst);
    }

    /// Steps only the wall clock (e.g. -24 h); boot time is unaffected.
    pub fn step_utc_ns(&self, delta: i64) {
        self.utc_ns.fetch_add(delta, Ordering::SeqCst);
    }

    pub fn set_boot_id(&self, id: &str) {
        *self.boot_id.lock().unwrap_or_else(PoisonError::into_inner) = boot_id(id);
    }
}

impl TimeProvider for FakeClock {
    fn boot_id(&self) -> BootId {
        self.boot_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn now_boot_ns(&self) -> u64 {
        self.boot_ns.load(Ordering::SeqCst)
    }

    fn now_utc_ns(&self) -> i128 {
        i128::from(self.utc_ns.load(Ordering::SeqCst))
    }
}

/// Records submitted jobs; can be switched to reject with `Busy`.
#[derive(Debug)]
pub struct CapturingDispatch<J> {
    jobs: Arc<Mutex<Vec<J>>>,
    busy: Arc<std::sync::atomic::AtomicBool>,
    refused: Arc<std::sync::atomic::AtomicUsize>,
}

impl<J> Clone for CapturingDispatch<J> {
    fn clone(&self) -> Self {
        Self {
            jobs: Arc::clone(&self.jobs),
            busy: Arc::clone(&self.busy),
            refused: Arc::clone(&self.refused),
        }
    }
}

impl<J> Default for CapturingDispatch<J> {
    fn default() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(Vec::new())),
            busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            refused: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl<J> CapturingDispatch<J> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::SeqCst);
    }

    /// Jobs refused while busy.
    pub fn refused(&self) -> usize {
        self.refused.load(Ordering::SeqCst)
    }

    /// Removes and returns every captured job.
    pub fn take(&self) -> Vec<J> {
        std::mem::take(&mut *self.jobs.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn len(&self) -> usize {
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<J: Send> Dispatch<J> for CapturingDispatch<J> {
    fn try_dispatch(&self, job: J) -> Result<(), DispatchError> {
        if self.busy.load(Ordering::SeqCst) {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return Err(DispatchError::Busy);
        }
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(job);
        Ok(())
    }
}

/// Logger capturing records for assertions.
#[derive(Debug, Default)]
pub struct MemoryLogger {
    records: Mutex<Vec<(LogLevel, &'static str, String)>>,
}

impl MemoryLogger {
    pub fn records(&self) -> Vec<(LogLevel, &'static str, String)> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Logger for MemoryLogger {
    fn log(&self, level: LogLevel, target: &'static str, message: &str) {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((level, target, message.to_owned()));
    }
}

/// Probe deriver of a setup without a live probe: every reading leaves the
/// state as it was.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdleProbeDeriver;

impl ProbeDeriver for IdleProbeDeriver {
    fn derive(
        &self,
        previous: &ProbeState,
        _reading: &ProbeReading,
        _read_boot_ns: u64,
    ) -> ProbeState {
        *previous
    }
}

/// The history's process actor of a setup that publishes a fixed model
/// and fixed memory parts (D-79): the presentation's and the Manager's
/// tests shape what it holds without following any run.
#[derive(Debug, Default)]
pub struct StaticProcessHistory {
    model: Mutex<Arc<ProcessHistoryModel>>,
    parts: Mutex<Vec<MemoryPart>>,
    /// What the parts were asked for, in order.
    asked: Mutex<Vec<PartsAsked>>,
}

/// The GPUs, window and cells a chart's parts were asked for.
pub type PartsAsked = (BTreeSet<EntityId>, (u64, u64), usize);

impl StaticProcessHistory {
    pub fn new(model: ProcessHistoryModel) -> Arc<Self> {
        let history = Self::default();
        history.publish(model);
        Arc::new(history)
    }

    pub fn publish(&self, model: ProcessHistoryModel) {
        *self.model.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(model);
    }

    /// The parts every chart gets.
    pub fn set_parts(&self, parts: Vec<MemoryPart>) {
        *self.parts.lock().unwrap_or_else(PoisonError::into_inner) = parts;
    }

    pub fn asked(&self) -> Vec<PartsAsked> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl ProcessHistoryReader for StaticProcessHistory {
    fn latest(&self) -> Arc<ProcessHistoryModel> {
        Arc::clone(&self.model.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl ProcessMemoryReader for StaticProcessHistory {
    fn parts(
        &self,
        gpus: &BTreeSet<EntityId>,
        window: (u64, u64),
        cells: usize,
    ) -> Vec<MemoryPart> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((gpus.clone(), window, cells));
        self.parts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The key of process `pid` on host `host-test`, boot `boot-test`, started
/// at tick 7.
pub fn test_process_key(pid: i64) -> ProcessKey {
    ProcessKey {
        host_id: host_id("host-test"),
        boot_id: boot_id("boot-test"),
        pid_namespace: PidNamespace::new("pid:[4026531836]")
            .unwrap_or_else(|error| panic!("test pid namespace: {error}")),
        pid: Pid::new(pid).unwrap_or_else(|error| panic!("test pid: {error}")),
        start_time_ticks: 7,
    }
}

/// Process `pid` holding the bytes of `held` on each of its GPUs (`None`:
/// a GPU it uses without a known figure).
pub fn gpu_process(pid: i64, name: &str, held: &[(&str, Option<u64>)]) -> ProcessRow {
    ProcessRow {
        key: test_process_key(pid),
        name: name.to_owned(),
        command: String::new(),
        uid: Some(1000),
        state: Some('R'),
        descendants: None,
        cpu_percent: None,
        rss_bytes: None,
        missing_reason: None,
        gpu: held
            .iter()
            .map(|(gpu, bytes)| ProcessGpuLink {
                gpu_entity_id: EntityId::new(*gpu)
                    .unwrap_or_else(|error| panic!("test entity: {error}")),
                usage: GpuUsageKind::Compute,
                utilization_percent: None,
                memory_bytes: *bytes,
            })
            .collect(),
        sample_kind: SampleKind::Measured,
        read_finished_boot_ns: 0,
    }
}

/// The process table of collection `cycle`, read at `read_ns`.
pub fn process_table(cycle: u64, read_ns: u64, rows: Vec<ProcessRow>) -> ProcessTable {
    ProcessTable {
        rows,
        cycle_id: cycle,
        read_finished_boot_ns: read_ns,
        ..ProcessTable::default()
    }
}

pub fn host_id(value: &str) -> HostId {
    HostId::new(value).unwrap_or_else(|error| panic!("test host id: {error}"))
}

pub fn boot_id(value: &str) -> BootId {
    BootId::new(value).unwrap_or_else(|error| panic!("test boot id: {error}"))
}

pub fn source_id(value: &str) -> SourceId {
    SourceId::new(value).unwrap_or_else(|error| panic!("test source id: {error}"))
}

pub fn adapter_id(value: &str) -> AdapterId {
    AdapterId::new(value).unwrap_or_else(|error| panic!("test adapter id: {error}"))
}

pub fn read_group(value: &str) -> ReadGroup {
    ReadGroup::new(value).unwrap_or_else(|error| panic!("test read group: {error}"))
}

/// MetricKey on host `test-host`, boot `fake-boot`.
pub fn metric_key(entity: &str, metric: &str, scope: &str) -> MetricKey {
    MetricKey::new(
        host_id("test-host"),
        boot_id("fake-boot"),
        EntityId::new(entity).unwrap_or_else(|error| panic!("test entity: {error}")),
        MetricId::new(metric).unwrap_or_else(|error| panic!("test metric: {error}")),
        Scope::new(scope).unwrap_or_else(|error| panic!("test scope: {error}")),
    )
}

/// Builder for synthetic observations used by collector/store tests.
#[derive(Debug, Clone)]
pub struct ObservationBuilder {
    observation: Observation,
}

impl ObservationBuilder {
    pub fn new(key: MetricKey, adapter: &str, source: &str) -> Self {
        Self {
            observation: Observation {
                schema_version: 1,
                cycle_id: 1,
                config_revision: 1,
                key,
                provenance: Provenance {
                    adapter_id: adapter_id(adapter),
                    source_id: source_id(source),
                    origin_id: None,
                    origin_relation: OriginRelation::Unknown,
                    driver: "test-driver".to_owned().into(),
                    device_path: "test:///device".to_owned().into(),
                    channel: "test".to_owned().into(),
                    generation: 1,
                    profile_id: ProfileId::new("test-profile")
                        .unwrap_or_else(|error| panic!("{error}")),
                    profile_revision: 1,
                },
                sample_kind: SampleKind::Measured,
                read_status: ReadStatus::Ok,
                quality: Quality::Verified,
                native_unit: NativeUnit::new("degC").unwrap_or_else(|error| panic!("{error}")),
                value: Some(Scalar::from_u64(0)),
                read_started_boot_ns: 0,
                read_finished_boot_ns: 0,
                device_sample_boot_ns: None,
                source_update_interval_ms: None,
                reason_codes: Vec::new(),
            },
        }
    }

    pub fn f64(mut self, value: f64) -> Self {
        self.observation.value =
            Some(Scalar::from_f64(value).unwrap_or_else(|error| panic!("{error}")));
        self.observation.read_status = ReadStatus::Ok;
        self
    }

    pub fn u64(mut self, value: u64) -> Self {
        self.observation.value = Some(Scalar::from_u64(value));
        self.observation.read_status = ReadStatus::Ok;
        self
    }

    pub fn failed(mut self, status: ReadStatus) -> Self {
        self.observation.read_status = status;
        self.observation.value = None;
        self
    }

    pub fn unit(mut self, unit: &str) -> Self {
        self.observation.native_unit =
            NativeUnit::new(unit).unwrap_or_else(|error| panic!("{error}"));
        self
    }

    pub fn origin(mut self, origin: &str) -> Self {
        self.observation.provenance.origin_id =
            Some(OriginId::new(origin).unwrap_or_else(|error| panic!("{error}")));
        self.observation.provenance.origin_relation = OriginRelation::Confirmed;
        self
    }

    pub fn quality(mut self, quality: Quality) -> Self {
        self.observation.quality = quality;
        self
    }

    pub fn kind(mut self, kind: SampleKind) -> Self {
        self.observation.sample_kind = kind;
        self
    }

    pub fn generation(mut self, generation: u32) -> Self {
        self.observation.provenance.generation = generation;
        self
    }

    pub fn cycle(mut self, cycle: u64) -> Self {
        self.observation.cycle_id = cycle;
        self
    }

    pub fn read_at(mut self, finished_boot_ns: u64) -> Self {
        self.observation.read_started_boot_ns = finished_boot_ns.saturating_sub(1_000_000);
        self.observation.read_finished_boot_ns = finished_boot_ns;
        self
    }

    pub fn device_time(mut self, boot_ns: u64) -> Self {
        self.observation.device_sample_boot_ns = Some(boot_ns);
        self
    }

    pub fn reason(mut self, code: &str) -> Self {
        self.observation.reason_codes.push(code.to_owned());
        self
    }

    pub fn build(self) -> Observation {
        self.observation
    }
}

/// Wraps observations into a batch consistent with their cycle/adapter.
pub fn batch_of(
    adapter: &str,
    group: &str,
    cycle: u64,
    records: Vec<Observation>,
) -> ObservationBatch {
    let finished = records
        .iter()
        .map(|record| record.read_finished_boot_ns)
        .max()
        .unwrap_or(0);
    let started = records
        .iter()
        .map(|record| record.read_started_boot_ns)
        .min()
        .unwrap_or(0);
    ObservationBatch {
        protocol_version: IPC_PROTOCOL_VERSION,
        adapter_id: adapter_id(adapter),
        host_generation: 1,
        catalog_revision: 1,
        config_revision: 1,
        request_id: Uuid::new_v4(),
        cycle_id: cycle,
        read_group: read_group(group),
        started_boot_ns: started,
        finished_boot_ns: finished,
        records,
        status: BatchStatus::Ok,
        error_code: None,
    }
}

/// A committed sample of `key` read at `read_ns`: selected with `value`,
/// or unavailable without one.
pub fn snapshot_entry(key: &MetricKey, value: Option<f64>, read_ns: u64) -> SnapshotEntry {
    let selection_state = if value.is_some() {
        SelectionState::Selected
    } else {
        SelectionState::Unavailable
    };
    SnapshotEntry {
        stale_after_boot_ns: None,
        sample: CanonicalSample {
            schema_version: 1,
            snapshot_revision: 1,
            cycle_id: 1,
            key: key.clone(),
            source_id: value.map(|_| source_id("test:source:1")),
            source_generation: value.map(|_| 1),
            unit: CanonicalUnit::Percent,
            value: value.map(|number| {
                Scalar::from_f64(number).unwrap_or_else(|error| panic!("test value: {error}"))
            }),
            sample_kind: SampleKind::Measured,
            selection_state,
            quality: Quality::Verified,
            freshness: Freshness::Fresh,
            observed_boot_ns: None,
            committed_boot_ns: read_ns,
            policy_revision: 1,
            candidate_source_ids: Vec::new(),
            reason_codes: Vec::new(),
            eligible_for_history: value.is_some(),
            measurement_freshness: Freshness::Unknown,
            read_finished_boot_ns: read_ns,
        },
        last_good: None,
    }
}

/// The entity id `value` (a test's own spelling, so a bad one panics).
pub fn entity_id(value: &str) -> EntityId {
    EntityId::new(value).unwrap_or_else(|error| panic!("test entity: {error}"))
}

/// A catalog entity of `kind` with `attributes`.
pub fn entity_record(
    id: &str,
    kind: EntityKind,
    display_name: &str,
    attributes: &[(&str, &str)],
) -> EntityRecord {
    EntityRecord {
        entity_id: entity_id(id),
        kind,
        parent_id: None,
        generation: 1,
        discovered_boot_ns: 0,
        removed_boot_ns: None,
        display_name: display_name.to_owned(),
        attributes: attributes
            .iter()
            .map(|(key, value)| Attribute {
                key: (*key).to_owned(),
                value: (*value).to_owned(),
            })
            .collect(),
    }
}

/// A catalog holding `entities`.
pub fn catalog_of(entities: Vec<EntityRecord>) -> CatalogSnapshot {
    CatalogSnapshot {
        entities: entities
            .into_iter()
            .map(|record| (record.entity_id.clone(), Arc::new(record)))
            .collect(),
        ..CatalogSnapshot::default()
    }
}

/// `table` handed to the history as the Manager does, in a snapshot of its
/// own beside `catalog`.
pub fn table_sample(
    table: ProcessTable,
    catalog: CatalogSnapshot,
    process_period_ms: u64,
) -> ProcessTableSample {
    ProcessTableSample {
        snapshot: Arc::new(ImmutableSnapshot {
            process_table: Arc::new(table),
            ..ImmutableSnapshot::default()
        }),
        catalog: Arc::new(catalog),
        process_period_ms,
    }
}

/// A snapshot of `revision` holding the given `(collector, entry)` samples.
pub fn snapshot_of(revision: u64, entries: Vec<(&str, SnapshotEntry)>) -> ImmutableSnapshot {
    let mut domains: BTreeMap<CollectorId, DomainSnapshot> = BTreeMap::new();
    for (collector, entry) in entries {
        let collector =
            CollectorId::new(collector).unwrap_or_else(|error| panic!("test collector: {error}"));
        let domain = domains.entry(collector).or_default();
        domain.revision = revision;
        domain
            .samples
            .insert(entry.sample.key.clone(), Arc::new(entry));
    }
    ImmutableSnapshot {
        revision,
        domains: domains
            .into_iter()
            .map(|(id, domain)| (id, Arc::new(domain)))
            .collect(),
        ..ImmutableSnapshot::default()
    }
}
