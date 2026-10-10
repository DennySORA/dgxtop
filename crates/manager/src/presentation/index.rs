//! Read-only lookups over one committed snapshot, built once per projection
//! so module builders never rescan every domain for each cell.

use std::collections::BTreeMap;

use dgxtop_core::canonical::{CatalogSnapshot, ImmutableSnapshot, SnapshotEntry};
use dgxtop_core::identity::{BootId, EntityId, HostId, MetricId, MetricKey, Scope};

/// `(metric_id, entity_id, scope)`: a series without host and boot, which
/// are fixed within one snapshot.
type SeriesCoordinates<'a> = (&'a str, &'a str, &'a str);

/// Committed samples ordered by metric, then entity, then scope.
pub(crate) struct SampleIndex<'a> {
    entries: BTreeMap<SeriesCoordinates<'a>, &'a SnapshotEntry>,
}

impl<'a> SampleIndex<'a> {
    pub(crate) fn new(snapshot: &'a ImmutableSnapshot) -> Self {
        let mut entries: BTreeMap<SeriesCoordinates<'a>, &'a SnapshotEntry> = BTreeMap::new();
        for domain in snapshot.domains.values() {
            for (key, entry) in &domain.samples {
                let coordinates = (
                    key.metric_id.as_str(),
                    key.entity_id.as_str(),
                    key.scope.as_str(),
                );
                let slot = entries.entry(coordinates).or_insert(entry);
                if is_newer(entry, slot) {
                    *slot = entry;
                }
            }
        }
        Self { entries }
    }

    pub(crate) fn get(&self, metric: &str, entity: &str, scope: &str) -> Option<&'a SnapshotEntry> {
        self.entries.get(&(metric, entity, scope)).copied()
    }

    /// The preferred scope when reported, else the first scope (in order)
    /// reported for the entity.
    pub(crate) fn find(
        &self,
        metric: &str,
        entity: &str,
        preferred_scope: &str,
    ) -> Option<&'a SnapshotEntry> {
        self.get(metric, entity, preferred_scope)
            .or_else(|| self.entries_of_entity(metric, entity).into_iter().next())
    }

    pub(crate) fn entries_of_metric(&self, metric: &str) -> Vec<&'a SnapshotEntry> {
        self.entries
            .range((metric, "", "")..)
            .take_while(|((candidate, _, _), _)| *candidate == metric)
            .map(|(_, entry)| *entry)
            .collect()
    }

    pub(crate) fn entries_of_entity(&self, metric: &str, entity: &str) -> Vec<&'a SnapshotEntry> {
        self.entries
            .range((metric, entity, "")..)
            .take_while(|((candidate_metric, candidate_entity, _), _)| {
                *candidate_metric == metric && *candidate_entity == entity
            })
            .map(|(_, entry)| *entry)
            .collect()
    }

    /// Distinct entities (sorted by id) reporting any of `metrics` whose id
    /// starts with `prefix`.
    pub(crate) fn entities_reporting(&self, metrics: &[&str], prefix: &str) -> Vec<&'a EntityId> {
        let mut entities: BTreeMap<&'a str, &'a EntityId> = BTreeMap::new();
        for metric in metrics {
            for entry in self.entries_of_metric(metric) {
                let entity = &entry.sample.key.entity_id;
                if entity.as_str().starts_with(prefix) {
                    entities.entry(entity.as_str()).or_insert(entity);
                }
            }
        }
        entities.into_values().collect()
    }

    /// Distinct entities (sorted by id) whose id starts with `prefix`.
    pub(crate) fn entities_with_prefix(&self, prefix: &str) -> Vec<&'a EntityId> {
        let mut entities: BTreeMap<&'a str, &'a EntityId> = BTreeMap::new();
        for entry in self.entries.values() {
            let entity = &entry.sample.key.entity_id;
            if entity.as_str().starts_with(prefix) {
                entities.entry(entity.as_str()).or_insert(entity);
            }
        }
        entities.into_values().collect()
    }

    fn any_key(&self) -> Option<&'a MetricKey> {
        self.entries.values().next().map(|entry| &entry.sample.key)
    }
}

/// Two domains never own the same key; if a transition ever produces one,
/// the most recently committed sample wins deterministically.
fn is_newer(candidate: &SnapshotEntry, current: &SnapshotEntry) -> bool {
    let rank = |entry: &SnapshotEntry| {
        (
            entry.sample.snapshot_revision,
            entry.sample.read_finished_boot_ns,
        )
    };
    rank(candidate) > rank(current)
}

