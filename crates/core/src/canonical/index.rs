//! The live index of one snapshot revision: every committed sample ordered
//! by `(metric, entity, scope)`, built at most once per revision and shared
//! by every reader of that revision (D-84).
//!
//! The frame build, the process history's inputs and the History tab's
//! chart picks all read the same revision; before this index each of them
//! rescanned every domain. Host and boot are fixed within one snapshot, so
//! a series is found by its three remaining coordinates.

use std::cmp::Ordering;
use std::fmt;
use std::sync::{Arc, OnceLock};

use super::{DomainSnapshot, SnapshotEntry};
use crate::identity::{CollectorId, EntityId, MetricKey};

/// `(metric_id, entity_id, scope)` of a series.
type Coordinates<'a> = (&'a str, &'a str, &'a str);

fn coordinates(entry: &SnapshotEntry) -> Coordinates<'_> {
    let key = &entry.sample.key;
    (
        key.metric_id.as_str(),
        key.entity_id.as_str(),
        key.scope.as_str(),
    )
}

/// Two domains never own the same key; if a transition ever produces one,
/// the most recently committed sample wins deterministically.
fn recency(entry: &SnapshotEntry) -> (u64, u64) {
    (
        entry.sample.snapshot_revision,
        entry.sample.read_finished_boot_ns,
    )
}

/// Committed samples of one revision, sorted by coordinates, one per series.
#[derive(Debug, Default)]
pub struct SampleIndex {
    entries: Vec<Arc<SnapshotEntry>>,
    /// Distinct entities reporting any sample, sorted by id.
    entities: Vec<EntityId>,
}

impl SampleIndex {
    /// Indexes every sample of `domains` (shared, not copied).
    pub fn new(domains: &std::collections::BTreeMap<CollectorId, Arc<DomainSnapshot>>) -> Self {
        let mut entries: Vec<Arc<SnapshotEntry>> = domains
            .values()
            .flat_map(|domain| domain.samples.values().cloned())
            .collect();
        // Stable: among equally recent duplicates the first domain wins.
        entries.sort_by(|left, right| {
            coordinates(left)
                .cmp(&coordinates(right))
                .then_with(|| recency(right).cmp(&recency(left)))
        });
        entries.dedup_by(|later, kept| coordinates(later) == coordinates(kept));
        let mut entities: Vec<EntityId> = entries
            .iter()
            .map(|entry| entry.sample.key.entity_id.clone())
            .collect();
        entities.sort();
        entities.dedup();
        Self { entries, entities }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, metric: &str, entity: &str, scope: &str) -> Option<&SnapshotEntry> {
        self.entries
            .binary_search_by(|entry| coordinates(entry).cmp(&(metric, entity, scope)))
            .ok()
            .map(|position| self.entries[position].as_ref())
    }

    /// The preferred scope when reported, else the first scope (in order)
    /// reported for the entity.
    pub fn find(
        &self,
        metric: &str,
        entity: &str,
        preferred_scope: &str,
    ) -> Option<&SnapshotEntry> {
        self.get(metric, entity, preferred_scope).or_else(|| {
            self.entries_of_entity(metric, entity)
                .first()
                .map(Arc::as_ref)
        })
    }

    /// Every series of `metric`, by entity then scope.
    pub fn entries_of_metric(&self, metric: &str) -> &[Arc<SnapshotEntry>] {
        self.range(|(candidate, _, _)| candidate.cmp(metric))
    }

    /// Every scope of `metric` reported for `entity`, by scope.
    pub fn entries_of_entity(&self, metric: &str, entity: &str) -> &[Arc<SnapshotEntry>] {
        self.range(|(candidate_metric, candidate_entity, _)| {
            candidate_metric
                .cmp(metric)
                .then_with(|| candidate_entity.cmp(entity))
        })
    }

    /// Distinct entities (sorted by id) whose id starts with `prefix`.
    pub fn entities_with_prefix(&self, prefix: &str) -> &[EntityId] {
        let start = self
            .entities
            .partition_point(|entity| entity.as_str() < prefix);
        let length = self.entities[start..]
            .iter()
            .take_while(|entity| entity.as_str().starts_with(prefix))
            .count();
        &self.entities[start..start + length]
    }

    /// Any committed key: names the snapshot's host and boot.
    pub fn any_key(&self) -> Option<&MetricKey> {
        self.entries.first().map(|entry| &entry.sample.key)
    }

    /// The contiguous entries `order` places at `Equal`.
    fn range(&self, order: impl Fn(Coordinates<'_>) -> Ordering) -> &[Arc<SnapshotEntry>] {
        let start = self
            .entries
            .partition_point(|entry| order(coordinates(entry)) == Ordering::Less);
        let end = self
            .entries
            .partition_point(|entry| order(coordinates(entry)) != Ordering::Greater);
        &self.entries[start..end.max(start)]
    }
}

/// A snapshot's [`SampleIndex`], built by its first reader.
///
/// It is a cache of the snapshot's own samples, not data: equality ignores
/// it, and a clone starts empty because a cloned snapshot may be edited
/// before it is read (a published snapshot never is).
#[derive(Default)]
pub struct SampleIndexCache {
    cell: OnceLock<SampleIndex>,
}

impl SampleIndexCache {
    pub(crate) fn get_or_build(
        &self,
        domains: &std::collections::BTreeMap<CollectorId, Arc<DomainSnapshot>>,
    ) -> &SampleIndex {
        self.cell.get_or_init(|| SampleIndex::new(domains))
    }

