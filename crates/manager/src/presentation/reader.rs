//! One read-only facade over the snapshot, catalog and trend book, shared by
//! every module builder of a projection.

use dgxtop_core::canonical::{CatalogSnapshot, Explanation, ImmutableSnapshot, SnapshotEntry};
use dgxtop_core::identity::MetricKey;
use dgxtop_core::view::{MetricCell, Trend};

use super::TrendBook;
use super::cell::{build_metric_cell, missing_cell};
use super::index::{KeyResolver, SampleIndex};

pub(crate) struct MetricReader<'a> {
    index: SampleIndex<'a>,
    keys: KeyResolver,
    snapshot: &'a ImmutableSnapshot,
    catalog: &'a CatalogSnapshot,
    trends: &'a TrendBook,
    now_boot_ns: u64,
}

impl<'a> MetricReader<'a> {
    pub(crate) fn new(
        snapshot: &'a ImmutableSnapshot,
        catalog: &'a CatalogSnapshot,
        trends: &'a TrendBook,
        now_boot_ns: u64,
    ) -> Self {
        let index = SampleIndex::new(snapshot);
        let keys = KeyResolver::new(catalog, &index);
        Self {
            index,
            keys,
            snapshot,
            catalog,
            trends,
            now_boot_ns,
        }
    }

    pub(crate) fn index(&self) -> &SampleIndex<'a> {
        &self.index
    }

    pub(crate) fn snapshot(&self) -> &'a ImmutableSnapshot {
        self.snapshot
    }

    pub(crate) fn catalog(&self) -> &'a CatalogSnapshot {
        self.catalog
    }

    pub(crate) fn now_boot_ns(&self) -> u64 {
        self.now_boot_ns
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

    /// The trend of a series recorded beside `(metric, entity, preferred
    /// scope)`, whose key `beside` derives from that series' key.
    pub(crate) fn trend_beside(
        &self,
        (metric, entity, preferred_scope): (&str, &str, &str),
        beside: impl Fn(&MetricKey) -> Option<MetricKey>,
        width: usize,
    ) -> Trend {
        let key = self
            .series_key(metric, entity, preferred_scope)
            .and_then(|key| beside(&key));
        self.trend_of(key, width)
    }

    /// The committed key of the series, else the catalog's.
    fn series_key(&self, metric: &str, entity: &str, preferred_scope: &str) -> Option<MetricKey> {
        self.index
            .find(metric, entity, preferred_scope)
            .map(|entry| entry.sample.key.clone())
            .or_else(|| self.keys.key(metric, entity, preferred_scope))
    }

    fn trend_of(&self, key: Option<MetricKey>, width: usize) -> Trend {
        match key {
            Some(key) if width > 0 => self.trends.trend(&key, width),
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
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, &fixture.trends, NOW);
        assert_eq!(
            reader.cell("memory.used", "memory", "system").value_f64(),
            Some(42.0)
        );
        let missing = reader.cell("memory.total", "memory", "system");
        assert_eq!(missing.state, CellState::Unavailable);
        assert_eq!(missing.value, None);
    }
}
