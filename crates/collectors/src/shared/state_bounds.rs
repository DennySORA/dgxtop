//! Bounds of the collector state carried between jobs. Each map is capped by
//! the plan limits; the oldest entries are evicted first, so persistent
//! source or process churn can never grow the state without limit.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::canonical::{CollectionLimits, CollectorState};
use dgxtop_core::identity::MetricKey;

/// Process bases kept per allowed process row (exited processes age out).
pub(crate) const PROCESS_BASES_PER_ROW: usize = 4;

/// Applies every bound. `touched` are the keys arbitrated in this job; their
/// hysteresis memos are evicted last.
pub(crate) fn enforce(
    state: &mut CollectorState,
    limits: &CollectionLimits,
    touched: &BTreeSet<MetricKey>,
) {
    let max_sources = as_len(limits.max_sources);
    let max_series = as_len(limits.max_series);
    let max_bases = as_len(limits.max_processes).saturating_mul(PROCESS_BASES_PER_ROW);
    evict_oldest(&mut state.candidates, max_sources, |observation| {
        observation.read_finished_boot_ns
    });
    evict_oldest(&mut state.counters, max_sources, |base| base.sample_boot_ns);
    evict_oldest(&mut state.last_good, max_series, |last_good| {
        last_good.sample_boot_ns
    });
    evict_oldest(&mut state.processes, max_bases, |base| base.sample_boot_ns);
    evict_selections(state, max_series, touched);
}

fn as_len(limit: u32) -> usize {
    usize::try_from(limit).unwrap_or(usize::MAX)
}

/// Removes the entries with the smallest `age_key` until `cap` remain; ties
/// are broken by map order so eviction is deterministic.
pub(crate) fn evict_oldest<K: Ord + Clone, V>(
    map: &mut BTreeMap<K, V>,
    cap: usize,
    age_key: impl Fn(&V) -> u64,
) {
    let excess = map.len().saturating_sub(cap);
    if excess == 0 {
        return;
    }
    let mut ages: Vec<(u64, K)> = map
        .iter()
        .map(|(key, value)| (age_key(value), key.clone()))
        .collect();
    ages.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    for (_, key) in ages.into_iter().take(excess) {
        map.remove(&key);
    }
}

fn evict_selections(state: &mut CollectorState, cap: usize, touched: &BTreeSet<MetricKey>) {
    let excess = state.selections.len().saturating_sub(cap);
    if excess == 0 {
        return;
    }
    let mut order: Vec<(bool, u64, MetricKey)> = state
        .selections
        .iter()
        .map(|(key, memo)| {
            (
                touched.contains(key),
                memo.selected_since_boot_ns,
                key.clone(),
            )
        })
        .collect();
    order.sort();
    for (_, _, key) in order.into_iter().take(excess) {
        state.selections.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::canonical::{CounterBase, SelectionMemo};
    use dgxtop_core::testing::{boot_id, metric_key, source_id};

    fn limits(max_sources: u32, max_series: u32) -> CollectionLimits {
        CollectionLimits {
            max_series,
            max_sources,
            max_candidates_per_series: 4,
            max_processes: 2,
            max_process_gpu_links: 4,
        }
    }

    #[test]
    fn oldest_counters_are_evicted_first() {
        let mut state = CollectorState::default();
        for index in 0..10u64 {
            state.counters.insert(
                source_id(&format!("s{index}")),
                CounterBase {
                    value: index,
                    boot_id: boot_id("b"),
                    generation: 1,
                    sample_boot_ns: index,
                },
            );
        }
        enforce(&mut state, &limits(3, 8), &BTreeSet::new());
        let kept: Vec<u64> = state.counters.values().map(|base| base.value).collect();
        assert_eq!(kept, vec![7, 8, 9]);
    }

    #[test]
    fn touched_selections_survive_eviction() {
        let mut state = CollectorState::default();
        let touched_key = metric_key("e", "cpu.temperature.package", "package:0");
        for index in 0..5u64 {
            let key = if index == 0 {
                touched_key.clone()
            } else {
                metric_key(&format!("e{index}"), "cpu.temperature.package", "package:0")
            };
            state.selections.insert(
                key,
                SelectionMemo {
                    selected: Some(source_id("s")),
                    selected_since_boot_ns: index,
                    challenger: None,
                    challenger_streak: 0,
                    challenger_since_boot_ns: 0,
                },
            );
        }
        let touched = BTreeSet::from([touched_key.clone()]);
        enforce(&mut state, &limits(8, 2), &touched);
        assert_eq!(state.selections.len(), 2);
        assert!(state.selections.contains_key(&touched_key));
    }
}
