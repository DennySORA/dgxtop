//! Catalog identity and capability state of a sysfs adapter.
//!
//! * Slots come from a [`SlotInterner`]: a source id keeps its slot forever.
//! * Generations: every new device incarnation (first sight, re-appearance,
//!   changed enumeration instance, or a read that saw the files vanish) gets
//!   a generation from one monotonic counter, so a `(slot, generation)` pair
//!   is never reused even after device state eviction.
//! * Support: unsupported/denied sources are re-probed at most once per
//!   `unsupported_probe_ms` through a per-device [`ProbeThrottle`]; a device
//!   re-creation (driver restart) resets its throttle.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::identity::{
    AdapterId, EntityId, MetricKey, OriginIdentity, ProfileId, ReadGroup, SourceId,
};
use dgxtop_core::metric::NativeUnit;
use dgxtop_core::observation::{
    Attribute, Capability, EntityKind, EntityRecord, MAX_ATTRIBUTES, SourceDescriptor,
};
use dgxtop_core::quality::{Availability, Quality, RawKind, ReadStatus, SampleKind, Support};

use super::catalog::CatalogContent;
use super::reader::{ReadFailure, ReadSpec, Reading, read_source};
use crate::common::{ProbeThrottle, SlotInterner, SystemFiles};
use crate::registry::AdapterOptions;

/// Device states kept for incarnation tracking (absent ones are evicted first).
const MAX_DEVICE_STATES: usize = 1024;
/// Shortest capability interval advertised (the adapters' minimum sample period).
const MIN_CAPABILITY_INTERVAL_MS: u32 = 1000;
/// Highest generation representable in provenance (`i32::MAX`).
const MAX_GENERATION: u32 = i32::MAX as u32;
/// Discovery error code: more sources than `max_sources`; lowest priority dropped.
pub(crate) const SOURCE_LIMIT_EXCEEDED: &str = "SOURCE_LIMIT_EXCEEDED";
/// Discovery error code: some devices could not be scanned this round.
pub(crate) const PARTIAL_DISCOVERY: &str = "PARTIAL_DISCOVERY";
/// Reason code of records for sources that disappeared.
pub(crate) const SOURCE_REMOVED: &str = "SOURCE_REMOVED";
/// Capability reason for permission failures (distinct from unsupported).
const PERMISSION_DENIED: &str = "PERMISSION_DENIED";

/// One device found by a scan: stable key plus its current enumeration
/// instance (canonical sysfs path); an instance change means re-creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScannedDevice {
    pub device_key: String,
    pub instance: String,
}

/// One source found by a scan, before slot and generation assignment.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceDraft {
    pub source_id: SourceId,
    pub device_key: String,
    pub entity_id: EntityId,
    /// Key of emitted records, proven by a profile.
    pub record_key: MetricKey,
    pub origin: OriginIdentity,
    pub driver: String,
    pub device_path: String,
    pub channel: String,
    pub native_unit: NativeUnit,
    pub raw_kind: RawKind,
    pub profile_id: ProfileId,
    pub profile_revision: u32,
    pub profile_rank: u16,
    pub quality: Quality,
    pub label: Option<String>,
    pub counter_modulus: Option<u64>,
    pub update_interval_ms: Option<u32>,
    pub read: ReadSpec,
}

/// One entity found by a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntityDraft {
    pub entity_id: EntityId,
    pub kind: EntityKind,
    pub parent_id: Option<EntityId>,
    pub display_name: String,
    pub attributes: Vec<Attribute>,
    /// Generation follows this device's incarnation; `None` means generation 1.
    pub device_key: Option<String>,
    /// Adapter-owned entities may be removed; shared ones (`cpu.package.N`) never.
    pub owned: bool,
}

/// How a scan ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScanOutcome {
    Complete,
    /// The driver class directory does not exist: legitimately no devices.
    Missing,
    /// The scan could not be trusted; the previous catalog stays.
    Failed(String),
}

/// Result of one scan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScanOutput {
    pub outcome: ScanOutcome,
    pub devices: Vec<ScannedDevice>,
    pub sources: Vec<SourceDraft>,
    pub entities: Vec<EntityDraft>,
    /// Devices skipped because of transient failures.
    pub partial_failures: u32,
}

impl ScanOutput {
    pub(crate) fn complete() -> Self {
        Self {
            outcome: ScanOutcome::Complete,
            devices: Vec::new(),
            sources: Vec::new(),
            entities: Vec::new(),
            partial_failures: 0,
        }
    }

