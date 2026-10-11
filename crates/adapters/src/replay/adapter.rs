//! `ReplayAdapter`: a `TelemetryPort` serving a validated [`ReplayFixture`]
//! in the replay namespace.
//!
//! * Cycle selection: fixture cycle index = `plan.cycle_id % cycles.len()`
//!   (cycle ids are not shifted). Counter series continue monotonically
//!   across loops: loop `L = cycle_id / cycles.len()` adds `L` times the
//!   series' per-loop advance (modulo a declared counter modulus).
//! * Records are emitted for the requested read group only, stamped with the
//!   current clock, sample kind `replay`/`simulated` and the reason code
//!   `REPLAY_FIXTURE`/`SYNTHETIC_FIXTURE`. A sample absent from a cycle
//!   produces no record (a missing sample); a sample with a status produces a
//!   failed record (value `None`, never 0).
//! * Process rows follow the procfs convention: entity
//!   `ProcessKey::entity_id()`, provenance channel = process basename,
//!   device path `/proc/<pid>`.
//! * Faults fire whenever their fixture cycle is replayed: `hang` sleeps in
//!   `read` (so the host misses its deadline), `fail` fails every record,
//!   `generation_change` bumps source generations once per cycle id,
//!   `remove_source` reports `removed` until the next discovery removes it.
//! * `slice = (index, count)` serves [`ReplayFixture::slice`]: the live
//!   adapter families of that slice, so several replay hosts stand for the
//!   live hosts (one per adapter family) like the REF workload describes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{AdapterId, EntityId, HostId, MetricKey, ReadGroup, SourceId};
use dgxtop_core::metric::Scalar;
use dgxtop_core::naming;
use dgxtop_core::observation::{
    AdapterHealth, AdapterManifest, Capability, CatalogDelta, DiscoverRequest, EntityRecord,
    MAX_RECORDS_PER_BATCH, Observation, ObservationBatch, ReadPlan, SourceDescriptor, split_page,
};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Availability, Quality, RawKind, ReadStatus, SampleKind, Support};
use dgxtop_core::shared_text::SharedText;

use super::fixture::{
    FIXTURE_FORMAT, FaultKind, FixtureEntity, FixtureSample, FixtureSource, FixtureValue,
    ReplayError, ReplayFixture, RowKind, live_group_of, process_pid, row_kind_of,
};
use super::manifest;
use crate::common::{Pager, RecordFactory, SlotInterner};
use crate::hwmon::catalog::{CatalogContent, CatalogPublisher};
use crate::hwmon::engine::codes;

/// Sources one replay host may serve (manifest `source_limit`).
pub const MAX_SOURCES_PER_HOST: usize = 2048;
/// Highest generation representable in provenance.
const MAX_GENERATION: u32 = 0x7fff_ffff;

/// Reason codes of replay records.
mod reasons {
    pub const REPLAY_FIXTURE: &str = "REPLAY_FIXTURE";
    pub const SYNTHETIC_FIXTURE: &str = "SYNTHETIC_FIXTURE";
    pub const INJECTED_FAILURE: &str = "REPLAY_INJECTED_FAILURE";
    pub const GENERATION_CHANGE: &str = "REPLAY_GENERATION_CHANGE";
    pub const SOURCE_REMOVED: &str = "SOURCE_REMOVED";
}

/// One source served by this host.
#[derive(Debug)]
struct ReplaySource {
    descriptor: SourceDescriptor,
    record_key: MetricKey,
    quality: Quality,
    base_group: String,
    rows: Option<RowKind>,
    removed: bool,
    origin_reason: &'static str,
}

/// Value or failure of one sample.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SampleOutcome {
    Value(Scalar),
    Failed(ReadStatus),
}

/// One resolved sample of one cycle.
#[derive(Debug, Clone)]
struct ReplaySample {
    source: usize,
    series: usize,
    outcome: SampleOutcome,
    quality: Option<Quality>,
    reasons: Vec<String>,
    entity: Option<EntityId>,
    channel: Option<SharedText>,
    device_path: Option<SharedText>,
}

