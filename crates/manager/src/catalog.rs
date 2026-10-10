//! DeviceCatalog: merges discovery pages (`CatalogDelta`) of every adapter
//! into one immutable [`CatalogSnapshot`].
//!
//! * Upserts apply page by page; removals are explicit and are committed only
//!   by a page with `complete == true` (a partial round never deletes sources
//!   it did not report).
//! * The total number of sources (and entities) is bounded by
//!   `store.max_sources`; removed sources leave a compact tombstone (bounded by
//!   the same number) so late records referencing them are recognised instead
//!   of triggering rediscovery.
//! * Every change rebuilds the snapshot once (with `regroup()`) and bumps the
//!   catalog revision.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use dgxtop_core::canonical::CatalogSnapshot;
use dgxtop_core::identity::{AdapterId, BootId, EntityId, HostId, SourceId};
use dgxtop_core::observation::{
    Capability, CatalogDelta, EntityRecord, MAX_CATALOG_PAGE, SourceDescriptor, split_page,
};
use dgxtop_core::quality::Availability;

/// Capabilities kept per adapter.
const MAX_CAPABILITIES_PER_ADAPTER: usize = 512;

/// What the catalog knows about a source id seen in a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKnowledge {
    Known,
    /// Removed by a complete discovery round (tombstone).
    Removed,
    Unknown,
}

/// A published catalog change (`CatalogChanged` event payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CatalogChange {
    pub(crate) revision: u32,
    pub(crate) changed: u32,
    pub(crate) removed: u32,
}

/// Result of merging one discovery page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct MergeOutcome {
    pub(crate) change: Option<CatalogChange>,
    pub(crate) complete: bool,
    pub(crate) next_page: Option<u32>,
    pub(crate) availability: Option<Availability>,
    /// New sources/entities refused because `store.max_sources` was reached.
    pub(crate) dropped_over_capacity: u32,
    /// The page violated the catalog contract and was ignored.
    pub(crate) rejected: Option<String>,
}

#[derive(Debug, Default)]
struct AdapterCatalog {
    generation: u32,
    availability: Option<Availability>,
    sources: BTreeMap<SourceId, Arc<SourceDescriptor>>,
    entities: BTreeMap<EntityId, Arc<EntityRecord>>,
    capabilities: BTreeMap<(String, Option<EntityId>), Capability>,
    pending_removed_sources: BTreeSet<SourceId>,
    pending_removed_entities: BTreeSet<EntityId>,
}

#[derive(Debug, Default)]
struct PageChanges {
    changed: u32,
    removed_sources: Vec<SourceId>,
    removed_entities: u32,
    added_sources: Vec<SourceId>,
    dropped: u32,
}

#[derive(Debug)]
pub(crate) struct DeviceCatalog {
    host_id: HostId,
    boot_id: BootId,
    max_sources: usize,
    adapters: BTreeMap<AdapterId, AdapterCatalog>,
    tombstones: VecDeque<SourceId>,
    tombstone_set: BTreeSet<SourceId>,
    revision: u32,
    snapshot: Arc<CatalogSnapshot>,
}

