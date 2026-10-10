//! Catalog bookkeeping shared by the procfs, network and filesystem adapters
//! (a candidate for `common.rs`; it needs slot release, which
//! `common::SlotInterner` does not offer).
//!
//! * Incarnations: an entity keeps its generation while its fingerprint (the
//!   identity-defining static facts) is unchanged. A changed fingerprint, or
//!   a reappearance after a committed removal, takes a new generation from
//!   one adapter-wide counter, so `(source_id, generation)` and
//!   `(slot, generation)` pairs are never reused.
//! * Slots: stable per source id while the source exists and never handed to
//!   another source; released once the removal is committed, so hotplug churn
//!   keeps every map bounded by the live catalog.
//! * Source limit: sources beyond the limit are dropped by admission rank
//!   (committed sources first within a rank) and reported as a capability
//!   limitation instead of growing memory.
//! * Paging: page 0 takes a fresh snapshot and later pages slice it. Upserts
//!   are computed against the committed catalog (re-sending is idempotent);
//!   removals are computed against everything the parent may hold and are
//!   sent only with the complete page, which also commits the generation.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::identity::{
    AdapterId, BootId, EntityId, HostId, MetricId, MetricKey, OriginId, OriginIdentity, ProfileId,
    ReadGroup, Scope, SourceId,
};
use dgxtop_core::metric::NativeUnit;
use dgxtop_core::observation::{
    Attribute, Capability, CatalogDelta, DiscoverRequest, EntityKind, EntityRecord, MAX_ATTRIBUTES,
    SourceDescriptor,
};
use dgxtop_core::quality::{Availability, RawKind, SampleKind, Support};
use dgxtop_core::text::sanitize_display;

use super::mechanics::joined_error_code;

/// Generations must fit the schema's `i32` range.
const MAX_GENERATION: u32 = i32::MAX.unsigned_abs();
const MAX_DISPLAY_BYTES: usize = 256;
const MAX_ATTRIBUTE_KEY_BYTES: usize = 64;
const MAX_ATTRIBUTE_VALUE_BYTES: usize = 256;
const MAX_LABEL_BYTES: usize = 256;
const MAX_DEVICE_PATH_BYTES: usize = 1024;

/// Static configuration of one adapter's catalog.
#[derive(Debug, Clone)]
pub(crate) struct CatalogConfig {
    pub adapter_id: AdapterId,
    pub profile_id: ProfileId,
    pub profile_revision: u32,
    pub source_limit: usize,
    pub page_size: usize,
}

/// Generation and first-seen time of one device incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Incarnation {
    pub generation: u32,
    pub discovered_boot_ns: u64,
}

/// Host and boot identity shared by every metric key of a snapshot.
#[derive(Debug, Clone)]
pub(crate) struct KeyContext {
    pub host_id: HostId,
    pub boot_id: BootId,
}

/// Entity fields supplied by an adapter.
#[derive(Debug, Clone)]
pub(crate) struct EntityParts {
    pub entity_id: EntityId,
    pub kind: EntityKind,
    pub parent_id: Option<EntityId>,
    pub display_name: String,
    pub attributes: Vec<Attribute>,
}

/// Source fields supplied by an adapter; strings are sanitized and bounded
/// when the draft is built.
#[derive(Debug, Clone)]
pub(crate) struct SourceSpec<'a> {
    pub device_id: &'a str,
    pub channel: &'a str,
    pub entity: &'a EntityId,
    pub scope: &'a Scope,
    pub metric_id: &'a str,
    pub group: &'a str,
    pub native_unit: &'a str,
    pub raw_kind: RawKind,
    pub driver: &'a str,
    pub device_path: &'a str,
    /// Confirmed physical origin (`procfs:<file>:<channel>` style).
    pub origin: &'a str,
    pub update_interval_ms: Option<u32>,
    pub label: Option<&'a str>,
    /// Admission priority under the source limit (lower is kept first).
    pub rank: u8,
}