/// One fault resolved against this host's sources.
#[derive(Debug, Clone)]
struct ReplayFault {
    cycle: usize,
    base_group: Option<String>,
    kind: ResolvedFault,
    last_applied_cycle: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedFault {
    Hang(u32),
    Fail(ReadStatus),
    GenerationChange,
    RemoveSource(usize),
    /// Removal of a source served by another slice.
    Elsewhere,
}

/// Effects of the faults firing in one read.
#[derive(Debug, Default)]
struct FaultEffects {
    hang_ms: u32,
    fail: Option<ReadStatus>,
    reasons: Vec<&'static str>,
}

/// Replay telemetry adapter (simulated/replay namespace only).
pub struct ReplayAdapter {
    manifest: AdapterManifest,
    clock: Arc<dyn TimeProvider>,
    namespace_label: String,
    seed: u64,
    sampling_ms: u32,
    latency: Duration,
    sources: Vec<ReplaySource>,
    entities: Vec<FixtureEntity>,
    cycles: Vec<Vec<ReplaySample>>,
    /// Per-loop advance of each counter series (0 for gauges).
    advances: Vec<u64>,
    faults: Vec<ReplayFault>,
    publisher: CatalogPublisher,
    pager: Pager,
    discovered_boot_ns: Option<u64>,
    shut_down: bool,
}

impl ReplayAdapter {
    /// Loads, validates and serves a fixture file.
    pub fn from_path(
        path: &Path,
        adapter_id: AdapterId,
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        slice: Option<(u32, u32)>,
    ) -> Result<Self, ReplayError> {
        let fixture = ReplayFixture::load(path)?;
        Self::from_fixture(fixture, adapter_id, clock, host_id, slice)
    }

    /// Serves an in-memory fixture (validated here).
    pub fn from_fixture(
        fixture: ReplayFixture,
        adapter_id: AdapterId,
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        slice: Option<(u32, u32)>,
    ) -> Result<Self, ReplayError> {
        if !naming::is_replay_adapter(adapter_id.as_str()) {
            return Err(ReplayError::NotReplayAdapter(adapter_id.to_string()));
        }
        fixture.validate()?;
        let (index, count) = slice_bounds(slice)?;
        let fixture = fixture.slice(index, count)?;
        let builder = Builder {
            fixture: &fixture,
            adapter_id: &adapter_id,
            host_id: &host_id,
            clock: &clock,
        };
        let (sources, positions) = builder.sources();
        if sources.len() > MAX_SOURCES_PER_HOST {
            return Err(ReplayError::SliceTooLarge {
                sources: sources.len(),
                limit: MAX_SOURCES_PER_HOST,
            });
        }
        let (cycles, advances) = builder.cycles(&sources, &positions);
        let faults = builder.faults(&positions);
        let entities = fixture.entities.clone();
        Ok(Self {
            manifest: manifest(&adapter_id, false),
            publisher: CatalogPublisher::new(adapter_id, MAX_RECORDS_PER_BATCH),
            clock,
            namespace_label: fixture.namespace_label.clone(),
            seed: fixture.seed,
            sampling_ms: fixture.sampling_ms,
            latency: Duration::from_micros(u64::from(fixture.simulated_latency_us)),
            sources,
            entities,
            cycles,
            advances,
            faults,
            pager: Pager::new(),
            discovered_boot_ns: None,
            shut_down: false,
        })
    }

    /// Declares in-process isolation (`--replay`) instead of a host process.
    pub fn with_in_process(mut self, in_process: bool) -> Self {
        self.manifest = manifest(&self.manifest.adapter_id, in_process);
        self
    }

    /// Seed recorded in the fixture.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Number of fixture cycles before the replay loops.
    pub fn cycle_count(&self) -> usize {
        self.cycles.len()
    }

    /// Number of sources served by this host (removed ones excluded).
    pub fn source_count(&self) -> usize {
        self.sources.iter().filter(|source| !source.removed).count()
    }