impl DeviceCatalog {
    pub(crate) fn new(host_id: HostId, boot_id: BootId, max_sources: u32) -> Self {
        let snapshot = CatalogSnapshot {
            revision: 1,
            host_id: Some(host_id.clone()),
            boot_id: Some(boot_id.clone()),
            ..CatalogSnapshot::default()
        };
        Self {
            host_id,
            boot_id,
            max_sources: usize::try_from(max_sources).unwrap_or(usize::MAX).max(1),
            adapters: BTreeMap::new(),
            tombstones: VecDeque::new(),
            tombstone_set: BTreeSet::new(),
            revision: 1,
            snapshot: Arc::new(snapshot),
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<CatalogSnapshot> {
        Arc::clone(&self.snapshot)
    }

    pub(crate) fn revision(&self) -> u32 {
        self.revision
    }

    /// Catalog generation of the last complete round of an adapter.
    pub(crate) fn generation(&self, adapter_id: &AdapterId) -> u32 {
        self.adapters
            .get(adapter_id)
            .map_or(0, |catalog| catalog.generation)
    }

    /// Last availability an adapter's discovery reported.
    pub(crate) fn availability(&self, adapter_id: &AdapterId) -> Option<Availability> {
        self.adapters
            .get(adapter_id)
            .and_then(|catalog| catalog.availability)
    }

    pub(crate) fn knows(&self, source_id: &SourceId) -> SourceKnowledge {
        if self.snapshot.sources.contains_key(source_id) {
            SourceKnowledge::Known
        } else if self.tombstone_set.contains(source_id) {
            SourceKnowledge::Removed
        } else {
            SourceKnowledge::Unknown
        }
    }

    /// Base read groups of the adapter's sources (page suffix removed).
    pub(crate) fn read_groups_of(&self, adapter_id: &AdapterId) -> BTreeSet<String> {
        self.adapters
            .get(adapter_id)
            .map(|catalog| {
                catalog
                    .sources
                    .values()
                    .map(|source| split_page(source.read_group.as_str()).0.to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Drops removals buffered by an unfinished discovery round.
    pub(crate) fn abandon_round(&mut self, adapter_id: &AdapterId) {
        if let Some(catalog) = self.adapters.get_mut(adapter_id) {
            catalog.pending_removed_sources.clear();
            catalog.pending_removed_entities.clear();
        }
    }

    pub(crate) fn merge(&mut self, adapter_id: &AdapterId, delta: CatalogDelta) -> MergeOutcome {
        if let Some(reason) = Self::contract_violation(adapter_id, &delta) {
            self.abandon_round(adapter_id);
            return MergeOutcome {
                rejected: Some(reason),
                ..MergeOutcome::default()
            };
        }
        let mut outcome = MergeOutcome {
            complete: delta.complete,
            next_page: delta.next_page,
            availability: delta.availability,
            ..MergeOutcome::default()
        };
        let free_sources = self.max_sources.saturating_sub(self.total_sources());
        let free_entities = self.max_sources.saturating_sub(self.total_entities());
        let catalog = self.adapters.entry(adapter_id.clone()).or_default();
        let changes = apply_page(catalog, delta, free_sources, free_entities);
        outcome.dropped_over_capacity = changes.dropped;
        for source_id in &changes.added_sources {
            if self.tombstone_set.remove(source_id) {
                self.tombstones.retain(|tombstone| tombstone != source_id);
            }
        }
        for source_id in &changes.removed_sources {
            self.add_tombstone(source_id.clone());
        }
        let removed = u32::try_from(changes.removed_sources.len())
            .unwrap_or(u32::MAX)
            .saturating_add(changes.removed_entities);
        if changes.changed > 0 || removed > 0 {
            self.rebuild();
            outcome.change = Some(CatalogChange {
                revision: self.revision,
                changed: changes.changed,
                removed,
            });
        }
        outcome
    }

    fn contract_violation(adapter_id: &AdapterId, delta: &CatalogDelta) -> Option<String> {
        if let Err(error) = delta.validate(MAX_CATALOG_PAGE) {
            return Some(error.to_string());
        }
        if delta.adapter_id.as_ref().is_some_and(|id| id != adapter_id) {
            return Some("catalog page names another adapter".to_owned());
        }
        if delta
            .sources
            .iter()
            .any(|source| &source.adapter_id != adapter_id)
        {
            return Some("catalog page carries sources of another adapter".to_owned());
        }
        None
    }

    fn total_sources(&self) -> usize {
        self.adapters
            .values()
            .map(|catalog| catalog.sources.len())
            .sum()
    }

    fn total_entities(&self) -> usize {
        self.adapters
            .values()
            .map(|catalog| catalog.entities.len())
            .sum()
    }

    fn add_tombstone(&mut self, source_id: SourceId) {
        if !self.tombstone_set.insert(source_id.clone()) {
            return;
        }
        self.tombstones.push_back(source_id);
        while self.tombstones.len() > self.max_sources {
            if let Some(evicted) = self.tombstones.pop_front() {
                self.tombstone_set.remove(&evicted);
            }
        }
    }

    fn rebuild(&mut self) {
        self.revision = self.revision.saturating_add(1);
        let mut snapshot = CatalogSnapshot {
            revision: self.revision,
            host_id: Some(self.host_id.clone()),
            boot_id: Some(self.boot_id.clone()),
            ..CatalogSnapshot::default()
        };
        for (adapter_id, catalog) in &self.adapters {
            snapshot.sources.extend(
                catalog
                    .sources
                    .iter()
                    .map(|(id, source)| (id.clone(), Arc::clone(source))),
            );
            snapshot.entities.extend(
                catalog
                    .entities
                    .iter()
                    .map(|(id, entity)| (id.clone(), Arc::clone(entity))),
            );
            if !catalog.capabilities.is_empty() {
                snapshot.capabilities.insert(
                    adapter_id.clone(),
                    catalog.capabilities.values().cloned().collect(),
                );
            }
        }
        snapshot.regroup();
        self.snapshot = Arc::new(snapshot);
    }
}

fn apply_page(
    catalog: &mut AdapterCatalog,
    delta: CatalogDelta,
    mut free_sources: usize,
    mut free_entities: usize,
) -> PageChanges {
    let mut changes = PageChanges::default();
    if let Some(availability) = delta.availability
        && catalog.availability != Some(availability)
    {
        catalog.availability = Some(availability);
    }
    for entity in delta.entities {
        let is_new = !catalog.entities.contains_key(&entity.entity_id);
        if is_new && free_entities == 0 {
            changes.dropped = changes.dropped.saturating_add(1);
            continue;
        }
        if upsert(&mut catalog.entities, entity.entity_id.clone(), entity) {
            changes.changed = changes.changed.saturating_add(1);
            if is_new {
                free_entities -= 1;
            }
        }
    }
    for source in delta.sources {
        let is_new = !catalog.sources.contains_key(&source.source_id);
        if is_new && free_sources == 0 {
            changes.dropped = changes.dropped.saturating_add(1);
            continue;
        }
        let source_id = source.source_id.clone();
        if upsert(&mut catalog.sources, source_id.clone(), source) {
            changes.changed = changes.changed.saturating_add(1);
            if is_new {
                free_sources -= 1;
                changes.added_sources.push(source_id);
            }
        }
    }
    for capability in delta.capabilities {
        let key = (
            capability.capability_id.clone(),
            capability.entity_id.clone(),
        );
        let fits = catalog.capabilities.contains_key(&key)
            || catalog.capabilities.len() < MAX_CAPABILITIES_PER_ADAPTER;
        if fits && catalog.capabilities.get(&key) != Some(&capability) {
            catalog.capabilities.insert(key, capability);
            changes.changed = changes.changed.saturating_add(1);
        }
    }
    catalog
        .pending_removed_sources
        .extend(delta.removed_sources);
    catalog
        .pending_removed_entities
        .extend(delta.removed_entities);
    if delta.complete {
        commit_removals(catalog, &mut changes);
        catalog.generation = delta.generation;
    }
    changes
}

fn commit_removals(catalog: &mut AdapterCatalog, changes: &mut PageChanges) {
    for source_id in std::mem::take(&mut catalog.pending_removed_sources) {
        if catalog.sources.remove(&source_id).is_some() {
            changes.removed_sources.push(source_id);
        }
    }
    for entity_id in std::mem::take(&mut catalog.pending_removed_entities) {
        if catalog.entities.remove(&entity_id).is_some() {
            changes.removed_entities = changes.removed_entities.saturating_add(1);
        }
    }
}

/// Inserts or replaces; returns whether the stored value changed.
fn upsert<K: Ord, V: PartialEq>(map: &mut BTreeMap<K, Arc<V>>, key: K, value: V) -> bool {
    match map.get(&key) {
        Some(existing) if **existing == value => false,
        _ => {
            map.insert(key, Arc::new(value));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::source_descriptor;
    use dgxtop_core::testing::{adapter_id, boot_id, host_id, metric_key, source_id};

    fn catalog(max_sources: u32) -> DeviceCatalog {
        DeviceCatalog::new(host_id("test-host"), boot_id("fake-boot"), max_sources)
    }

    fn page(sources: Vec<SourceDescriptor>) -> CatalogDelta {
        CatalogDelta {
            adapter_id: Some(adapter_id("hwmon")),
            generation: 1,
            availability: Some(Availability::Available),
            sources,
            complete: true,
            ..CatalogDelta::default()
        }
    }

    fn temp(name: &str, slot: u32) -> SourceDescriptor {
        source_descriptor(
            "hwmon",
            name,
            slot,
            "fast",
            Some(metric_key(
                "cpu.package.0",
                "cpu.temperature.package",
                "package:0",
            )),
        )
    }

    #[test]
    fn pages_merge_and_removals_wait_for_the_complete_page() {
        let mut catalog = catalog(64);
        let hwmon = adapter_id("hwmon");
        let first = catalog.merge(
            &hwmon,
            page(vec![temp("hwmon:a:t1", 1), temp("hwmon:a:t2", 2)]),
        );
        let change = first.change.unwrap();
        assert_eq!(change.revision, 2);
        assert_eq!(change.changed, 2);
        assert_eq!(
            catalog.snapshot().by_metric.values().next().unwrap().len(),
            2
        );

        // Round 2, page 0: reports t1 only and removes t2, but is not complete.
        let mut partial = page(vec![temp("hwmon:a:t1", 1)]);
        partial.complete = false;
        partial.next_page = Some(1);
        partial.generation = 2;
        partial.removed_sources = vec![source_id("hwmon:a:t2")];
        let outcome = catalog.merge(&hwmon, partial);
        assert!(outcome.change.is_none(), "nothing changed yet");
        assert_eq!(outcome.next_page, Some(1));
        assert_eq!(
            catalog.knows(&source_id("hwmon:a:t2")),
            SourceKnowledge::Known
        );

        // Final page commits the removal and the generation.
        let mut last = page(vec![]);
        last.generation = 2;
        let outcome = catalog.merge(&hwmon, last);
        let change = outcome.change.unwrap();
        assert_eq!(change.removed, 1);
        assert_eq!(change.revision, 3);
        assert_eq!(catalog.generation(&hwmon), 2);
        assert_eq!(
            catalog.knows(&source_id("hwmon:a:t2")),
            SourceKnowledge::Removed
        );
        assert_eq!(
            catalog.knows(&source_id("hwmon:a:zz")),
            SourceKnowledge::Unknown
        );
    }

    #[test]
    fn abandoned_rounds_forget_buffered_removals() {
        let mut catalog = catalog(64);
        let hwmon = adapter_id("hwmon");
        catalog.merge(&hwmon, page(vec![temp("hwmon:a:t1", 1)]));
        let mut partial = page(vec![]);
        partial.complete = false;
        partial.next_page = Some(1);
        partial.removed_sources = vec![source_id("hwmon:a:t1")];
        catalog.merge(&hwmon, partial);
        catalog.abandon_round(&hwmon);
        catalog.merge(&hwmon, page(vec![]));
        assert_eq!(
            catalog.knows(&source_id("hwmon:a:t1")),
            SourceKnowledge::Known
        );
    }

    #[test]
    fn sources_and_tombstones_are_bounded() {
        let mut catalog = catalog(3);
        let hwmon = adapter_id("hwmon");
        let sources: Vec<_> = (0..5u32)
            .map(|index| temp(&format!("hwmon:a:t{index}"), index))
            .collect();
        let outcome = catalog.merge(&hwmon, page(sources));
        assert_eq!(outcome.dropped_over_capacity, 2);
        assert_eq!(catalog.snapshot().sources.len(), 3);
        // Remove and re-add repeatedly: tombstones never exceed max_sources.
        for round in 0..10u32 {
            let mut removal = page(vec![]);
            removal.removed_sources = (0..3u32)
                .map(|index| source_id(&format!("hwmon:a:t{index}")))
                .collect();
            catalog.merge(&hwmon, removal);
            let fresh: Vec<_> = (0..3u32)
                .map(|index| temp(&format!("hwmon:r{round}:t{index}"), 100 + index))
                .collect();
            catalog.merge(&hwmon, page(fresh));
            let mut cleanup = page(vec![]);
            cleanup.removed_sources = (0..3u32)
                .map(|index| source_id(&format!("hwmon:r{round}:t{index}")))
                .collect();
            catalog.merge(&hwmon, cleanup);
            assert!(catalog.tombstones.len() <= 3);
            assert_eq!(catalog.tombstones.len(), catalog.tombstone_set.len());
        }
    }

    #[test]
    fn invalid_or_foreign_pages_are_rejected() {
        let mut catalog = catalog(64);
        let hwmon = adapter_id("hwmon");
        let mut foreign = page(vec![temp("hwmon:a:t1", 1)]);
        foreign.adapter_id = Some(adapter_id("nvml"));
        assert!(catalog.merge(&hwmon, foreign).rejected.is_some());
        let mut contradictory = page(vec![]);
        contradictory.next_page = Some(3);
        assert!(catalog.merge(&hwmon, contradictory).rejected.is_some());
        assert_eq!(catalog.revision(), 1);
    }
}
