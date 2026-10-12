//! The published snapshot slot, domain copy-on-write and recent gaps.

use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use dgxtop_core::canonical::{
    CanonicalSample, CanonicalValue, DomainSnapshot, Explanation, GapRecord, ImmutableSnapshot,
    LastGood, SnapshotEntry,
};
use dgxtop_core::identity::{MetricKey, SourceId};
use dgxtop_core::ports::SnapshotReader;

use super::counters::StoreCounters;
use dgxtop_core::sampling::stale_deadline;

/// Gaps listed in `ImmutableSnapshot::recent_gaps`.
pub(crate) const RECENT_GAPS: usize = 64;

struct SharedState {
    latest: RwLock<Arc<ImmutableSnapshot>>,
    counters: Mutex<StoreCounters>,
}

/// Read side of the live state (`SnapshotReader`): the latest immutable
/// snapshot behind a lock held only to clone one `Arc`, plus the store
/// counters. Cheap to clone.
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
    pub(crate) fn new(initial: Arc<ImmutableSnapshot>) -> Self {
        Self {
            shared: Arc::new(SharedState {
                latest: RwLock::new(initial),
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

/// Stale windows of the sources of one commit: a value is
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

type SharedEntries<V> = BTreeMap<MetricKey, Arc<V>>;

/// Builds the next domain snapshot: the previous samples with the batch's
/// keys replaced. A missing value never refreshes `last_good`.
///
/// The maps are built in one ordered pass over the previous entries and
/// the batch's, not cloned and then overwritten key by key; keys the batch
/// does not touch, and explanations equal to the previous ones, keep the
/// previous revision's shared entry (D-84).
pub(crate) fn build_domain(
    previous: Option<&DomainSnapshot>,
    values: Vec<CanonicalValue>,
    explanations: Vec<Explanation>,
    windows: &StaleWindows<'_>,
    (revision, cycle_id, now_ns): (u64, u64, u64),
) -> DomainSnapshot {
    let previous_samples = previous.map(|domain| &domain.samples);
    let previous_explanations = previous.map(|domain| &domain.explanations);
    let committed = values
        .into_iter()
        .map(|value| {
            let kept = previous_samples
                .and_then(|samples| samples.get(&value.key))
                .and_then(|entry| entry.last_good.clone());
            let last_good = refreshed_last_good(&value).or(kept);
            let stale_after_boot_ns = windows.deadline(&value);
            let sample = CanonicalSample::commit(value, revision, now_ns);
            let entry = SnapshotEntry {
                sample,
                last_good,
                stale_after_boot_ns,
            };
            (entry.sample.key.clone(), Arc::new(entry))
        })
        .collect();
    let explained = explanations
        .into_iter()
        .map(|explanation| {
            let shared = previous_explanations
                .and_then(|known| known.get(&explanation.key))
                .filter(|known| ***known == explanation)
                .map_or_else(|| Arc::new(explanation), Arc::clone);
            (shared.key.clone(), shared)
        })
        .collect();
    DomainSnapshot {
        revision,
        cycle_id,
        committed_boot_ns: now_ns,
        samples: merged(previous_samples, committed),
        explanations: merged(previous_explanations, explained),
    }
}

/// The previous entries with `updates` replacing theirs (the last update of
/// a key wins), in one ordered pass.
fn merged<V>(
    previous: Option<&SharedEntries<V>>,
    mut updates: Vec<(MetricKey, Arc<V>)>,
) -> SharedEntries<V> {
    // Stable, so of two updates of one key the later stays last.
    updates.sort_by(|left, right| left.0.cmp(&right.0));
    let mut updates = updates.into_iter().peekable();
    let mut kept = previous.into_iter().flatten().peekable();
    let mut entries: Vec<(MetricKey, Arc<V>)> = Vec::new();
    loop {
        let order = match (kept.peek(), updates.peek()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((kept_key, _)), Some((update_key, _))) => (*kept_key).cmp(update_key),
        };
        if order == Ordering::Less {
            if let Some((key, value)) = kept.next() {
                entries.push((key.clone(), Arc::clone(value)));
            }
            continue;
        }
        if order == Ordering::Equal {
            kept.next();
        }
        if let Some(update) = updates.next()
            && updates.peek().is_none_or(|next| next.0 != update.0)
        {
            entries.push(update);
        }
    }
    // Sorted and unique: the map is bulk-built without re-sorting work.
    entries.into_iter().collect()
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

    fn keyed_value(entity: &str, selected: f64) -> CanonicalValue {
        CanonicalValue {
            key: metric_key(entity, "gpu.temperature", "gpu"),
            ..value(Some(selected), 10)
        }
    }

    fn explanation(entity: &str, reason: &str) -> Explanation {
        Explanation {
            key: metric_key(entity, "gpu.temperature", "gpu"),
            policy_revision: 1,
            selection_state: SelectionState::Selected,
            selected_source: Some(source_id("nvml:0:temp")),
            pinned_source: None,
            candidates: vec![],
            conflict_spread: None,
            tolerance: None,
            switch: None,
            reason_codes: vec![reason.to_owned()],
        }
    }

    fn windows(periods: &BTreeMap<SourceId, u64>) -> StaleWindows<'_> {
        StaleWindows {
            periods,
            stale_factor: 3,
            default_period_ns: 1_000,
        }
    }

    #[test]
    fn untouched_keys_and_unchanged_explanations_keep_their_shared_entries() {
        let periods = BTreeMap::new();
        let first = build_domain(
            None,
            vec![keyed_value("gpu:0", 50.0), keyed_value("gpu:1", 51.0)],
            vec![explanation("gpu:0", "a"), explanation("gpu:1", "a")],
            &windows(&periods),
            (1, 1, 11),
        );
        let second = build_domain(
            Some(&first),
            vec![keyed_value("gpu:1", 52.0), keyed_value("gpu:2", 53.0)],
            vec![explanation("gpu:0", "a"), explanation("gpu:1", "b")],
            &windows(&periods),
            (2, 2, 21),
        );
        let key = |entity: &str| metric_key(entity, "gpu.temperature", "gpu");
        assert_eq!(second.samples.len(), 3);
        assert!(Arc::ptr_eq(
            &first.samples[&key("gpu:0")],
            &second.samples[&key("gpu:0")]
        ));
        assert_eq!(
            second.samples[&key("gpu:1")].sample.value,
            Scalar::from_f64(52.0).ok()
        );
        assert!(Arc::ptr_eq(
            &first.explanations[&key("gpu:0")],
            &second.explanations[&key("gpu:0")]
        ));
        assert!(!Arc::ptr_eq(
            &first.explanations[&key("gpu:1")],
            &second.explanations[&key("gpu:1")]
        ));
        assert_eq!(second.explanations[&key("gpu:1")].reason_codes, ["b"]);
    }

    #[test]
    fn the_last_update_of_a_key_wins_whatever_the_batch_order() {
        let periods = BTreeMap::new();
        let previous = build_domain(
            None,
            vec![keyed_value("gpu:1", 1.0)],
            vec![],
            &windows(&periods),
            (1, 1, 11),
        );
        let next = build_domain(
            Some(&previous),
            vec![
                keyed_value("gpu:2", 2.0),
                keyed_value("gpu:0", 3.0),
                keyed_value("gpu:2", 4.0),
            ],
            vec![],
            &windows(&periods),
            (2, 2, 21),
        );
        let values: Vec<(String, Option<Scalar>)> = next
            .samples
            .values()
            .map(|entry| (entry.sample.key.entity_id.to_string(), entry.sample.value))
            .collect();
        assert_eq!(
            values,
            vec![
                ("gpu:0".to_owned(), Scalar::from_f64(3.0).ok()),
                ("gpu:1".to_owned(), Scalar::from_f64(1.0).ok()),
                ("gpu:2".to_owned(), Scalar::from_f64(4.0).ok()),
            ]
        );
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
