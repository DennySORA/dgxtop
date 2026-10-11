//! Series segments.
//!
//! A segment is a run of history with one source incarnation, one policy
//! revision, one sample kind and one aggregation method. Any change cuts a new
//! segment; queries report the cut as a source change and records refer to
//! their segment by a per-series ordinal, so no per-sample string is stored.

use std::sync::Arc;

use dgxtop_core::budget::{ByteLedger, CapacityError, Partition};
use dgxtop_core::history::HistoryMethod;
use dgxtop_core::identity::{MetricKey, SourceId};
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::quality::SampleKind;
use uuid::Uuid;

use dgxtop_core::budget::accounted::Account;

/// Upper bound of segments retained per series.
pub(crate) const MAX_SEGMENTS_PER_SERIES: usize = 4096;
/// Accounted bytes of one segment (metadata, uuid, shared key, view slot).
const SEGMENT_BYTES: u64 = 320;

/// Everything whose change cuts a segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegmentIdentity {
    pub source_id: SourceId,
    pub source_generation: u32,
    pub policy_revision: u32,
    pub sample_kind: SampleKind,
    pub method: HistoryMethod,
}

/// Immutable description of one segment, shared with readers and the archive.
#[derive(Debug)]
pub(crate) struct SegmentMeta {
    pub uuid: Uuid,
    pub key: MetricKey,
    pub identity: SegmentIdentity,
    pub unit: CanonicalUnit,
    pub started_ns: u64,
    /// First segment of its method in this series: its start is not a change.
    pub opens_series: bool,
}

/// Why no segment could be assigned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SegmentError {
    Limit,
    Capacity(CapacityError),
}

/// Segments of one series, oldest first, addressed by ordinal.
pub(crate) struct SegmentTable {
    first_ordinal: u32,
    segments: Arc<Vec<Arc<SegmentMeta>>>,
    current_gauge: Option<u32>,
    current_counter: Option<u32>,
    account: Account,
}

impl SegmentTable {
    pub(crate) fn new(ledger: &ByteLedger) -> Self {
        Self {
            first_ordinal: 0,
            segments: Arc::new(Vec::new()),
            current_gauge: None,
            current_counter: None,
            account: Account::new(ledger, Partition::Aggregates),
        }
    }

    /// Ordinal of the segment matching `identity`, creating one on change.
    pub(crate) fn resolve(
        &mut self,
        key: &MetricKey,
        identity: SegmentIdentity,
        unit: CanonicalUnit,
        started_ns: u64,
    ) -> Result<u32, SegmentError> {
        let is_counter = identity.method == HistoryMethod::CounterDelta;
        let current = if is_counter {
            self.current_counter
        } else {
            self.current_gauge
        };
        if !self.would_create(&identity, unit)
            && let Some(ordinal) = current
        {
            return Ok(ordinal);
        }
        if self.segments.len() >= MAX_SEGMENTS_PER_SERIES {
            return Err(SegmentError::Limit);
        }
        let ordinal = self
            .first_ordinal
            .checked_add(u32::try_from(self.segments.len()).map_err(|_| SegmentError::Limit)?)
            .ok_or(SegmentError::Limit)?;
        self.account
            .try_grow(SEGMENT_BYTES)
            .map_err(SegmentError::Capacity)?;
        let segment = Arc::new(SegmentMeta {
            uuid: Uuid::new_v4(),
            key: key.clone(),
            identity,
            unit,
            started_ns,
            opens_series: current.is_none(),
        });
        Arc::make_mut(&mut self.segments).push(segment);
        if is_counter {
            self.current_counter = Some(ordinal);
        } else {
            self.current_gauge = Some(ordinal);
        }
        Ok(ordinal)
    }

    pub(crate) fn get(&self, ordinal: u32) -> Option<&Arc<SegmentMeta>> {
        let index = ordinal.checked_sub(self.first_ordinal)?;
        self.segments.get(usize::try_from(index).ok()?)
    }

    /// Whether [`SegmentTable::resolve`] would create a segment.
    pub(crate) fn would_create(&self, identity: &SegmentIdentity, unit: CanonicalUnit) -> bool {
        let current = if identity.method == HistoryMethod::CounterDelta {
            self.current_counter
        } else {
            self.current_gauge
        };
        !current
            .and_then(|ordinal| self.get(ordinal))
            .is_some_and(|segment| segment.identity == *identity && segment.unit == unit)
    }

