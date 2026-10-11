//! Read-only lookups over one committed snapshot. The index itself is the
//! snapshot's own, built once per revision and shared by the frame build,
//! the process history's inputs and the History tab's chart picks (D-84);
//! this view only adapts it to the module builders.

use std::sync::Arc;

use dgxtop_core::canonical::{self, CatalogSnapshot, ImmutableSnapshot, SnapshotEntry};
use dgxtop_core::identity::{BootId, EntityId, HostId, MetricId, MetricKey, Scope};

/// Committed samples ordered by metric, then entity, then scope.
#[derive(Clone, Copy)]
pub(crate) struct SnapshotLookup<'a> {
    shared: &'a canonical::SampleIndex,
}

fn entries(slice: &[Arc<SnapshotEntry>]) -> impl Iterator<Item = &SnapshotEntry> {
    slice.iter().map(Arc::as_ref)
}

impl<'a> SnapshotLookup<'a> {
    pub(crate) fn new(snapshot: &'a ImmutableSnapshot) -> Self {
        Self {
            shared: snapshot.index(),
        }
    }

    pub(crate) fn get(&self, metric: &str, entity: &str, scope: &str) -> Option<&'a SnapshotEntry> {
        self.shared.get(metric, entity, scope)
    }

    /// The preferred scope when reported, else the first scope (in order)
    /// reported for the entity.
    pub(crate) fn find(
        &self,
        metric: &str,
        entity: &str,
        preferred_scope: &str,
    ) -> Option<&'a SnapshotEntry> {
        self.shared.find(metric, entity, preferred_scope)
    }

    pub(crate) fn entries_of_metric(
        &self,
        metric: &str,
    ) -> impl Iterator<Item = &'a SnapshotEntry> + use<'a> {
        entries(self.shared.entries_of_metric(metric))
    }

    pub(crate) fn entries_of_entity(
        &self,
        metric: &str,
        entity: &str,
    ) -> impl Iterator<Item = &'a SnapshotEntry> + use<'a> {
        entries(self.shared.entries_of_entity(metric, entity))
    }

    /// Distinct entities (sorted by id) reporting any of `metrics` whose id
    /// starts with `prefix`.
    pub(crate) fn entities_reporting(&self, metrics: &[&str], prefix: &str) -> Vec<&'a EntityId> {
        let mut entities: Vec<&'a EntityId> = metrics
            .iter()
            .flat_map(|metric| self.shared.entries_of_metric(metric))
            .map(|entry| &entry.sample.key.entity_id)
            .filter(|entity| entity.as_str().starts_with(prefix))
            .collect();
        entities.sort();
        entities.dedup();
        entities
    }

    /// Distinct entities (sorted by id) whose id starts with `prefix`.
    pub(crate) fn entities_with_prefix(&self, prefix: &str) -> Vec<&'a EntityId> {
        self.shared.entities_with_prefix(prefix).iter().collect()
    }

    fn any_key(&self) -> Option<&'a MetricKey> {
        self.shared.any_key()
    }
}

/// Rebuilds full metric keys (for trend lookups) when no sample carries one.
pub(crate) struct KeyResolver {
    origin: Option<(HostId, BootId)>,
}

impl KeyResolver {
    /// Committed samples name the live host/boot best; the catalog is the
    /// fallback before the first commit.
    pub(crate) fn new(catalog: &CatalogSnapshot, index: &SnapshotLookup<'_>) -> Self {
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
        let index = SnapshotLookup::new(&fixture.snapshot);
        assert!(index.get("cpu.utilization.total", "cpu", "host").is_some());
        assert!(
            index
                .get("cpu.utilization.total", "cpu", "core:0")
                .is_none()
        );
        assert_eq!(index.entries_of_metric("cpu.utilization.total").count(), 3);
        assert!(index.entries_of_metric("memory.used").next().is_none());
        let fan = index.find("gpu.fan.speed", "gpu:GPU-a", "gpu").unwrap();
        assert_eq!(fan.sample.key.scope.as_str(), "fan:0");
        let cores = index.entities_reporting(&["cpu.utilization.total"], "cpu.core.");
        let names: Vec<&str> = cores.iter().map(|id| id.as_str()).collect();
        assert_eq!(names, vec!["cpu.core.0", "cpu.core.1"]);
        assert_eq!(index.entities_with_prefix("gpu:").len(), 1);
    }

    #[test]
    fn every_reader_of_a_revision_shares_the_snapshot_index() {
        let fixture = fixture();
        let frame = MetricReaderProbe::of(&fixture);
        let history_tab = MetricReaderProbe::of(&fixture);
        let run_inputs = SnapshotLookup::new(&fixture.snapshot);
        assert!(std::ptr::eq(frame, history_tab));
        assert!(std::ptr::eq(frame, run_inputs.shared));
        assert!(std::ptr::eq(frame, fixture.snapshot.index()));
    }

    /// The shared index behind a reader built as the frame build does.
    struct MetricReaderProbe;

    impl MetricReaderProbe {
        fn of(fixture: &Fixture) -> &canonical::SampleIndex {
            let reader = crate::presentation::reader::MetricReader::new(
                &fixture.snapshot,
                &fixture.catalog,
                &fixture.trends,
                fixture.now_boot_ns,
            );
            reader.index().shared
        }
    }

    #[test]
    fn key_resolver_prefers_samples_then_catalog() {
        let fixture = fixture();
        let index = SnapshotLookup::new(&fixture.snapshot);
        let resolver = KeyResolver::new(&fixture.catalog, &index);
        let key = resolver.key("memory.used", "memory", "system").unwrap();
        assert_eq!(key.host_id.as_str(), "test-host");

        let empty = Fixture::new();
        let empty_index = SnapshotLookup::new(&empty.snapshot);
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