/// One source of a snapshot before slot assignment.
#[derive(Debug, Clone)]
pub(crate) struct SourceDraft<L> {
    descriptor: SourceDescriptor,
    rank: u8,
    locator: L,
}

/// Builds a draft; `None` when an identity derived from device names is
/// invalid (oversized or control characters).
pub(crate) fn source_draft<L>(
    config: &CatalogConfig,
    keys: &KeyContext,
    spec: &SourceSpec<'_>,
    locator: L,
) -> Option<SourceDraft<L>> {
    Some(SourceDraft {
        descriptor: descriptor_for(config, keys, spec)?,
        rank: spec.rank,
        locator,
    })
}

/// Descriptor for `spec` (slot 0, generation 0 until the catalog assigns
/// them); also used for uncatalogued process records.
pub(crate) fn descriptor_for(
    config: &CatalogConfig,
    keys: &KeyContext,
    spec: &SourceSpec<'_>,
) -> Option<SourceDescriptor> {
    let source_id =
        dgxtop_core::naming::source_id(config.adapter_id.as_str(), spec.device_id, spec.channel)
            .ok()?;
    let metric = MetricKey::new(
        keys.host_id.clone(),
        keys.boot_id.clone(),
        spec.entity.clone(),
        MetricId::new(spec.metric_id).ok()?,
        spec.scope.clone(),
    );
    Some(SourceDescriptor {
        source_id,
        adapter_id: config.adapter_id.clone(),
        slot: 0,
        generation: 0,
        entity_id: spec.entity.clone(),
        device_id: sanitize_display(spec.device_id, MAX_LABEL_BYTES),
        channel: sanitize_display(spec.channel, MAX_LABEL_BYTES),
        metric: Some(metric),
        origin: OriginIdentity::confirmed(OriginId::new(spec.origin).ok()?),
        driver: spec.driver.to_owned(),
        api_version: None,
        device_path: sanitize_display(spec.device_path, MAX_DEVICE_PATH_BYTES),
        native_unit: NativeUnit::new(spec.native_unit).ok()?,
        raw_kind: spec.raw_kind,
        sample_kind: SampleKind::Measured,
        read_group: ReadGroup::new(spec.group).ok()?,
        update_interval_ms: spec.update_interval_ms,
        profile_id: config.profile_id.clone(),
        profile_revision: config.profile_revision,
        profile_rank: 0,
        label: spec
            .label
            .map(|label| sanitize_display(label, MAX_LABEL_BYTES)),
        counter_modulus: None,
    })
}

/// A catalogued source and how the adapter reads it.
#[derive(Debug, Clone)]
pub(crate) struct LiveSource<L> {
    pub descriptor: SourceDescriptor,
    pub locator: L,
    rank: u8,
}

#[derive(Debug, Clone)]
struct LiveEntity<L> {
    record: EntityRecord,
    family: &'static str,
    sources: Vec<LiveSource<L>>,
}

/// A source chosen by a read plan.
#[derive(Debug)]
pub(crate) enum Selected<'a, L> {
    /// Present in the latest snapshot.
    Live(&'a LiveSource<L>),
    /// Known to the parent but gone from the latest snapshot.
    Removed(&'a SourceDescriptor),
}

/// Sources chosen by a read plan plus slots this catalog does not know.
#[derive(Debug)]
pub(crate) struct Selection<'a, L> {
    pub sources: Vec<Selected<'a, L>>,
    pub unknown_slots: usize,
}

/// Size of the bookkeeping, for diagnostics and boundedness tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CatalogStats {
    /// Committed catalog generation (0 before the first complete discovery).
    pub generation: u32,
    pub entities: usize,
    pub sources: usize,
    /// Slot assignments currently held (bounded by live + committed sources).
    pub slot_table: usize,
    /// Incarnation records currently held.
    pub incarnations: usize,
    /// Next slot number; slot numbers below it are never handed out again.
    pub next_slot: u32,
}

#[derive(Debug, Clone)]
struct IncarnationRecord {
    fingerprint: String,
    incarnation: Incarnation,
}

