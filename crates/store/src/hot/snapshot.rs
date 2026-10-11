//! The published snapshot slot, domain copy-on-write and recent gaps.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use dgxtop_core::canonical::{
    CanonicalSample, CanonicalValue, DomainSnapshot, Explanation, GapRecord, ImmutableSnapshot,
    LastGood, SnapshotEntry,
};
use dgxtop_core::identity::SourceId;
use dgxtop_core::ports::SnapshotReader;

use super::counters::StoreCounters;
use crate::candidates::{CandidateStore, RetainedCandidate};
use dgxtop_core::sampling::stale_deadline;

/// Gaps listed in `ImmutableSnapshot::recent_gaps`.
pub(crate) const RECENT_GAPS: usize = 64;

struct SharedState {
    latest: RwLock<Arc<ImmutableSnapshot>>,
    candidates: Arc<RwLock<CandidateStore>>,
    counters: Mutex<StoreCounters>,
}

/// Read side of the hot store (`SnapshotReader`): the latest immutable
/// snapshot behind a lock held only to clone one `Arc`, plus the retained
/// raw candidates and the store counters. Cheap to clone.
#[derive(Clone)]
pub struct SharedSnapshot {
    shared: Arc<SharedState>,
}

impl fmt::Debug for SharedSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedSnapshot")
            .field("revision", &self.latest().revision)
            .finish_non_exhaustive()
    }
}

impl SharedSnapshot {
    pub(crate) fn new(
        initial: Arc<ImmutableSnapshot>,
        candidates: Arc<RwLock<CandidateStore>>,
    ) -> Self {
        Self {
            shared: Arc::new(SharedState {
                latest: RwLock::new(initial),
                candidates,
                counters: Mutex::new(StoreCounters::default()),
            }),
        }
    }

    pub(crate) fn publish(&self, snapshot: Arc<ImmutableSnapshot>) {
        let previous = {
            let mut latest = self
                .shared
                .latest
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *latest, snapshot)
        };
        // The retired snapshot is dropped outside the lock.
        drop(previous);
    }

    pub(crate) fn publish_counters(&self, counters: StoreCounters) {
        *self
            .shared
            .counters
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = counters;
    }

    /// Store counters as of the last store operation.
    pub fn counters(&self) -> StoreCounters {
        *self
            .shared
            .counters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Newest retained raw readings of one source (oldest first).
    pub fn recent_candidates(&self, source_id: &SourceId, max: usize) -> Vec<RetainedCandidate> {
        self.shared
            .candidates
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .recent(source_id, max)
    }

    /// Sources with retained raw readings.
    pub fn candidate_sources(&self) -> Vec<SourceId> {
        self.shared
            .candidates
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .source_ids()
    }
}

impl SnapshotReader for SharedSnapshot {
    fn latest(&self) -> Arc<ImmutableSnapshot> {
        Arc::clone(
            &self
                .shared
                .latest
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

/// The value as last good annotation, if it carries a value.
fn refreshed_last_good(value: &CanonicalValue) -> Option<LastGood> {
    if !value.selection_state.has_value() {
        return None;
    }
    Some(LastGood {
        value: value.value?,
        unit: value.unit,
        sample_boot_ns: value.sample_time_boot_ns(),
        source_id: value.source_id.clone()?,
    })
}

/// Stale windows of the sources of one commit (STORAGE.md §3): a value is
/// current until `sample time + stale_factor x source period`.
pub(crate) struct StaleWindows<'a> {
    pub(crate) periods: &'a BTreeMap<SourceId, u64>,
    pub(crate) stale_factor: u32,
    /// Period of sources whose read group is unknown (the fast period, which
    /// can only shorten a window, never stretch it).
    pub(crate) default_period_ns: u64,
}

impl StaleWindows<'_> {
    fn deadline(&self, value: &CanonicalValue) -> Option<u64> {
        if !value.selection_state.has_value() || value.value.is_none() {
            return None;
        }
        let period = value
            .source_id
            .as_ref()
            .and_then(|source| self.periods.get(source))
            .copied()
            .unwrap_or(self.default_period_ns);
        Some(stale_deadline(
            value.sample_time_boot_ns(),
            self.stale_factor,
            period,
        ))
    }
}

/// Builds the next domain snapshot: the previous samples with the batch's
/// keys replaced. A missing value never refreshes `last_good`.
pub(crate) fn build_domain(
    previous: Option<&DomainSnapshot>,
    values: Vec<CanonicalValue>,
    explanations: Vec<Explanation>,
    windows: &StaleWindows<'_>,
    (revision, cycle_id, now_ns): (u64, u64, u64),
) -> DomainSnapshot {
    let mut samples = previous
        .map(|domain| domain.samples.clone())
        .unwrap_or_default();
    for value in values {
        let kept = samples
            .get(&value.key)
            .and_then(|entry| entry.last_good.clone());
        let last_good = refreshed_last_good(&value).or(kept);
        let stale_after_boot_ns = windows.deadline(&value);
        let sample = CanonicalSample::commit(value, revision, now_ns);
        samples.insert(
            sample.key.clone(),
            Arc::new(SnapshotEntry {
                sample,
                last_good,
                stale_after_boot_ns,
            }),
        );
    }
    let mut explanation_map = previous
        .map(|domain| domain.explanations.clone())
        .unwrap_or_default();
    for explanation in explanations {
        explanation_map.insert(explanation.key.clone(), Arc::new(explanation));
    }
    DomainSnapshot {
        revision,
        cycle_id,
        committed_boot_ns: now_ns,
        samples,
        explanations: explanation_map,
    }
}

/// Bounded list of the most recent gaps; adjacent repeats of the same gap
/// are merged so a sustained outage does not flood the list.
pub(crate) struct RecentGaps {
    gaps: VecDeque<GapRecord>,
    shared: Arc<Vec<GapRecord>>,
    dirty: bool,
    tolerance_ns: u64,
}

impl RecentGaps {
    pub(crate) fn new(tolerance_ns: u64) -> Self {
        Self {
            gaps: VecDeque::new(),
            shared: Arc::new(Vec::new()),
            dirty: false,
            tolerance_ns,
        }
    }