    fn catalog_content(&self, discovered_boot_ns: u64) -> CatalogContent {
        let entities: Vec<EntityRecord> = self
            .entities
            .iter()
            .map(|entity| EntityRecord {
                entity_id: entity.entity_id.clone(),
                kind: entity.kind,
                parent_id: entity.parent_id.clone(),
                generation: 1,
                discovered_boot_ns,
                removed_boot_ns: None,
                display_name: entity.display_name.clone(),
                attributes: entity.attributes.clone(),
            })
            .collect();
        CatalogContent {
            sources: self
                .sources
                .iter()
                .filter(|source| !source.removed)
                .map(|source| source.descriptor.clone())
                .collect(),
            removable_entities: entities
                .iter()
                .map(|entity| entity.entity_id.clone())
                .collect(),
            entities,
            capabilities: self.capabilities(),
            availability: Availability::Available,
            error_code: None,
        }
    }

    fn capabilities(&self) -> Vec<Capability> {
        let keys: BTreeSet<(String, EntityId)> = self
            .sources
            .iter()
            .filter(|source| !source.removed)
            .map(|source| {
                (
                    format!(
                        "{}|{}",
                        source.record_key.metric_id, source.record_key.scope
                    ),
                    source.record_key.entity_id.clone(),
                )
            })
            .collect();
        keys.into_iter()
            .map(|(capability_id, entity_id)| Capability {
                capability_id,
                entity_id: Some(entity_id),
                support: Support::Supported,
                reason: None,
                min_interval_ms: Some(self.sampling_ms),
            })
            .collect()
    }

    /// Applies the faults of `cycle_index` for `group`.
    fn apply_faults(&mut self, cycle_index: usize, cycle_id: u64, group: &str) -> FaultEffects {
        let mut effects = FaultEffects::default();
        let mut bump_generation = None;
        for fault in &mut self.faults {
            let applies = fault.cycle == cycle_index
                && fault.base_group.as_deref().is_none_or(|only| only == group);
            if !applies {
                continue;
            }
            match fault.kind {
                ResolvedFault::Hang(ms) => effects.hang_ms = effects.hang_ms.max(ms),
                ResolvedFault::Fail(status) => {
                    effects.fail = Some(status);
                    effects.reasons.push(reasons::INJECTED_FAILURE);
                }
                ResolvedFault::GenerationChange => {
                    if fault.last_applied_cycle != Some(cycle_id) {
                        fault.last_applied_cycle = Some(cycle_id);
                        bump_generation = Some(fault.base_group.clone());
                    }
                    effects.reasons.push(reasons::GENERATION_CHANGE);
                }
                ResolvedFault::RemoveSource(index) => {
                    if let Some(source) = self.sources.get_mut(index) {
                        source.removed = true;
                    }
                }
                ResolvedFault::Elsewhere => {}
            }
        }
        if let Some(only) = bump_generation {
            for source in &mut self.sources {
                if only
                    .as_deref()
                    .is_none_or(|group| group == source.base_group)
                {
                    source.descriptor.generation = source
                        .descriptor
                        .generation
                        .saturating_add(1)
                        .min(MAX_GENERATION);
                }
            }
        }
        effects
    }