#[derive(Debug, Default)]
struct SlotTable {
    slots: BTreeMap<SourceId, u32>,
    next: u32,
}

impl SlotTable {
    fn slot_for(&mut self, source: &SourceId) -> Option<u32> {
        if let Some(slot) = self.slots.get(source) {
            return Some(*slot);
        }
        let slot = self.next;
        self.next = self.next.checked_add(1)?;
        self.slots.insert(source.clone(), slot);
        Some(slot)
    }
}

#[derive(Debug, Default)]
struct Committed {
    generation: u32,
    entities: BTreeMap<EntityId, EntityRecord>,
    sources: BTreeMap<SourceId, SourceDescriptor>,
    slots: BTreeMap<u32, SourceId>,
}

#[derive(Debug)]
struct PendingDiscovery {
    previous_generation: u32,
    target_generation: u32,
    availability: Availability,
    entities: Vec<EntityRecord>,
    sources: Vec<SourceDescriptor>,
    removed_entities: Vec<EntityId>,
    removed_sources: Vec<SourceId>,
    capabilities: Vec<Capability>,
    error_code: Option<String>,
    page_count: u32,
    committed: bool,
}

/// Catalog state of one adapter.
#[derive(Debug)]
pub(crate) struct CatalogBook<L> {
    config: CatalogConfig,
    slots: SlotTable,
    incarnations: BTreeMap<EntityId, IncarnationRecord>,
    next_generation: u32,
    live: Vec<LiveEntity<L>>,
    live_slots: BTreeMap<u32, (usize, usize)>,
    committed: Committed,
    announced_entities: BTreeSet<EntityId>,
    announced_sources: BTreeSet<SourceId>,
    /// Entities of the latest enumeration, admitted or not: their
    /// incarnations are kept so entities over the source limit do not take
    /// a fresh generation on every discovery.
    last_seen: BTreeSet<EntityId>,
    pending: Option<PendingDiscovery>,
    discovered: bool,
}

impl<L: Clone> CatalogBook<L> {
    pub(crate) fn new(config: CatalogConfig) -> Self {
        Self {
            config,
            slots: SlotTable::default(),
            incarnations: BTreeMap::new(),
            next_generation: 1,
            live: Vec::new(),
            live_slots: BTreeMap::new(),
            committed: Committed::default(),
            announced_entities: BTreeSet::new(),
            announced_sources: BTreeSet::new(),
            last_seen: BTreeSet::new(),
            pending: None,
            discovered: false,
        }
    }

    pub(crate) fn config(&self) -> &CatalogConfig {
        &self.config
    }

    /// Whether any discovery snapshot exists (reads need one).
    pub(crate) fn is_discovered(&self) -> bool {
        self.discovered
    }

    /// Starts a page-0 discovery snapshot.
    pub(crate) fn begin_snapshot(&mut self, now_boot_ns: u64) -> SnapshotBuilder<'_, L> {
        SnapshotBuilder {
            book: self,
            now_boot_ns,
            entities: Vec::new(),
            seen: BTreeSet::new(),
            capabilities: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// Serves page `request.page` of the pending snapshot.
    pub(crate) fn serve_page(&mut self, request: &DiscoverRequest) -> CatalogDelta {
        let page_size = self.config.page_size;
        let delta = match &self.pending {
            Some(pending)
                if pending.previous_generation == request.previous_generation
                    && request.page < pending.page_count =>
            {
                pending.page(request, &self.config.adapter_id, page_size)
            }
            _ => return self.unavailable_page(request, "DISCOVERY_PAGE_UNAVAILABLE"),
        };
        if delta.complete {
            self.commit_pending();
        } else {
            self.announce(&delta);
        }
        delta
    }

    /// Delta carrying only an error (nothing upserted, nothing removed).
    pub(crate) fn unavailable_page(&self, request: &DiscoverRequest, code: &str) -> CatalogDelta {
        CatalogDelta {
            request_id: Some(request.request_id),
            adapter_id: Some(self.config.adapter_id.clone()),
            generation: self.committed.generation,
            complete: false,
            next_page: None,
            error_code: Some(code.to_owned()),
            ..CatalogDelta::default()
        }
    }

    /// Sources of `group` addressed by `slots` (empty means all live ones).
    pub(crate) fn select(&self, group: &str, slots: &[u32]) -> Selection<'_, L> {
        if slots.is_empty() {
            let sources = self
                .live
                .iter()
                .flat_map(|entity| entity.sources.iter())
                .filter(|source| source.descriptor.read_group.as_str() == group)
                .map(Selected::Live)
                .collect();
            return Selection {
                sources,
                unknown_slots: 0,
            };
        }
        let mut selection = Selection {
            sources: Vec::new(),
            unknown_slots: 0,
        };
        for slot in slots {
            match self.lookup_slot(*slot) {
                Some(selected) if selected_group(&selected) == group => {
                    selection.sources.push(selected);
                }
                Some(_) => {}
                None => selection.unknown_slots += 1,
            }
        }
        selection
    }