    /// Whether a reader has built the index yet.
    pub fn is_built(&self) -> bool {
        self.cell.get().is_some()
    }
}

impl Clone for SampleIndexCache {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for SampleIndexCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl fmt::Debug for SampleIndexCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SampleIndexCache")
            .field("built", &self.is_built())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::canonical::{CanonicalSample, CanonicalValue, ImmutableSnapshot};
    use crate::metric::{CanonicalUnit, Scalar};
    use crate::quality::{Freshness, Quality, SampleKind, SelectionState};
    use crate::testing::metric_key;

    fn entry(metric: &str, entity: &str, scope: &str, value: f64, revision: u64) -> SnapshotEntry {
        let value = CanonicalValue {
            key: metric_key(entity, metric, scope),
            cycle_id: 1,
            source_id: None,
            source_generation: None,
            unit: CanonicalUnit::Percent,
            value: Scalar::from_f64(value).ok(),
            sample_kind: SampleKind::Measured,
            selection_state: SelectionState::Selected,
            quality: Quality::Verified,
            freshness: Freshness::Fresh,
            measurement_freshness: Freshness::Unknown,
            observed_boot_ns: None,
            read_finished_boot_ns: 10,
            policy_revision: 1,
            candidate_source_ids: vec![],
            reason_codes: vec![],
            eligible_for_history: true,
        };
        SnapshotEntry {
            sample: CanonicalSample::commit(value, revision, 11),
            last_good: None,
            stale_after_boot_ns: None,
        }
    }

    fn snapshot(domains: &[(&str, Vec<SnapshotEntry>)]) -> ImmutableSnapshot {
        let domains: BTreeMap<CollectorId, Arc<DomainSnapshot>> = domains
            .iter()
            .map(|(collector, entries)| {
                let samples = entries
                    .iter()
                    .map(|entry| (entry.sample.key.clone(), Arc::new(entry.clone())))
                    .collect();
                let domain = DomainSnapshot {
                    samples,
                    ..DomainSnapshot::default()
                };
                (CollectorId::new(*collector).unwrap(), Arc::new(domain))
            })
            .collect();
        ImmutableSnapshot {
            domains,
            ..ImmutableSnapshot::default()
        }
    }

    fn value_of(entry: &SnapshotEntry) -> f64 {
        entry.sample.value.unwrap().to_f64_lossy()
    }

    #[test]
    fn lookups_by_coordinates_and_ranges() {
        let snapshot = snapshot(&[
            (
                "cpu",
                vec![
                    entry("cpu.utilization.total", "cpu", "host", 10.0, 1),
                    entry("cpu.utilization.total", "cpu.core.1", "core:1", 20.0, 1),
                    entry("cpu.utilization.total", "cpu.core.0", "core:0", 30.0, 1),
                ],
            ),
            (
                "gpu",
                vec![
                    entry("gpu.fan.speed", "gpu:GPU-a", "fan:1", 40.0, 1),
                    entry("gpu.fan.speed", "gpu:GPU-a", "fan:0", 50.0, 1),
                ],
            ),
        ]);
        let index = snapshot.index();
        assert_eq!(index.len(), 5);
        assert!(index.get("cpu.utilization.total", "cpu", "host").is_some());
        assert!(
            index
                .get("cpu.utilization.total", "cpu", "core:0")
                .is_none()
        );
        assert_eq!(index.entries_of_metric("cpu.utilization.total").len(), 3);
        assert!(index.entries_of_metric("memory.used").is_empty());
        assert!(
            index
                .entries_of_entity("gpu.fan.speed", "gpu:GPU-b")
                .is_empty()
        );
        let fan = index.find("gpu.fan.speed", "gpu:GPU-a", "gpu").unwrap();
        assert_eq!(fan.sample.key.scope.as_str(), "fan:0");
        let cores: Vec<&str> = index
            .entities_with_prefix("cpu.core.")
            .iter()
            .map(EntityId::as_str)
            .collect();
        assert_eq!(cores, vec!["cpu.core.0", "cpu.core.1"]);
        assert_eq!(index.entities_with_prefix("gpu:").len(), 1);
        assert!(index.entities_with_prefix("zzz").is_empty());
        assert!(index.any_key().is_some());
    }

    #[test]
    fn duplicate_coordinates_keep_the_newest_commit() {
        let snapshot = snapshot(&[
            ("cpu", vec![entry("cpu.load.1m", "host", "host", 1.0, 1)]),
            ("memory", vec![entry("cpu.load.1m", "host", "host", 2.0, 5)]),
            (
                "network",
                vec![entry("cpu.load.1m", "host", "host", 3.0, 2)],
            ),
        ]);
        let index = snapshot.index();
        assert_eq!(index.len(), 1);
        assert_eq!(
            value_of(index.get("cpu.load.1m", "host", "host").unwrap()),
            2.0
        );
    }

    #[test]
    fn the_index_is_built_once_per_revision_and_shared_by_its_readers() {
        let snapshot = Arc::new(snapshot(&[(
            "cpu",
            vec![entry("cpu.load.1m", "host", "host", 1.0, 1)],
        )]));
        assert!(!snapshot.sample_index.is_built());
        let first = std::ptr::from_ref(snapshot.index()) as usize;
        let reader = Arc::clone(&snapshot);
        let second = std::thread::spawn(move || std::ptr::from_ref(reader.index()) as usize)
            .join()
            .unwrap();
        assert!(snapshot.sample_index.is_built());
        assert_eq!(first, second);
    }

    #[test]
    fn a_cloned_snapshot_rebuilds_its_index_and_compares_equal() {
        let original = snapshot(&[("cpu", vec![entry("cpu.load.1m", "host", "host", 1.0, 1)])]);
        assert_eq!(original.index().len(), 1);
        let mut edited = original.clone();
        assert!(!edited.sample_index.is_built());
        assert_eq!(edited, original);
        edited.domains.clear();
        assert!(edited.index().is_empty());
        assert_eq!(original.index().len(), 1);
    }
}