    pub(crate) fn missing() -> Self {
        Self {
            outcome: ScanOutcome::Missing,
            ..Self::complete()
        }
    }

    pub(crate) fn failed(code: impl Into<String>) -> Self {
        Self {
            outcome: ScanOutcome::Failed(code.into()),
            ..Self::complete()
        }
    }
}

/// Capability state of one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceSupport {
    /// Not probed yet.
    Unknown,
    Supported,
    /// Unsupported or permission denied; re-probed only when throttling allows.
    Unsupported {
        status: ReadStatus,
        reason: String,
    },
}

/// One catalogued source with its read spec and capability state.
#[derive(Debug, Clone)]
pub(crate) struct SourceEntry {
    pub descriptor: SourceDescriptor,
    pub record_key: MetricKey,
    pub quality: Quality,
    pub read: ReadSpec,
    pub support: SourceSupport,
}

#[derive(Debug)]
struct DeviceState {
    generation: u32,
    instance: String,
    present: bool,
    last_seen_scan: u64,
    throttle: ProbeThrottle,
}

#[derive(Debug, Clone)]
struct EntityState {
    record: EntityRecord,
    owned: bool,
}

/// Sources requested by a read plan.
#[derive(Debug, Default)]
pub(crate) struct ReadTargets {
    pub current: Vec<SourceId>,
    /// Sources gone from the scan but still held by the parent catalog.
    pub departed: Vec<SourceId>,
    pub unknown_slots: Vec<u32>,
}

/// Catalog identity, entries and capability state of one sysfs adapter.
#[derive(Debug)]
pub(crate) struct SourceTable {
    adapter_id: AdapterId,
    read_group: ReadGroup,
    max_sources: usize,
    probe_interval_ms: u64,
    interner: SlotInterner,
    next_generation: u32,
    scan_counter: u64,
    devices: BTreeMap<String, DeviceState>,
    sources: BTreeMap<SourceId, SourceEntry>,
    by_slot: BTreeMap<u32, SourceId>,
    departed: BTreeMap<SourceId, SourceEntry>,
    entities: BTreeMap<EntityId, EntityState>,
    suspects: BTreeSet<String>,
}

impl SourceTable {
    pub(crate) fn new(
        adapter_id: AdapterId,
        read_group: ReadGroup,
        options: &AdapterOptions,
    ) -> Self {
        let max_sources = usize::try_from(options.max_sources.min(2048)).unwrap_or(2048);
        Self {
            adapter_id,
            read_group,
            max_sources,
            probe_interval_ms: options.unsupported_probe_ms,
            interner: SlotInterner::new(),
            next_generation: 1,
            scan_counter: 0,
            devices: BTreeMap::new(),
            sources: BTreeMap::new(),
            by_slot: BTreeMap::new(),
            departed: BTreeMap::new(),
            entities: BTreeMap::new(),
            suspects: BTreeSet::new(),
        }
    }

    /// Device keys whose cached metadata must be re-read (suspected re-creation).
    pub(crate) fn suspects(&self) -> &BTreeSet<String> {
        &self.suspects
    }

    pub(crate) fn entry(&self, source_id: &SourceId) -> Option<&SourceEntry> {
        self.sources
            .get(source_id)
            .or_else(|| self.departed.get(source_id))
    }

    pub(crate) fn source_count(&self) -> usize {
        self.sources.len()
    }

    /// Applies a scan. `Err(code)` leaves the previous catalog untouched;
    /// `Ok(error_code)` reports partial problems of an accepted scan.
    pub(crate) fn apply(
        &mut self,
        scan: ScanOutput,
        now_boot_ns: u64,
    ) -> Result<Option<String>, String> {
        if let ScanOutcome::Failed(code) = scan.outcome {
            return Err(code);
        }
        self.scan_counter = self.scan_counter.wrapping_add(1);
        self.update_devices(&scan.devices);
        let truncated = self.rebuild_sources(scan.sources);
        self.rebuild_entities(scan.entities, now_boot_ns);
        self.suspects.clear();
        let error_code = if truncated {
            Some(SOURCE_LIMIT_EXCEEDED.to_owned())
        } else if scan.partial_failures > 0 {
            Some(PARTIAL_DISCOVERY.to_owned())
        } else {
            None
        };
        Ok(error_code)
    }