    pub(crate) fn stats(&self) -> CatalogStats {
        CatalogStats {
            generation: self.committed.generation,
            entities: self.live.len(),
            sources: self.live.iter().map(|entity| entity.sources.len()).sum(),
            slot_table: self.slots.slots.len(),
            incarnations: self.incarnations.len(),
            next_slot: self.slots.next,
        }
    }

    /// Drops pending pages (shutdown).
    pub(crate) fn clear_pending(&mut self) {
        self.pending = None;
    }

    fn lookup_slot(&self, slot: u32) -> Option<Selected<'_, L>> {
        if let Some((entity, source)) = self.live_slots.get(&slot) {
            let live = self.live.get(*entity)?.sources.get(*source)?;
            return Some(Selected::Live(live));
        }
        let source_id = self.committed.slots.get(&slot)?;
        self.committed.sources.get(source_id).map(Selected::Removed)
    }

    fn allocate_generation(&mut self) -> Option<u32> {
        let generation = self.next_generation;
        if generation > MAX_GENERATION {
            return None;
        }
        self.next_generation = generation.checked_add(1)?;
        Some(generation)
    }

    fn announce(&mut self, delta: &CatalogDelta) {
        for entity in &delta.entities {
            self.announced_entities.insert(entity.entity_id.clone());
        }
        for source in &delta.sources {
            self.announced_sources.insert(source.source_id.clone());
        }
    }

    fn install_live(&mut self, live: Vec<LiveEntity<L>>) {
        self.live_slots.clear();
        for (entity_index, entity) in live.iter().enumerate() {
            for (source_index, source) in entity.sources.iter().enumerate() {
                self.live_slots
                    .insert(source.descriptor.slot, (entity_index, source_index));
            }
        }
        self.live = live;
        self.discovered = true;
    }

    /// Forgets slots and incarnations nobody can reference any more.
    fn prune_unreferenced(&mut self) {
        let mut sources: BTreeSet<SourceId> = self.announced_sources.clone();
        sources.extend(self.committed.sources.keys().cloned());
        sources.extend(self.live_source_ids());
        self.slots.slots.retain(|id, _| sources.contains(id));
        let mut entities: BTreeSet<EntityId> = self.announced_entities.clone();
        entities.extend(self.last_seen.iter().cloned());
        entities.extend(self.committed.entities.keys().cloned());
        entities.extend(
            self.live
                .iter()
                .map(|entity| entity.record.entity_id.clone()),
        );
        self.incarnations.retain(|id, _| entities.contains(id));
    }

    fn live_source_ids(&self) -> impl Iterator<Item = SourceId> + '_ {
        self.live
            .iter()
            .flat_map(|entity| entity.sources.iter())
            .map(|source| source.descriptor.source_id.clone())
    }

    fn commit_pending(&mut self) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        if pending.committed {
            return;
        }
        pending.committed = true;
        self.committed = Committed::from_live(&self.live, pending.target_generation);
        self.announced_entities.clear();
        self.announced_sources.clear();
        self.prune_unreferenced();
    }
}

