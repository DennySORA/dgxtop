//! Gap index: visible missing-data intervals with their reason.
//!
//! Adjacent gaps with the same key, adapter and reason are merged so a
//! sustained outage is one entry, not one per cycle. Lists are bounded; the
//! oldest entry is dropped when a list is full.

use std::collections::BTreeSet;
use std::sync::Arc;

use dgxtop_core::budget::{ByteLedger, Partition};
use dgxtop_core::canonical::GapRecord;
use dgxtop_core::identity::{AdapterId, MetricKey};
use dgxtop_core::text::sanitize_display;

use dgxtop_core::budget::accounted::{Account, AccountedDeque};

/// Longest reason kept (bytes); the result schema allows 256.
pub(crate) const MAX_REASON_BYTES: usize = 128;
/// Distinct reasons interned before falling back to [`OTHER_REASON`].
const MAX_REASONS: usize = 256;
const REASON_BYTES: u64 = 192;
const OTHER_REASON: &str = "other";

/// One gap of the index.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GapEntry {
    pub key: Option<MetricKey>,
    pub adapter_id: Option<AdapterId>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub reason: Arc<str>,
    pub affected_count: u32,
    /// Already copied into an archive journal; a settled entry is immutable.
    pub settled: bool,
}

impl GapEntry {
    pub(crate) fn overlaps(&self, start_ns: u64, end_ns: u64) -> bool {
        self.start_ns < end_ns && self.end_ns > start_ns
    }

    fn mergeable_with(&self, other: &GapEntry, tolerance_ns: u64) -> bool {
        !self.settled
            && self.key == other.key
            && self.adapter_id == other.adapter_id
            && self.reason == other.reason
            && other.start_ns <= self.end_ns.saturating_add(tolerance_ns)
            && other.end_ns.saturating_add(tolerance_ns) >= self.start_ns
    }

    pub(crate) fn to_record(&self) -> GapRecord {
        GapRecord {
            key: self.key.clone(),
            adapter_id: self.adapter_id.clone(),
            start_boot_ns: self.start_ns,
            end_boot_ns: self.end_ns,
            reason: self.reason.to_string(),
            affected_count: self.affected_count,
        }
    }
}

/// Outcome of inserting a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GapInsert {
    Merged,
    Added { dropped_oldest: bool },
    Rejected,
}

/// Bounded, start-ordered gap list.
pub(crate) struct GapList {
    entries: AccountedDeque<GapEntry>,
    capacity: usize,
}

impl GapList {
    pub(crate) fn new(ledger: &ByteLedger, capacity: usize) -> Self {
        Self {
            entries: AccountedDeque::new(ledger, Partition::Aggregates),
            capacity: capacity.max(1),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn insert(&mut self, entry: GapEntry, tolerance_ns: u64) -> GapInsert {
        if let Some(index) = (0..self.entries.len()).rev().find(|index| {
            self.entries
                .get(*index)
                .is_some_and(|existing| existing.mergeable_with(&entry, tolerance_ns))
        }) && let Some(existing) = self.entries.get_mut(index)
        {
            existing.start_ns = existing.start_ns.min(entry.start_ns);
            existing.end_ns = existing.end_ns.max(entry.end_ns);
            existing.affected_count = existing.affected_count.max(entry.affected_count);
            return GapInsert::Merged;
        }
        let dropped_oldest = self.entries.len() >= self.capacity;
        if dropped_oldest {
            self.entries.pop_front();
        }
        let position = self
            .entries
            .iter()
            .position(|existing| existing.start_ns > entry.start_ns)
            .unwrap_or(self.entries.len());
        match self.entries.try_insert(position, entry) {
            Ok(()) => GapInsert::Added { dropped_oldest },
            Err(_) => GapInsert::Rejected,
        }
    }

    /// Marks entries ending before `horizon_ns` as settled and returns them.
    pub(crate) fn settle_before(&mut self, horizon_ns: u64) -> Vec<GapEntry> {
        let mut settled = Vec::new();
        for index in 0..self.entries.len() {
            if let Some(entry) = self.entries.get_mut(index)
                && !entry.settled
                && entry.end_ns < horizon_ns
            {
                entry.settled = true;
                settled.push(entry.clone());
            }
        }
        settled
    }

    /// Drops entries that end at or before `floor_ns` (retention).
    pub(crate) fn drop_ending_by(&mut self, floor_ns: u64) -> usize {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.end_ns > floor_ns);
        self.entries.release_spare();
        before - self.entries.len()
    }

    /// Drops the oldest entry (capacity eviction).
    pub(crate) fn drop_oldest(&mut self) -> bool {
        let dropped = self.entries.pop_front().is_some();
        self.entries.release_spare();
        dropped
    }

    pub(crate) fn oldest_start(&self) -> Option<u64> {
        self.entries.front().map(|entry| entry.start_ns)
    }

    /// Copies of the entries overlapping `[start, end)` that apply to `key`.
    pub(crate) fn overlapping(&self, key: &MetricKey, start_ns: u64, end_ns: u64) -> Vec<GapEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.overlaps(start_ns, end_ns))
            .filter(|entry| entry.key.as_ref().is_none_or(|gap_key| gap_key == key))
            .cloned()
            .collect()
    }

    /// Bytes the next insertion may have to reserve.
    pub(crate) fn growth_cost(&self) -> u64 {
        self.entries.growth_cost()
    }
}