    fn update_devices(&mut self, devices: &[ScannedDevice]) {
        let seen: BTreeSet<&str> = devices
            .iter()
            .map(|device| device.device_key.as_str())
            .collect();
        for (key, state) in &mut self.devices {
            if !seen.contains(key.as_str()) {
                state.present = false;
            }
        }
        let mut assigned = false;
        for device in devices {
            let fresh_generation = self.next_generation;
            match self.devices.get_mut(&device.device_key) {
                Some(state) => {
                    let recreated = !state.present
                        || state.instance != device.instance
                        || self.suspects.contains(&device.device_key);
                    if recreated {
                        state.generation = fresh_generation;
                        state.throttle = ProbeThrottle::new(self.probe_interval_ms);
                        assigned = true;
                    }
                    state.instance.clone_from(&device.instance);
                    state.present = true;
                    state.last_seen_scan = self.scan_counter;
                }
                None => {
                    self.devices.insert(
                        device.device_key.clone(),
                        DeviceState {
                            generation: fresh_generation,
                            instance: device.instance.clone(),
                            present: true,
                            last_seen_scan: self.scan_counter,
                            throttle: ProbeThrottle::new(self.probe_interval_ms),
                        },
                    );
                    assigned = true;
                }
            }
        }
        if assigned {
            self.next_generation = self.next_generation.saturating_add(1).min(MAX_GENERATION);
        }
        self.evict_absent_devices();
    }