fn selected_group<'a, L>(selected: &Selected<'a, L>) -> &'a str {
    match selected {
        Selected::Live(source) => source.descriptor.read_group.as_str(),
        Selected::Removed(descriptor) => descriptor.read_group.as_str(),
    }
}

impl Committed {
    fn from_live<L>(live: &[LiveEntity<L>], generation: u32) -> Self {
        let mut committed = Self {
            generation,
            ..Self::default()
        };
        for entity in live {
            committed
                .entities
                .insert(entity.record.entity_id.clone(), entity.record.clone());
            for source in &entity.sources {
                let descriptor = &source.descriptor;
                committed
                    .slots
                    .insert(descriptor.slot, descriptor.source_id.clone());
                committed
                    .sources
                    .insert(descriptor.source_id.clone(), descriptor.clone());
            }
        }
        committed
    }
}

impl PendingDiscovery {
    fn page(
        &self,
        request: &DiscoverRequest,
        adapter_id: &AdapterId,
        page_size: usize,
    ) -> CatalogDelta {
        let size = page_size.max(1);
        let start = usize::try_from(request.page)
            .unwrap_or(usize::MAX)
            .saturating_mul(size);
        let end = start.saturating_add(size);
        let entity_count = self.entities.len();
        let complete = request.page.saturating_add(1) >= self.page_count;
        CatalogDelta {
            request_id: Some(request.request_id),
            adapter_id: Some(adapter_id.clone()),
            generation: self.target_generation,
            availability: Some(self.availability),
            entities: slice(&self.entities, start, end),
            sources: slice(
                &self.sources,
                start.saturating_sub(entity_count),
                end.saturating_sub(entity_count),
            ),
            removed_sources: if complete {
                self.removed_sources.clone()
            } else {
                Vec::new()
            },
            removed_entities: if complete {
                self.removed_entities.clone()
            } else {
                Vec::new()
            },
            capabilities: slice(&self.capabilities, start, end),
            complete,
            next_page: (!complete).then(|| request.page.saturating_add(1)),
            error_code: self.error_code.clone(),
        }
    }
}

fn slice<T: Clone>(items: &[T], start: usize, end: usize) -> Vec<T> {
    let start = start.min(items.len());
    let end = end.clamp(start, items.len());
    items[start..end].to_vec()
}

fn page_count(upserts: usize, capabilities: usize, page_size: usize) -> u32 {
    let size = page_size.max(1);
    let pages = upserts
        .div_ceil(size)
        .max(capabilities.div_ceil(size))
        .max(1);
    u32::try_from(pages).unwrap_or(u32::MAX)
}

/// Collects one page-0 snapshot; `finish` turns it into the live catalog
/// and the first page of the delta.
pub(crate) struct SnapshotBuilder<'book, L> {
    book: &'book mut CatalogBook<L>,
    now_boot_ns: u64,
    entities: Vec<LiveEntity<L>>,
    seen: BTreeSet<EntityId>,
    capabilities: Vec<Capability>,
    errors: Vec<String>,
}