/// Rebuilds full metric keys (for trend lookups) when no sample carries one.
pub(crate) struct KeyResolver {
    origin: Option<(HostId, BootId)>,
}

impl KeyResolver {
    /// Committed samples name the live host/boot best; the catalog is the
    /// fallback before the first commit.
    pub(crate) fn new(catalog: &CatalogSnapshot, index: &SampleIndex<'_>) -> Self {
        let from_samples = index
            .any_key()
            .map(|key| (key.host_id.clone(), key.boot_id.clone()));
        let from_catalog = catalog.host_id.clone().zip(catalog.boot_id.clone());
        Self {
            origin: from_samples.or(from_catalog),
        }
    }

    pub(crate) fn key(&self, metric: &str, entity: &str, scope: &str) -> Option<MetricKey> {
        let (host_id, boot_id) = self.origin.clone()?;
        Some(MetricKey::new(
            host_id,
            boot_id,
            EntityId::new(entity).ok()?,
            MetricId::new(metric).ok()?,
            Scope::new(scope).ok()?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{Fixture, sample};
    use dgxtop_core::testing::{boot_id, host_id};

    fn fixture() -> Fixture {
        let mut fixture = Fixture::new();
        fixture.add(
            "cpu",
            sample("cpu.utilization.total", "cpu", "host").f64(10.0),
        );
        fixture.add(
            "cpu",
            sample("cpu.utilization.total", "cpu.core.1", "core:1").f64(20.0),
        );
        fixture.add(
            "cpu",
            sample("cpu.utilization.total", "cpu.core.0", "core:0").f64(30.0),
        );
        fixture.add(
            "gpu",
            sample("gpu.fan.speed", "gpu:GPU-a", "fan:1").f64(40.0),
        );
        fixture.add(
            "gpu",
            sample("gpu.fan.speed", "gpu:GPU-a", "fan:0").f64(50.0),
        );
        fixture
    }

    #[test]
    fn lookups_by_coordinates_and_metric_ranges() {
        let fixture = fixture();
        let index = SampleIndex::new(&fixture.snapshot);
        assert!(index.get("cpu.utilization.total", "cpu", "host").is_some());
        assert!(
            index
                .get("cpu.utilization.total", "cpu", "core:0")
                .is_none()
        );
        assert_eq!(index.entries_of_metric("cpu.utilization.total").len(), 3);
        assert!(index.entries_of_metric("memory.used").is_empty());
        let fan = index.find("gpu.fan.speed", "gpu:GPU-a", "gpu").unwrap();
        assert_eq!(fan.sample.key.scope.as_str(), "fan:0");
        let cores = index.entities_reporting(&["cpu.utilization.total"], "cpu.core.");
        let names: Vec<&str> = cores.iter().map(|id| id.as_str()).collect();
        assert_eq!(names, vec!["cpu.core.0", "cpu.core.1"]);
        assert_eq!(index.entities_with_prefix("gpu:").len(), 1);
    }

    #[test]
    fn duplicate_coordinates_keep_the_newest_commit() {
        let mut fixture = Fixture::new();
        fixture.add(
            "cpu",
            sample("cpu.load.1m", "host", "host").f64(1.0).revision(1),
        );
        fixture.add(
            "memory",
            sample("cpu.load.1m", "host", "host").f64(2.0).revision(5),
        );
        let index = SampleIndex::new(&fixture.snapshot);
        let entry = index.get("cpu.load.1m", "host", "host").unwrap();
        assert_eq!(entry.sample.value.unwrap().to_f64_lossy(), 2.0);
    }

    #[test]
    fn key_resolver_prefers_samples_then_catalog() {
        let fixture = fixture();
        let index = SampleIndex::new(&fixture.snapshot);
        let resolver = KeyResolver::new(&fixture.catalog, &index);
        let key = resolver.key("memory.used", "memory", "system").unwrap();
        assert_eq!(key.host_id.as_str(), "test-host");

        let empty = Fixture::new();
        let empty_index = SampleIndex::new(&empty.snapshot);
        let mut catalog = CatalogSnapshot::default();
        assert!(
            KeyResolver::new(&catalog, &empty_index)
                .key("memory.used", "memory", "system")
                .is_none()
        );
        catalog.host_id = Some(host_id("catalog-host"));
        catalog.boot_id = Some(boot_id("catalog-boot"));
        let resolver = KeyResolver::new(&catalog, &empty_index);
        let key = resolver.key("memory.used", "memory", "system").unwrap();
        assert_eq!(key.host_id.as_str(), "catalog-host");
        assert!(resolver.key("Bad Metric", "memory", "system").is_none());
    }
}