    fn evict_absent_devices(&mut self) {
        while self.devices.len() > MAX_DEVICE_STATES {
            let oldest = self
                .devices
                .iter()
                .filter(|(_, state)| !state.present)
                .min_by_key(|(_, state)| state.last_seen_scan)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    self.devices.remove(&key);
                }
                None => break,
            }
        }
    }

    fn generation_of(&self, device_key: &str) -> u32 {
        self.devices
            .get(device_key)
            .map_or(1, |state| state.generation)
    }

    /// Rebuilds the source map; returns whether `max_sources` truncated it.
    fn rebuild_sources(&mut self, drafts: Vec<SourceDraft>) -> bool {
        let mut unique: BTreeMap<SourceId, SourceDraft> = BTreeMap::new();
        for draft in drafts {
            unique.entry(draft.source_id.clone()).or_insert(draft);
        }
        // Truncation keeps the lowest source ids.
        let mut ordered: Vec<SourceDraft> = unique.into_values().collect();
        let truncated = ordered.len() > self.max_sources;
        ordered.truncate(self.max_sources);
        let mut previous = std::mem::take(&mut self.sources);
        self.by_slot.clear();
        for draft in ordered {
            let Some(slot) = self.interner.slot_for(&draft.source_id) else {
                continue;
            };
            let generation = self.generation_of(&draft.device_key);
            let support = previous
                .remove(&draft.source_id)
                .filter(|entry| entry.descriptor.generation == generation)
                .map_or(SourceSupport::Unknown, |entry| entry.support);
            let entry = self.entry_from_draft(draft, slot, generation, support);
            self.by_slot
                .insert(slot, entry.descriptor.source_id.clone());
            self.sources
                .insert(entry.descriptor.source_id.clone(), entry);
        }
        for (source_id, entry) in previous {
            self.departed.insert(source_id, entry);
        }
        for source_id in self.sources.keys() {
            self.departed.remove(source_id);
        }
        truncated
    }

    fn entry_from_draft(
        &self,
        draft: SourceDraft,
        slot: u32,
        generation: u32,
        support: SourceSupport,
    ) -> SourceEntry {
        let metric = Some(draft.record_key.clone());
        SourceEntry {
            descriptor: SourceDescriptor {
                source_id: draft.source_id,
                adapter_id: self.adapter_id.clone(),
                slot,
                generation,
                entity_id: draft.entity_id,
                device_id: draft.device_key,
                channel: draft.channel,
                metric,
                origin: draft.origin,
                driver: draft.driver,
                api_version: None,
                device_path: draft.device_path,
                native_unit: draft.native_unit,
                raw_kind: draft.raw_kind,
                sample_kind: SampleKind::Measured,
                read_group: self.read_group.clone(),
                update_interval_ms: draft.update_interval_ms,
                profile_id: draft.profile_id,
                profile_revision: draft.profile_revision,
                profile_rank: draft.profile_rank,
                label: draft.label,
                counter_modulus: draft.counter_modulus,
            },
            record_key: draft.record_key,
            quality: draft.quality,
            read: draft.read,
            support,
        }
    }

    fn rebuild_entities(&mut self, drafts: Vec<EntityDraft>, now_boot_ns: u64) {
        let mut rebuilt: BTreeMap<EntityId, EntityState> = BTreeMap::new();
        for draft in drafts {
            if rebuilt.contains_key(&draft.entity_id) {
                continue;
            }
            let generation = draft
                .device_key
                .as_deref()
                .map_or(1, |key| self.generation_of(key));
            let discovered_boot_ns = self
                .entities
                .get(&draft.entity_id)
                .filter(|state| state.record.generation == generation)
                .map_or(now_boot_ns, |state| state.record.discovered_boot_ns);
            let mut attributes = draft.attributes;
            attributes.truncate(MAX_ATTRIBUTES);
            let record = EntityRecord {
                entity_id: draft.entity_id.clone(),
                kind: draft.kind,
                parent_id: draft.parent_id,
                generation,
                discovered_boot_ns,
                removed_boot_ns: None,
                display_name: draft.display_name,
                attributes,
            };
            rebuilt.insert(
                draft.entity_id,
                EntityState {
                    record,
                    owned: draft.owned,
                },
            );
        }
        self.entities = rebuilt;
    }

    /// Forgets departed sources the parent no longer holds.
    pub(crate) fn prune_departed(&mut self, still_held: impl Fn(&SourceId) -> bool) {
        self.departed.retain(|source_id, _| still_held(source_id));
    }

    /// Resolves plan slots (empty = every current source).
    pub(crate) fn targets(&self, slots: &[u32], held: impl Fn(&SourceId) -> bool) -> ReadTargets {
        if slots.is_empty() {
            return ReadTargets {
                current: self.sources.keys().cloned().collect(),
                departed: self
                    .departed
                    .keys()
                    .filter(|source_id| held(source_id))
                    .cloned()
                    .collect(),
                unknown_slots: Vec::new(),
            };
        }
        let mut targets = ReadTargets::default();
        for slot in slots {
            if let Some(source_id) = self.by_slot.get(slot) {
                targets.current.push(source_id.clone());
            } else if let Some((source_id, _)) = self
                .departed
                .iter()
                .find(|(source_id, entry)| entry.descriptor.slot == *slot && held(source_id))
            {
                targets.departed.push(source_id.clone());
            } else {
                targets.unknown_slots.push(*slot);
            }
        }
        targets
    }

    /// Reads one current source, honouring unsupported re-probe throttling.
    pub(crate) fn read_one(
        &mut self,
        files: &dyn SystemFiles,
        source_id: &SourceId,
        now_boot_ns: u64,
    ) -> Result<Reading, ReadFailure> {
        let Some(entry) = self.sources.get_mut(source_id) else {
            return Err(ReadFailure::new(ReadStatus::Removed, SOURCE_REMOVED));
        };
        let device_key = &entry.descriptor.device_id;
        if let SourceSupport::Unsupported { status, reason } = &entry.support {
            let may_probe = self.devices.get_mut(device_key).is_none_or(|device| {
                device
                    .throttle
                    .should_probe(source_id.as_str(), now_boot_ns)
            });
            if !may_probe {
                return Err(ReadFailure {
                    status: *status,
                    reason: reason.clone(),
                });
            }
        }
        let result = read_source(files, &entry.read);
        match &result {
            Ok(_) => entry.support = SourceSupport::Supported,
            Err(failure) => match failure.status {
                ReadStatus::Unsupported | ReadStatus::PermissionDenied => {
                    entry.support = SourceSupport::Unsupported {
                        status: failure.status,
                        reason: failure.reason.clone(),
                    };
                    if let Some(device) = self.devices.get_mut(device_key) {
                        // Registers the failed probe so the next one waits a full interval.
                        let _ = device
                            .throttle
                            .should_probe(source_id.as_str(), now_boot_ns);
                    }
                }
                ReadStatus::Removed => {
                    self.suspects.insert(device_key.clone());
                }
                _ => {}
            },
        }
        result
    }

    /// Discovery-time capability probe of unprobed (and due unsupported) sources.
    pub(crate) fn probe_capabilities(
        &mut self,
        files: &dyn SystemFiles,
        now: impl Fn() -> u64,
        deadline_boot_ns: u64,
    ) {
        let pending: Vec<SourceId> = self
            .sources
            .iter()
            .filter(|(_, entry)| entry.support != SourceSupport::Supported)
            .map(|(source_id, _)| source_id.clone())
            .collect();
        for source_id in pending {
            let now_boot_ns = now();
            if now_boot_ns > deadline_boot_ns {
                break;
            }
            let _ = self.read_one(files, &source_id, now_boot_ns);
        }
    }

    /// Capabilities aggregated per `(metric_id|scope, entity)`.
    pub(crate) fn capabilities(&self) -> Vec<Capability> {
        let mut merged: BTreeMap<(String, EntityId), CapabilityAccumulator> = BTreeMap::new();
        for entry in self.sources.values() {
            let key = (
                format!("{}|{}", entry.record_key.metric_id, entry.record_key.scope),
                entry.record_key.entity_id.clone(),
            );
            merged
                .entry(key)
                .or_default()
                .merge(&entry.support, entry.descriptor.update_interval_ms);
        }
        merged
            .into_iter()
            .map(|((capability_id, entity_id), accumulator)| {
                accumulator.finish(capability_id, entity_id)
            })
            .collect()
    }

    /// Adapter availability derived from source support.
    pub(crate) fn availability(&self) -> Availability {
        if self.sources.is_empty() {
            return Availability::Unavailable;
        }
        let usable = self
            .sources
            .values()
            .any(|entry| !matches!(entry.support, SourceSupport::Unsupported { .. }));
        let denied = self.sources.values().any(|entry| {
            matches!(
                entry.support,
                SourceSupport::Unsupported {
                    status: ReadStatus::PermissionDenied,
                    ..
                }
            )
        });
        if usable {
            Availability::Available
        } else if denied {
            Availability::PermissionDenied
        } else {
            Availability::Unavailable
        }
    }

    /// Human-readable health summary (bounded, no paths).
    pub(crate) fn summary(&self) -> String {
        let unsupported = self
            .sources
            .values()
            .filter(|entry| matches!(entry.support, SourceSupport::Unsupported { .. }))
            .count();
        let devices = self.devices.values().filter(|state| state.present).count();
        format!(
            "{devices} devices, {} sources, {unsupported} unsupported",
            self.sources.len()
        )
    }

    /// Current catalog for publication.
    pub(crate) fn content(&self, error_code: Option<String>) -> CatalogContent {
        CatalogContent {
            sources: self
                .sources
                .values()
                .map(|entry| entry.descriptor.clone())
                .collect(),
            entities: self
                .entities
                .values()
                .map(|state| state.record.clone())
                .collect(),
            removable_entities: self
                .entities
                .iter()
                .filter(|(_, state)| state.owned)
                .map(|(entity_id, _)| entity_id.clone())
                .collect(),
            capabilities: self.capabilities(),
            availability: self.availability(),
            error_code,
        }
    }
}