impl<L: Clone> SnapshotBuilder<'_, L> {
    pub(crate) fn config(&self) -> &CatalogConfig {
        &self.book.config
    }

    /// Incarnation of `entity_id`: unchanged fingerprint keeps the
    /// generation, anything else takes a new one. `None` when the generation
    /// space is exhausted.
    pub(crate) fn incarnation(
        &mut self,
        entity_id: &EntityId,
        fingerprint: &str,
    ) -> Option<Incarnation> {
        if let Some(known) = self.book.incarnations.get(entity_id)
            && known.fingerprint == fingerprint
        {
            return Some(known.incarnation);
        }
        let Some(generation) = self.book.allocate_generation() else {
            self.errors.push("GENERATION_SPACE_EXHAUSTED".to_owned());
            return None;
        };
        let incarnation = Incarnation {
            generation,
            discovered_boot_ns: self.now_boot_ns,
        };
        self.book.incarnations.insert(
            entity_id.clone(),
            IncarnationRecord {
                fingerprint: fingerprint.to_owned(),
                incarnation,
            },
        );
        Some(incarnation)
    }

    /// Adds one entity of `family` with its sources.
    pub(crate) fn add(
        &mut self,
        family: &'static str,
        parts: EntityParts,
        incarnation: Incarnation,
        sources: Vec<SourceDraft<L>>,
    ) {
        if !self.seen.insert(parts.entity_id.clone()) {
            self.errors.push("DUPLICATE_ENTITY".to_owned());
            return;
        }
        let record = EntityRecord {
            entity_id: parts.entity_id,
            kind: parts.kind,
            parent_id: parts.parent_id,
            generation: incarnation.generation,
            discovered_boot_ns: incarnation.discovered_boot_ns,
            removed_boot_ns: None,
            display_name: sanitize_display(&parts.display_name, MAX_DISPLAY_BYTES),
            attributes: bounded_attributes(parts.attributes),
        };
        let sources = sources
            .into_iter()
            .map(|draft| {
                let mut descriptor = draft.descriptor;
                descriptor.generation = incarnation.generation;
                LiveSource {
                    descriptor,
                    locator: draft.locator,
                    rank: draft.rank,
                }
            })
            .collect();
        self.entities.push(LiveEntity {
            record,
            family,
            sources,
        });
    }

    /// Keeps the previous snapshot's entities of `family` (its enumeration
    /// failed this time; a failed read is not a disappearance).
    pub(crate) fn carry_over_family(&mut self, family: &str) {
        let carried: Vec<LiveEntity<L>> = self
            .book
            .live
            .iter()
            .filter(|entity| {
                entity.family == family && !self.seen.contains(&entity.record.entity_id)
            })
            .cloned()
            .collect();
        self.push_carried(carried);
    }

    /// Keeps the previous snapshot's entities matching `predicate` (e.g. an
    /// interface whose identity file could not be read this time).
    pub(crate) fn carry_over_matching(&mut self, predicate: impl Fn(&EntityRecord) -> bool) {
        let carried: Vec<LiveEntity<L>> = self
            .book
            .live
            .iter()
            .filter(|entity| {
                predicate(&entity.record) && !self.seen.contains(&entity.record.entity_id)
            })
            .cloned()
            .collect();
        self.push_carried(carried);
    }

    pub(crate) fn capability(&mut self, capability: Capability) {
        self.capabilities.push(capability);
    }

    pub(crate) fn error(&mut self, code: impl Into<String>) {
        self.errors.push(code.into());
    }

    /// Admits sources under the limit, installs the live catalog and serves
    /// page 0 of the delta.
    pub(crate) fn finish(
        self,
        request: &DiscoverRequest,
        availability: Availability,
    ) -> CatalogDelta {
        let SnapshotBuilder {
            book,
            entities,
            seen,
            mut capabilities,
            errors,
            ..
        } = self;
        let live = admit(book, entities, &mut capabilities);
        book.last_seen = seen;
        book.install_live(live);
        book.prune_unreferenced();
        book.pending = Some(plan_delta(
            book,
            request,
            availability,
            capabilities,
            &errors,
        ));
        book.serve_page(request)
    }

    fn push_carried(&mut self, carried: Vec<LiveEntity<L>>) {
        for entity in carried {
            self.seen.insert(entity.record.entity_id.clone());
            self.entities.push(entity);
        }
    }
}

fn bounded_attributes(attributes: Vec<Attribute>) -> Vec<Attribute> {
    attributes
        .into_iter()
        .take(MAX_ATTRIBUTES)
        .map(|attribute| Attribute {
            key: sanitize_display(&attribute.key, MAX_ATTRIBUTE_KEY_BYTES),
            value: sanitize_display(&attribute.value, MAX_ATTRIBUTE_VALUE_BYTES),
        })
        .collect()
}

/// Counts of what admission had to leave out.
#[derive(Debug, Default)]
struct AdmissionTally {
    dropped_entities: usize,
    exhausted_slots: usize,
}