/// Bounded set of shared reason strings; reasons are short machine codes, so
/// interning keeps the index free of per-entry heap strings. Each new reason
/// is accounted when first seen; without memory the shared fallback is used.
pub(crate) struct ReasonInterner {
    reasons: BTreeSet<Arc<str>>,
    fallback: Arc<str>,
    account: Account,
}

impl ReasonInterner {
    pub(crate) fn new(ledger: &ByteLedger) -> Self {
        Self {
            reasons: BTreeSet::new(),
            fallback: Arc::from(OTHER_REASON),
            account: Account::new(ledger, Partition::Aggregates),
        }
    }

    pub(crate) fn intern(&mut self, reason: &str) -> Arc<str> {
        let cleaned = sanitize_display(reason, MAX_REASON_BYTES);
        let cleaned = if cleaned.is_empty() {
            "unspecified".to_owned()
        } else {
            cleaned
        };
        if let Some(existing) = self.reasons.get(cleaned.as_str()) {
            return Arc::clone(existing);
        }
        if self.reasons.len() >= MAX_REASONS || self.account.try_grow(REASON_BYTES).is_err() {
            return Arc::clone(&self.fallback);
        }
        let shared: Arc<str> = Arc::from(cleaned.as_str());
        self.reasons.insert(Arc::clone(&shared));
        shared
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::metric_key;

    fn gap(start: u64, end: u64, reason: &str, interner: &mut ReasonInterner) -> GapEntry {
        GapEntry {
            key: Some(metric_key("e", "gpu.temperature", "gpu")),
            adapter_id: None,
            start_ns: start,
            end_ns: end,
            reason: interner.intern(reason),
            affected_count: 1,
            settled: false,
        }
    }

    #[test]
    fn adjacent_same_reason_gaps_merge() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut interner = ReasonInterner::new(&ledger);
        let mut list = GapList::new(&ledger, 8);
        assert_eq!(
            list.insert(gap(0, 10, "timeout", &mut interner), 0),
            GapInsert::Added {
                dropped_oldest: false
            }
        );
        assert_eq!(
            list.insert(gap(10, 20, "timeout", &mut interner), 0),
            GapInsert::Merged
        );
        assert_eq!(
            list.insert(gap(25, 30, "timeout", &mut interner), 5),
            GapInsert::Merged
        );
        assert!(matches!(
            list.insert(gap(30, 40, "reset", &mut interner), 0),
            GapInsert::Added { .. }
        ));
        let key = metric_key("e", "gpu.temperature", "gpu");
        let all = list.overlapping(&key, 0, 100);
        assert_eq!(all.len(), 2);
        assert_eq!((all[0].start_ns, all[0].end_ns), (0, 30));
        assert_eq!(&*all[1].reason, "reset");
        let other_key = metric_key("e", "gpu.power.draw", "gpu");
        assert!(list.overlapping(&other_key, 0, 100).is_empty());
    }

    #[test]
    fn settled_gaps_are_not_extended_and_lists_are_bounded() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut interner = ReasonInterner::new(&ledger);
        let mut list = GapList::new(&ledger, 2);
        list.insert(gap(0, 10, "drop", &mut interner), 0);
        assert_eq!(list.settle_before(11).len(), 1);
        assert!(list.settle_before(11).is_empty());
        assert!(matches!(
            list.insert(gap(10, 20, "drop", &mut interner), 0),
            GapInsert::Added { .. }
        ));
        assert_eq!(
            list.insert(gap(50, 60, "drop2", &mut interner), 0),
            GapInsert::Added {
                dropped_oldest: true
            }
        );
        assert_eq!(list.oldest_start(), Some(10));
        assert_eq!(list.drop_ending_by(20), 1);
        assert!(list.drop_oldest());
        assert!(list.is_empty());
    }

    #[test]
    fn reasons_are_sanitized_bounded_and_interned() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut interner = ReasonInterner::new(&ledger);
        let first = interner.intern("time\u{1b}[31mout");
        assert_eq!(&*first, "timeout");
        assert!(Arc::ptr_eq(&first, &interner.intern("timeout")));
        assert_eq!(&*interner.intern(""), "unspecified");
        assert!(interner.intern(&"x".repeat(500)).len() <= MAX_REASON_BYTES);
        for index in 0..MAX_REASONS {
            interner.intern(&format!("r{index}"));
        }
        assert_eq!(&*interner.intern("brand-new"), OTHER_REASON);
    }
}