#[derive(Debug, Default)]
struct CapabilityAccumulator {
    supported: bool,
    unknown: bool,
    denied: bool,
    unsupported_reason: Option<String>,
    interval_ms: u32,
}

impl CapabilityAccumulator {
    fn merge(&mut self, support: &SourceSupport, update_interval_ms: Option<u32>) {
        self.interval_ms = self
            .interval_ms
            .max(update_interval_ms.unwrap_or(0))
            .max(MIN_CAPABILITY_INTERVAL_MS);
        match support {
            SourceSupport::Supported => self.supported = true,
            SourceSupport::Unknown => self.unknown = true,
            SourceSupport::Unsupported { status, reason } => {
                if *status == ReadStatus::PermissionDenied {
                    self.denied = true;
                }
                if self.unsupported_reason.is_none() {
                    self.unsupported_reason = Some(reason.clone());
                }
            }
        }
    }

    fn finish(self, capability_id: String, entity_id: EntityId) -> Capability {
        let (support, reason) = if self.supported {
            (Support::Supported, None)
        } else if self.unknown {
            (Support::Unknown, None)
        } else if self.denied {
            (Support::Unsupported, Some(PERMISSION_DENIED.to_owned()))
        } else {
            (Support::Unsupported, self.unsupported_reason)
        };
        Capability {
            capability_id,
            entity_id: Some(entity_id),
            support,
            reason,
            min_interval_ms: Some(self.interval_ms),
        }
    }
}