/// Applies the source limit and assigns slots in snapshot order.
fn admit<L: Clone>(
    book: &mut CatalogBook<L>,
    entities: Vec<LiveEntity<L>>,
    capabilities: &mut Vec<Capability>,
) -> Vec<LiveEntity<L>> {
    let admitted = admitted_positions(book, &entities);
    let total_sources: usize = entities.iter().map(|entity| entity.sources.len()).sum();
    let sourced_entities: BTreeSet<usize> = admitted.iter().map(|(entity, _)| *entity).collect();
    // Entities without sources (e.g. skipped network mounts) share what the
    // limit leaves; entities with admitted sources never compete for it.
    let mut sourceless_budget = book
        .config
        .source_limit
        .saturating_sub(sourced_entities.len());
    let mut tally = AdmissionTally::default();
    let mut live = Vec::new();
    for (entity_index, entity) in entities.into_iter().enumerate() {
        let had_sources = !entity.sources.is_empty();
        let entity = keep_admitted_sources(book, entity, entity_index, &admitted, &mut tally);
        let keep = if had_sources {
            !entity.sources.is_empty()
        } else {
            take_one(&mut sourceless_budget)
        };
        if keep {
            live.push(entity);
        } else {
            tally.dropped_entities += 1;
        }
    }
    report_limits(
        capabilities,
        total_sources.saturating_sub(admitted.len()),
        &tally,
    );
    live
}

/// Keeps the admitted sources of one entity and gives each its slot.
fn keep_admitted_sources<L>(
    book: &mut CatalogBook<L>,
    mut entity: LiveEntity<L>,
    entity_index: usize,
    admitted: &BTreeSet<(usize, usize)>,
    tally: &mut AdmissionTally,
) -> LiveEntity<L> {
    let sources = std::mem::take(&mut entity.sources);
    for (source_index, mut source) in sources.into_iter().enumerate() {
        if !admitted.contains(&(entity_index, source_index)) {
            continue;
        }
        let Some(slot) = book.slots.slot_for(&source.descriptor.source_id) else {
            tally.exhausted_slots += 1;
            continue;
        };
        source.descriptor.slot = slot;
        entity.sources.push(source);
    }
    entity
}

fn take_one(budget: &mut usize) -> bool {
    let available = *budget > 0;
    *budget = budget.saturating_sub(1);
    available
}

fn report_limits(
    capabilities: &mut Vec<Capability>,
    dropped_sources: usize,
    tally: &AdmissionTally,
) {
    if dropped_sources > 0 || tally.dropped_entities > 0 {
        capabilities.push(limitation(
            "catalog.source_limit",
            format!(
                "SOURCE_LIMIT_EXCEEDED:sources={dropped_sources},entities={}",
                tally.dropped_entities
            ),
        ));
    }
    if tally.exhausted_slots > 0 {
        capabilities.push(limitation(
            "catalog.slots",
            format!("SLOT_SPACE_EXHAUSTED:{}", tally.exhausted_slots),
        ));
    }
}

/// `(entity, source)` positions kept under the limit: lower rank first, then
/// sources the parent already holds, then snapshot order. Duplicate source
/// ids keep their first occurrence only.
fn admitted_positions<L>(
    book: &CatalogBook<L>,
    entities: &[LiveEntity<L>],
) -> BTreeSet<(usize, usize)> {
    let mut candidates: Vec<(u8, bool, usize, usize, usize)> = Vec::new();
    let mut order = 0usize;
    for (entity_index, entity) in entities.iter().enumerate() {
        for (source_index, source) in entity.sources.iter().enumerate() {
            let is_new = !book
                .committed
                .sources
                .contains_key(&source.descriptor.source_id);
            candidates.push((source.rank, is_new, order, entity_index, source_index));
            order += 1;
        }
    }
    candidates.sort_unstable();
    let mut seen_ids = BTreeSet::new();
    let mut admitted = BTreeSet::new();
    for (_, _, _, entity_index, source_index) in candidates {
        if admitted.len() >= book.config.source_limit {
            break;
        }
        let id = &entities[entity_index].sources[source_index]
            .descriptor
            .source_id;
        if seen_ids.insert(id.clone()) {
            admitted.insert((entity_index, source_index));
        }
    }
    admitted
}