    /// Ordinal of the oldest segment still referenced by current state.
    pub(crate) fn current_floor(&self) -> Option<u32> {
        match (self.current_gauge, self.current_counter) {
            (Some(gauge), Some(counter)) => Some(gauge.min(counter)),
            (gauge, counter) => gauge.or(counter),
        }
    }

    /// Retires segments older than `ordinal` (no record references them).
    pub(crate) fn retire_before(&mut self, ordinal: u32) {
        let retire = usize::try_from(ordinal.saturating_sub(self.first_ordinal))
            .unwrap_or(usize::MAX)
            .min(self.segments.len());
        if retire == 0 {
            return;
        }
        Arc::make_mut(&mut self.segments).drain(..retire);
        self.first_ordinal = ordinal.min(
            self.first_ordinal
                .saturating_add(u32::try_from(retire).unwrap_or(u32::MAX)),
        );
        let remaining = u64::try_from(self.segments.len()).unwrap_or(u64::MAX);
        self.account
            .shrink_to(remaining.saturating_mul(SEGMENT_BYTES));
    }

    /// Shared snapshot for readers (copy-on-write).
    pub(crate) fn view(&self) -> SegmentsView {
        SegmentsView {
            first_ordinal: self.first_ordinal,
            segments: Arc::clone(&self.segments),
        }
    }

    pub(crate) fn has_counter_segment(&self) -> bool {
        self.current_counter.is_some()
    }
}

/// Reader-side lookup of segments by ordinal.
#[derive(Clone)]
pub(crate) struct SegmentsView {
    first_ordinal: u32,
    segments: Arc<Vec<Arc<SegmentMeta>>>,
}

impl SegmentsView {
    pub(crate) fn get(&self, ordinal: u32) -> Option<&Arc<SegmentMeta>> {
        let index = ordinal.checked_sub(self.first_ordinal)?;
        self.segments.get(usize::try_from(index).ok()?)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arc<SegmentMeta>> {
        self.segments.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::{metric_key, source_id};

    fn identity(source: &str, generation: u32, method: HistoryMethod) -> SegmentIdentity {
        SegmentIdentity {
            source_id: source_id(source),
            source_generation: generation,
            policy_revision: 1,
            sample_kind: SampleKind::Measured,
            method,
        }
    }

    #[test]
    fn identity_change_cuts_a_segment_and_methods_are_tracked_apart() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let key = metric_key("gpu:0", "gpu.temperature", "gpu");
        let mut table = SegmentTable::new(&ledger);
        let gauge = HistoryMethod::TimeWeightedStep;
        let first = table
            .resolve(&key, identity("a", 1, gauge), CanonicalUnit::DegC, 10)
            .unwrap();
        let same = table
            .resolve(&key, identity("a", 1, gauge), CanonicalUnit::DegC, 20)
            .unwrap();
        assert_eq!(first, same);
        let counter = table
            .resolve(
                &key,
                identity("a", 1, HistoryMethod::CounterDelta),
                CanonicalUnit::Joules,
                20,
            )
            .unwrap();
        assert_ne!(counter, first);
        let regenerated = table
            .resolve(&key, identity("a", 2, gauge), CanonicalUnit::DegC, 30)
            .unwrap();
        assert_eq!(regenerated, 2);
        assert!(table.get(first).unwrap().opens_series);
        assert!(table.get(counter).unwrap().opens_series);
        assert!(!table.get(regenerated).unwrap().opens_series);
        assert_eq!(table.get(regenerated).unwrap().started_ns, 30);
        assert_eq!(ledger.used(Partition::Aggregates), 3 * SEGMENT_BYTES);
    }

    #[test]
    fn retired_ordinals_never_resolve_again() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let key = metric_key("gpu:0", "gpu.temperature", "gpu");
        let mut table = SegmentTable::new(&ledger);
        let gauge = HistoryMethod::TimeWeightedStep;
        for generation in 0..4 {
            table
                .resolve(
                    &key,
                    identity("a", generation, gauge),
                    CanonicalUnit::DegC,
                    0,
                )
                .unwrap();
        }
        let view_before = table.view();
        table.retire_before(3);
        assert!(table.get(2).is_none());
        assert_eq!(table.get(3).unwrap().identity.source_generation, 3);
        // A reader view taken earlier keeps its own copy.
        assert!(view_before.get(0).is_some());
        assert_eq!(ledger.used(Partition::Aggregates), SEGMENT_BYTES);
        let next = table
            .resolve(&key, identity("b", 0, gauge), CanonicalUnit::DegC, 5)
            .unwrap();
        assert_eq!(next, 4);
    }
}