    /// Sources of `group` requested by the plan (empty slots = all).
    fn requested(&self, plan: &ReadPlan, group: &str) -> BTreeSet<usize> {
        let requested_slots: BTreeSet<u32> = plan.source_slots.iter().copied().collect();
        self.sources
            .iter()
            .enumerate()
            .filter(|(_, source)| source.base_group == group)
            .filter(|(_, source)| {
                requested_slots.is_empty() || requested_slots.contains(&source.descriptor.slot)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn build_records(
        &self,
        plan: &ReadPlan,
        group: &str,
        cycle_index: usize,
        loop_number: u64,
        effects: &FaultEffects,
    ) -> Vec<Observation> {
        let requested = self.requested(plan, group);
        let mut records = Vec::new();
        for &index in &requested {
            let source = &self.sources[index];
            if source.removed && self.publisher.holds(&source.descriptor.source_id) {
                let now = self.clock.now_boot_ns();
                records.push(RecordFactory::failed(
                    plan,
                    &source.descriptor,
                    source.record_key.clone(),
                    ReadStatus::Removed,
                    now,
                    now,
                    reasons::SOURCE_REMOVED.to_owned(),
                ));
            }
        }
        let Some(samples) = self.cycles.get(cycle_index) else {
            return records;
        };
        for sample in samples {
            if !requested.contains(&sample.source) {
                continue;
            }
            let source = &self.sources[sample.source];
            if source.removed {
                continue;
            }
            records.push(self.sample_record(plan, source, sample, loop_number, effects));
        }
        records
    }

    fn sample_record(
        &self,
        plan: &ReadPlan,
        source: &ReplaySource,
        sample: &ReplaySample,
        loop_number: u64,
        effects: &FaultEffects,
    ) -> Observation {
        let started = self.clock.now_boot_ns();
        let key = match &sample.entity {
            Some(entity_id) => MetricKey {
                entity_id: entity_id.clone(),
                ..source.record_key.clone()
            },
            None => source.record_key.clone(),
        };
        let outcome = match effects.fail {
            Some(status) => SampleOutcome::Failed(status),
            None => sample.outcome,
        };
        let mut record = match outcome {
            SampleOutcome::Value(value) => RecordFactory::ok(
                plan,
                &source.descriptor,
                key,
                self.looped(source, sample, value, loop_number),
                sample.quality.unwrap_or(source.quality),
                started,
                self.clock.now_boot_ns(),
                vec![source.origin_reason.to_owned()],
            ),
            SampleOutcome::Failed(status) => RecordFactory::failed(
                plan,
                &source.descriptor,
                key,
                status,
                started,
                self.clock.now_boot_ns(),
                source.origin_reason.to_owned(),
            ),
        };
        record.reason_codes.extend(sample.reasons.iter().cloned());
        record
            .reason_codes
            .extend(effects.reasons.iter().map(|reason| (*reason).to_owned()));
        record
            .reason_codes
            .truncate(dgxtop_core::observation::MAX_REASON_CODES);
        if let Some(channel) = &sample.channel {
            record.provenance.channel = channel.clone();
        }
        if let Some(device_path) = &sample.device_path {
            record.provenance.device_path = device_path.clone();
        }
        record
    }

    /// Counter values continue across loops; gauges repeat.
    fn looped(
        &self,
        source: &ReplaySource,
        sample: &ReplaySample,
        value: Scalar,
        loop_number: u64,
    ) -> Scalar {
        let advance = self.advances.get(sample.series).copied().unwrap_or(0);
        match (value.as_u64(), source.descriptor.raw_kind) {
            (Some(raw), RawKind::Counter) if advance > 0 && loop_number > 0 => Scalar::from_u64(
                advance_counter(raw, advance, loop_number, source.descriptor.counter_modulus),
            ),
            _ => value,
        }
    }

    fn serve_page(&self, plan: &ReadPlan, group: &str, page: u32) -> ObservationBatch {
        match self
            .pager
            .page(plan.cycle_id, group, page, MAX_RECORDS_PER_BATCH)
        {
            Some(served) => RecordFactory::batch(
                plan,
                served.records,
                served.started_boot_ns,
                served.finished_boot_ns,
                served.continues,
                None,
            ),
            None => RecordFactory::failed_batch(
                plan,
                self.clock.now_boot_ns(),
                codes::PAGE_SNAPSHOT_MISSING,
            ),
        }
    }
}

impl TelemetryPort for ReplayAdapter {
    fn manifest(&self) -> &AdapterManifest {
        &self.manifest
    }

    fn discover(&mut self, request: &DiscoverRequest) -> CatalogDelta {
        if self.shut_down {
            return self.publisher.failure(
                request,
                codes::ADAPTER_SHUT_DOWN,
                Some(Availability::Unavailable),
            );
        }
        if request.adapter_id != self.manifest.adapter_id {
            return self
                .publisher
                .failure(request, codes::ADAPTER_ID_MISMATCH, None);
        }
        if request.page > 0 {
            return self.publisher.next(request);
        }
        let now = self.clock.now_boot_ns();
        if now > request.deadline_boot_ns {
            return self
                .publisher
                .failure(request, codes::DEADLINE_EXPIRED, None);
        }
        let discovered_boot_ns = *self.discovered_boot_ns.get_or_insert(now);
        let content = self.catalog_content(discovered_boot_ns);
        self.publisher.begin(request, content)
    }

    fn read(&mut self, plan: &ReadPlan) -> ObservationBatch {
        let now = self.clock.now_boot_ns();
        if self.shut_down {
            return RecordFactory::failed_batch(plan, now, codes::ADAPTER_SHUT_DOWN);
        }
        if plan.adapter_id != self.manifest.adapter_id {
            return RecordFactory::failed_batch(plan, now, codes::ADAPTER_ID_MISMATCH);
        }
        let (group, encoded_page) = split_page(plan.read_group.as_str());
        let page = if plan.page > 0 {
            plan.page
        } else {
            encoded_page
        };
        if live_group_of(group).is_none() {
            return RecordFactory::failed_batch(plan, now, codes::UNKNOWN_READ_GROUP);
        }
        if page > 0 {
            return self.serve_page(plan, group, page);
        }
        if now > plan.deadline_boot_ns {
            return RecordFactory::failed_batch(plan, now, codes::DEADLINE_EXPIRED);
        }
        let cycle_count = u64::try_from(self.cycles.len()).unwrap_or(1).max(1);
        let cycle_index = usize::try_from(plan.cycle_id % cycle_count).unwrap_or(0);
        let loop_number = plan.cycle_id / cycle_count;
        let effects = self.apply_faults(cycle_index, plan.cycle_id, group);
        if effects.hang_ms > 0 {
            std::thread::sleep(Duration::from_millis(u64::from(effects.hang_ms)));
        }
        if !self.latency.is_zero() {
            std::thread::sleep(self.latency);
        }
        let started = self.clock.now_boot_ns();
        let records = self.build_records(plan, group, cycle_index, loop_number, &effects);
        let finished = self.clock.now_boot_ns().max(started);
        self.pager
            .store(plan.cycle_id, group, records, started, finished);
        self.serve_page(plan, group, 0)
    }

    fn health(&self) -> AdapterHealth {
        if self.shut_down {
            return AdapterHealth {
                availability: Availability::Unavailable,
                detail: Some("shut down".to_owned()),
            };
        }
        AdapterHealth {
            availability: Availability::Available,
            detail: Some(format!(
                "replay {}: {} sources, {} cycles",
                self.namespace_label,
                self.source_count(),
                self.cycles.len()
            )),
        }
    }

    fn shutdown(&mut self) {
        self.shut_down = true;
        self.pager = Pager::new();
    }
}

fn slice_bounds(slice: Option<(u32, u32)>) -> Result<(u32, u32), ReplayError> {
    let (index, count) = slice.unwrap_or((0, 1));
    if count == 0 || index >= count {
        return Err(ReplayError::InvalidSlice { index, count });
    }
    Ok((index, count))
}

/// `raw + loops * advance`, modulo the declared modulus (exact, no overflow).
fn advance_counter(raw: u64, advance: u64, loops: u64, modulus: Option<u64>) -> u64 {
    let offset = u128::from(advance) * u128::from(loops);
    match modulus {
        Some(modulus) if modulus > 0 => {
            let total = (u128::from(raw) + offset) % u128::from(modulus);
            u64::try_from(total).unwrap_or(raw)
        }
        _ => u64::try_from(u128::from(raw) + offset).unwrap_or(u64::MAX),
    }
}

/// Per-loop advance of a counter series: last - first plus one typical step,
/// so the loop boundary looks like an ordinary increment. A series that
/// decreases without a modulus (a reset in the fixture) does not advance.
fn series_advance(values: &[u64], modulus: Option<u64>) -> u64 {
    let (Some(first), Some(last)) = (values.first(), values.last()) else {
        return 0;
    };
    let steps = u128::try_from(values.len().saturating_sub(1)).unwrap_or(0);
    if steps == 0 {
        return 0;
    }
    let span = match modulus {
        Some(modulus) if modulus > 0 => {
            let modulus = u128::from(modulus);
            values
                .windows(2)
                .map(|pair| {
                    (u128::from(pair[1]) + modulus - u128::from(pair[0]) % modulus) % modulus
                })
                .sum::<u128>()
        }
        _ => {
            if values.windows(2).any(|pair| pair[1] < pair[0]) {
                return 0;
            }
            u128::from(*last) - u128::from(*first)
        }
    };
    let advance = span + span / steps;
    u64::try_from(advance).unwrap_or(0)
}

/// Builds the host's view of a fixture.
struct Builder<'a> {
    fixture: &'a ReplayFixture,
    adapter_id: &'a AdapterId,
    host_id: &'a HostId,
    clock: &'a Arc<dyn TimeProvider>,
}

impl Builder<'_> {
    /// The slice's sources plus the source-id -> index map.
    fn sources(&self) -> (Vec<ReplaySource>, BTreeMap<SourceId, usize>) {
        let boot_id = self.clock.boot_id();
        let mut interner = SlotInterner::new();
        let mut sources = Vec::new();
        let mut positions = BTreeMap::new();
        for source in &self.fixture.sources {
            let Some(slot) = interner.slot_for(&source.source_id) else {
                continue;
            };
            let Ok(read_group) = ReadGroup::new(source.read_group.as_str()) else {
                continue;
            };
            let record_key = MetricKey::new(
                self.host_id.clone(),
                boot_id.clone(),
                source.entity_id.clone(),
                source.metric_id.clone(),
                source.scope.clone(),
            );
            positions.insert(source.source_id.clone(), sources.len());
            sources.push(ReplaySource {
                descriptor: self.descriptor(source, slot, read_group, &record_key),
                record_key,
                quality: source.quality,
                base_group: source.read_group.clone(),
                rows: row_kind_of(&source.read_group).filter(|_| source.rows),
                removed: false,
                origin_reason: match source.sample_kind {
                    SampleKind::Replay => reasons::REPLAY_FIXTURE,
                    _ => reasons::SYNTHETIC_FIXTURE,
                },
            });
        }
        (sources, positions)
    }

    fn descriptor(
        &self,
        source: &FixtureSource,
        slot: u32,
        read_group: ReadGroup,
        record_key: &MetricKey,
    ) -> SourceDescriptor {
        // Row sources have no single proven key.
        let metric = (!source.rows).then(|| record_key.clone());
        SourceDescriptor {
            source_id: source.source_id.clone(),
            adapter_id: self.adapter_id.clone(),
            slot,
            generation: 1,
            entity_id: source.entity_id.clone(),
            device_id: source.entity_id.to_string(),
            channel: source
                .channel
                .clone()
                .unwrap_or_else(|| source.metric_id.to_string()),
            metric,
            origin: source.origin.clone(),
            driver: source.driver.clone().unwrap_or_else(|| "replay".to_owned()),
            api_version: Some(FIXTURE_FORMAT.to_owned()),
            device_path: source
                .device_path
                .clone()
                .or_else(|| process_device_path(source))
                .unwrap_or_else(|| format!("replay://{}", self.fixture.namespace_label)),
            native_unit: source.native_unit.clone(),
            raw_kind: source.raw_kind,
            sample_kind: source.sample_kind,
            read_group,
            update_interval_ms: source.update_interval_ms,
            profile_id: source.profile_id.clone(),
            profile_revision: source.profile_revision,
            profile_rank: source.profile_rank,
            label: source.label.clone(),
            counter_modulus: source.counter_modulus,
        }
    }

    /// Slice samples per cycle plus per-series counter advances.
    fn cycles(
        &self,
        sources: &[ReplaySource],
        positions: &BTreeMap<SourceId, usize>,
    ) -> (Vec<Vec<ReplaySample>>, Vec<u64>) {
        let mut interner = Interner::default();
        let mut series: BTreeMap<(usize, Option<EntityId>), usize> = BTreeMap::new();
        let mut series_values: Vec<Vec<u64>> = Vec::new();
        let mut series_modulus: Vec<Option<u64>> = Vec::new();
        let mut cycles = Vec::with_capacity(self.fixture.cycles.len());
        for cycle in &self.fixture.cycles {
            let mut samples = Vec::new();
            for sample in cycle {
                let Some(&source_index) = positions.get(&sample.source_id) else {
                    continue;
                };
                let source = &sources[source_index];
                let entity = sample
                    .entity_id
                    .as_ref()
                    .map(|entity| interner.entity(entity));
                let next_series = series.len();
                let series_index = *series
                    .entry((source_index, entity.clone()))
                    .or_insert(next_series);
                if series_index == series_values.len() {
                    series_values.push(Vec::new());
                    series_modulus.push(source.descriptor.counter_modulus);
                }
                let outcome = outcome_of(sample);
                if let (SampleOutcome::Value(value), RawKind::Counter) =
                    (outcome, source.descriptor.raw_kind)
                    && let Some(raw) = value.as_u64()
                {
                    series_values[series_index].push(raw);
                }
                samples.push(ReplaySample {
                    source: source_index,
                    series: series_index,
                    outcome,
                    quality: sample.quality,
                    reasons: sample.reasons.clone(),
                    device_path: match (source.rows, &entity) {
                        (Some(RowKind::Process), Some(entity)) => process_pid(entity.as_str())
                            .map(|pid| SharedText::from(interner.text(&format!("/proc/{pid}")))),
                        _ => None,
                    },
                    channel: sample
                        .channel
                        .as_deref()
                        .map(|channel| SharedText::from(interner.text(channel))),
                    entity,
                });
            }
            cycles.push(samples);
        }
        let advances = series_values
            .iter()
            .zip(&series_modulus)
            .map(|(values, modulus)| series_advance(values, *modulus))
            .collect();
        (cycles, advances)
    }

    fn faults(&self, positions: &BTreeMap<SourceId, usize>) -> Vec<ReplayFault> {
        self.fixture
            .faults
            .iter()
            .map(|fault| ReplayFault {
                cycle: usize::try_from(fault.cycle).unwrap_or(usize::MAX),
                base_group: fault.read_group.clone(),
                kind: match &fault.kind {
                    FaultKind::Hang { ms } => ResolvedFault::Hang(*ms),
                    FaultKind::Fail { status } => ResolvedFault::Fail(*status),
                    FaultKind::GenerationChange => ResolvedFault::GenerationChange,
                    FaultKind::RemoveSource { source_id } => positions
                        .get(source_id)
                        .map_or(ResolvedFault::Elsewhere, |index| {
                            ResolvedFault::RemoveSource(*index)
                        }),
                },
                last_applied_cycle: None,
            })
            .collect()
    }
}

/// `/proc/<pid>` of a per-process (non-row) `procfs.process` source.
fn process_device_path(source: &FixtureSource) -> Option<String> {
    if source.rows || row_kind_of(&source.read_group) != Some(RowKind::Process) {
        return None;
    }
    process_pid(source.entity_id.as_str()).map(|pid| format!("/proc/{pid}"))
}

fn outcome_of(sample: &FixtureSample) -> SampleOutcome {
    match (sample.value, sample.status) {
        (Some(FixtureValue::U64(value)), _) => SampleOutcome::Value(Scalar::from_u64(value)),
        (Some(FixtureValue::F64(value)), _) => Scalar::from_f64(value).map_or(
            SampleOutcome::Failed(ReadStatus::Error),
            SampleOutcome::Value,
        ),
        (None, Some(status)) => SampleOutcome::Failed(status),
        (None, None) => SampleOutcome::Failed(ReadStatus::Error),
    }
}

/// Shares repeated entity ids and texts between samples (bounded memory).
#[derive(Default)]
struct Interner {
    entities: BTreeMap<EntityId, EntityId>,
    texts: BTreeMap<String, Arc<str>>,
}

impl Interner {
    fn entity(&mut self, entity: &EntityId) -> EntityId {
        self.entities
            .entry(entity.clone())
            .or_insert_with(|| entity.clone())
            .clone()
    }

    fn text(&mut self, text: &str) -> Arc<str> {
        if let Some(shared) = self.texts.get(text) {
            return Arc::clone(shared);
        }
        let shared: Arc<str> = Arc::from(text);
        self.texts.insert(text.to_owned(), Arc::clone(&shared));
        shared
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_bounds_reject_invalid_partitions() {
        assert!(slice_bounds(Some((0, 0))).is_err());
        assert!(slice_bounds(Some((3, 3))).is_err());
        assert_eq!(slice_bounds(None).unwrap(), (0, 1));
        assert_eq!(slice_bounds(Some((2, 7))).unwrap(), (2, 7));
    }
}