fn limitation(capability_id: &str, reason: String) -> Capability {
    Capability {
        capability_id: capability_id.to_owned(),
        entity_id: None,
        support: Support::Unsupported,
        reason: Some(reason),
        min_interval_ms: None,
    }
}

/// Upserts against the committed catalog, removals against everything the
/// parent may hold, and the generation the complete page will commit.
fn plan_delta<L: Clone>(
    book: &CatalogBook<L>,
    request: &DiscoverRequest,
    availability: Availability,
    capabilities: Vec<Capability>,
    errors: &[String],
) -> PendingDiscovery {
    let committed = &book.committed;
    let resync = request.previous_generation != committed.generation;
    let (entities, entities_changed) = entity_upserts(book, resync);
    let (sources, sources_changed) = source_upserts(book, resync);
    let removed_entities = removed_entities(book);
    let removed_sources = removed_sources(book);
    let changed = committed.generation == 0
        || entities_changed
        || sources_changed
        || !removed_entities.is_empty()
        || !removed_sources.is_empty();
    let target_generation = if changed {
        committed.generation.saturating_add(1)
    } else {
        committed.generation
    };
    let page_count = page_count(
        entities.len() + sources.len(),
        capabilities.len(),
        book.config.page_size,
    );
    PendingDiscovery {
        previous_generation: request.previous_generation,
        target_generation,
        availability,
        entities,
        sources,
        removed_entities,
        removed_sources,
        capabilities,
        error_code: joined_error_code(errors),
        page_count,
        committed: false,
    }
}

/// Live entities differing from the committed ones (all of them on a
/// resync) and whether anything actually changed.
fn entity_upserts<L>(book: &CatalogBook<L>, resync: bool) -> (Vec<EntityRecord>, bool) {
    let mut changed = false;
    let mut upserts = Vec::new();
    for entity in &book.live {
        let differs = book.committed.entities.get(&entity.record.entity_id) != Some(&entity.record);
        changed |= differs;
        if differs || resync {
            upserts.push(entity.record.clone());
        }
    }
    (upserts, changed)
}

/// Live sources differing from the committed ones (all of them on a
/// resync) and whether anything actually changed.
fn source_upserts<L>(book: &CatalogBook<L>, resync: bool) -> (Vec<SourceDescriptor>, bool) {
    let mut changed = false;
    let mut upserts = Vec::new();
    for descriptor in book
        .live
        .iter()
        .flat_map(|entity| entity.sources.iter())
        .map(|source| &source.descriptor)
    {
        let differs = book.committed.sources.get(&descriptor.source_id) != Some(descriptor);
        changed |= differs;
        if differs || resync {
            upserts.push(descriptor.clone());
        }
    }
    (upserts, changed)
}

/// Entities the parent may hold that the snapshot no longer has.
fn removed_entities<L>(book: &CatalogBook<L>) -> Vec<EntityId> {
    let live: BTreeSet<&EntityId> = book
        .live
        .iter()
        .map(|entity| &entity.record.entity_id)
        .collect();
    let gone: BTreeSet<&EntityId> = book
        .committed
        .entities
        .keys()
        .chain(book.announced_entities.iter())
        .filter(|id| !live.contains(id))
        .collect();
    gone.into_iter().cloned().collect()
}

/// Sources the parent may hold that the snapshot no longer has.
fn removed_sources<L>(book: &CatalogBook<L>) -> Vec<SourceId> {
    let live: BTreeSet<&SourceId> = book
        .live
        .iter()
        .flat_map(|entity| entity.sources.iter())
        .map(|source| &source.descriptor.source_id)
        .collect();
    let gone: BTreeSet<&SourceId> = book
        .committed
        .sources
        .keys()
        .chain(book.announced_sources.iter())
        .filter(|id| !live.contains(id))
        .collect();
    gone.into_iter().cloned().collect()
}