    pub(crate) fn note(&mut self, gap: GapRecord) {
        self.dirty = true;
        let tolerance = self.tolerance_ns;
        if let Some(existing) = self.gaps.iter_mut().rev().find(|existing| {
            existing.key == gap.key
                && existing.adapter_id == gap.adapter_id
                && existing.reason == gap.reason
                && gap.start_boot_ns <= existing.end_boot_ns.saturating_add(tolerance)
                && gap.end_boot_ns.saturating_add(tolerance) >= existing.start_boot_ns
        }) {
            existing.start_boot_ns = existing.start_boot_ns.min(gap.start_boot_ns);
            existing.end_boot_ns = existing.end_boot_ns.max(gap.end_boot_ns);
            existing.affected_count = existing.affected_count.max(gap.affected_count);
            return;
        }
        if self.gaps.len() >= RECENT_GAPS {
            self.gaps.pop_front();
        }
        self.gaps.push_back(gap);
    }

    /// Changed list to publish, if any.
    pub(crate) fn pending(&self) -> Option<Vec<GapRecord>> {
        self.dirty.then(|| self.gaps.iter().cloned().collect())
    }

    pub(crate) fn mark_published(&mut self, shared: Arc<Vec<GapRecord>>) {
        self.shared = shared;
        self.dirty = false;
    }

    pub(crate) fn shared(&self) -> Arc<Vec<GapRecord>> {
        Arc::clone(&self.shared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::metric::{CanonicalUnit, Scalar};
    use dgxtop_core::quality::{Freshness, Quality, SampleKind, SelectionState};
    use dgxtop_core::testing::{metric_key, source_id};

    fn value(selected: Option<f64>, read_ns: u64) -> CanonicalValue {
        CanonicalValue {
            key: metric_key("gpu:0", "gpu.temperature", "gpu"),
            cycle_id: 1,
            source_id: selected.map(|_| source_id("nvml:0:temp")),
            source_generation: selected.map(|_| 1),
            unit: CanonicalUnit::DegC,
            value: selected.map(|v| Scalar::from_f64(v).unwrap()),
            sample_kind: SampleKind::Measured,
            selection_state: if selected.is_some() {
                SelectionState::Selected
            } else {
                SelectionState::Unavailable
            },
            quality: Quality::Verified,
            freshness: Freshness::Fresh,
            measurement_freshness: Freshness::Unknown,
            observed_boot_ns: None,
            read_finished_boot_ns: read_ns,
            policy_revision: 1,
            candidate_source_ids: vec![],
            reason_codes: vec![],
            eligible_for_history: selected.is_some(),
        }
    }

    #[test]
    fn committed_values_carry_the_stale_deadline_of_their_source() {
        let key = metric_key("gpu:0", "gpu.temperature", "gpu");
        let periods = BTreeMap::from([(source_id("nvml:0:temp"), 2_000)]);
        let windows = StaleWindows {
            periods: &periods,
            stale_factor: 3,
            default_period_ns: 1_000,
        };
        let domain = build_domain(
            None,
            vec![value(Some(60.0), 10)],
            vec![],
            &windows,
            (1, 1, 11),
        );
        // sample time 10 + 3 x period 2000.
        assert_eq!(domain.samples[&key].stale_after_boot_ns, Some(6_010));
        let missing = build_domain(
            Some(&domain),
            vec![value(None, 20)],
            vec![],
            &windows,
            (2, 2, 21),
        );
        assert_eq!(missing.samples[&key].stale_after_boot_ns, None);
        let unknown_source = StaleWindows {
            periods: &BTreeMap::new(),
            stale_factor: 3,
            default_period_ns: 1_000,
        };
        let fallback = build_domain(
            None,
            vec![value(Some(60.0), 10)],
            vec![],
            &unknown_source,
            (1, 1, 11),
        );
        assert_eq!(fallback.samples[&key].stale_after_boot_ns, Some(3_010));
    }

    #[test]
    fn recent_gaps_merge_repeats_and_stay_bounded() {
        let mut recent = RecentGaps::new(5);
        let gap = |start: u64, reason: &str| GapRecord {
            key: None,
            adapter_id: None,
            start_boot_ns: start,
            end_boot_ns: start + 10,
            reason: reason.to_owned(),
            affected_count: 1,
        };
        recent.note(gap(0, "timeout"));
        recent.note(gap(12, "timeout"));
        assert_eq!(recent.pending().unwrap().len(), 1);
        for index in 0..100 {
            recent.note(gap(1000 * index, &format!("r{index}")));
        }
        let pending = recent.pending().unwrap();
        assert_eq!(pending.len(), RECENT_GAPS);
        recent.mark_published(Arc::new(pending));
        assert!(recent.pending().is_none());
        assert_eq!(recent.shared().len(), RECENT_GAPS);
    }
}
