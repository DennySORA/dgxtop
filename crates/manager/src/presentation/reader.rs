//! One read-only facade over the snapshot, catalog and the state's trends,
//! shared by every module builder of a projection.

use std::collections::BTreeSet;

use dgxtop_core::canonical::{CatalogSnapshot, Explanation, ImmutableSnapshot, SnapshotEntry};
use dgxtop_core::identity::{EntityId, MetricKey, ProcessKey};
use dgxtop_core::metric::ids;
use dgxtop_core::naming::entity;
use dgxtop_core::ports::TrendReader;
use dgxtop_core::view::{MetricCell, Trend};

use super::cell::{build_metric_cell, missing_cell};
use super::index::{KeyResolver, SnapshotLookup};

pub(crate) struct MetricReader<'a> {
    index: SnapshotLookup<'a>,
    keys: KeyResolver,
    snapshot: &'a ImmutableSnapshot,
    catalog: &'a CatalogSnapshot,
    /// `None` where no trend is drawn (the history's chart picks).
    trends: Option<&'a dyn TrendReader>,
    now_boot_ns: u64,
}

impl<'a> MetricReader<'a> {
    pub(crate) fn new(
        snapshot: &'a ImmutableSnapshot,
        catalog: &'a CatalogSnapshot,
        now_boot_ns: u64,
    ) -> Self {
        let index = SnapshotLookup::new(snapshot);
        let keys = KeyResolver::new(catalog, &index);
        Self {
            index,
            keys,
            snapshot,
            catalog,
            trends: None,
            now_boot_ns,
        }
    }

    /// Reads trends from the state's `trends`.
    pub(crate) fn with_trends(self, trends: &'a dyn TrendReader) -> Self {
        Self {
            trends: Some(trends),
            ..self
        }
    }

    pub(crate) fn index(&self) -> &SnapshotLookup<'a> {
        &self.index
    }

    pub(crate) fn catalog(&self) -> &'a CatalogSnapshot {
        self.catalog
    }

    pub(crate) fn explanation(&self, key: &MetricKey) -> Option<&'a Explanation> {
        self.snapshot
            .explanation(key)
            .map(|explanation| explanation.as_ref())
    }

    pub(crate) fn cell_of(&self, entry: &SnapshotEntry) -> MetricCell {
        build_metric_cell(
            entry,
            self.explanation(&entry.sample.key),
            self.catalog,
            self.now_boot_ns,
        )
    }

    pub(crate) fn cell(&self, metric: &str, entity: &str, scope: &str) -> MetricCell {
        self.cell_or_missing(self.index.get(metric, entity, scope), metric)
    }

    pub(crate) fn cell_preferring(
        &self,
        metric: &str,
        entity: &str,
        preferred_scope: &str,
    ) -> MetricCell {
        self.cell_or_missing(self.index.find(metric, entity, preferred_scope), metric)
    }

    pub(crate) fn cell_or_missing(
        &self,
        entry: Option<&SnapshotEntry>,
        metric: &str,
    ) -> MetricCell {
        entry.map_or_else(|| missing_cell(metric), |entry| self.cell_of(entry))
    }

    /// Most recent `width` trend points of a series (empty when unknown).
    pub(crate) fn trend_preferring(
        &self,
        metric: &str,
        entity: &str,
        preferred_scope: &str,
        width: usize,
    ) -> Trend {
        let key = self.series_key(metric, entity, preferred_scope);
        self.trend_of(key, width)
    }

    /// What the processes of `gpus` held beside each of the latest
    /// `width` points of the host's used memory (D-64).
    pub(crate) fn gpu_memory_trend(&self, gpus: &BTreeSet<EntityId>, width: usize) -> Trend {
        self.beside_used_memory(width, |trends, used| {
            trends.gpu_memory_beside(used, gpus, width)
        })
    }

    /// What `process` held on `gpus` beside the same points (D-64).
    pub(crate) fn process_memory_trend(
        &self,
        process: &ProcessKey,
        gpus: &BTreeSet<EntityId>,
        width: usize,
    ) -> Trend {
        self.beside_used_memory(width, |trends, used| {
            trends.process_memory_beside(used, process, gpus, width)
        })
    }

    fn beside_used_memory(
        &self,
        width: usize,
        read: impl Fn(&dyn TrendReader, &MetricKey) -> Trend,
    ) -> Trend {
        let used = self.series_key(ids::MEMORY_USED, entity::MEMORY, entity::SCOPE_SYSTEM);
        match (self.trends, used) {
            (Some(trends), Some(used)) if width > 0 => read(trends, &used),
            _ => Trend::default(),
        }
    }

    /// The committed key of the series, else the catalog's.
    fn series_key(&self, metric: &str, entity: &str, preferred_scope: &str) -> Option<MetricKey> {
        self.index
            .find(metric, entity, preferred_scope)
            .map(|entry| entry.sample.key.clone())
            .or_else(|| self.keys.key(metric, entity, preferred_scope))
    }

    fn trend_of(&self, key: Option<MetricKey>, width: usize) -> Trend {
        match (self.trends, key) {
            (Some(trends), Some(key)) if width > 0 => trends.trend(&key, width),
            _ => Trend::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{Fixture, NOW, sample};

    use dgxtop_core::view::CellState;

    #[test]
    fn cells_and_values_follow_selection_state() {
        let mut fixture = Fixture::new();
        fixture.add("memory", sample("memory.used", "memory", "system").u64(42));
        fixture.add(
            "memory",
            sample("memory.cached", "memory", "system").unavailable(),
        );
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW);
        assert_eq!(
            reader.cell("memory.used", "memory", "system").value_f64(),
            Some(42.0)
        );
        let missing = reader.cell("memory.total", "memory", "system");
        assert_eq!(missing.state, CellState::Unavailable);
        assert_eq!(missing.value, None);
    }
}
