//! Normal-data coverage: the share of *scheduled*
//! sampling slots whose read reached the store in a committed batch.
//!
//! The denominator is fixed in advance from the configured periods and the
//! groups each replay host serves, so a skipped, failed, throttled or
//! dropped read always lowers coverage — it can never shrink the
//! denominator. Samples the fixture marks missing/failed on purpose live
//! inside successful reads and therefore do not count against coverage
//! (the "pre-defined capability" exclusion).

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::clock::{NANOS_PER_SECOND, ms_to_ns};
use dgxtop_core::config::SamplingConfig;
use dgxtop_core::naming::{self, PeriodClass};
use dgxtop_core::observation::split_page;
use serde::Serialize;

/// How long after a window ends its coverage waits for the commits of reads
/// that finished inside it. A read commits only after every page of its
/// cycle arrived and the collection was stored, so a report taken at the end
/// would miss a read that finished just before it; a read committing later
/// than this counts as missing.
pub const COMMIT_GRACE_NS: u64 = NANOS_PER_SECOND;

/// One read group a host is expected to deliver every `period_ns`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScheduledGroup {
    pub adapter_id: String,
    pub read_group: String,
    pub period_ns: u64,
}

/// Configured period of a replay read group spelled `<live adapter>.<group>`
/// (the period class of the live group it mirrors; unknown groups are fast).
pub fn replay_group_period_ms(read_group: &str, sampling: &SamplingConfig) -> u32 {
    let (base, _page) = split_page(read_group);
    let class = base.split_once('.').and_then(|(adapter, group)| {
        naming::READ_GROUPS
            .iter()
            .find(|spec| spec.adapter == adapter && spec.group == group)
            .map(|spec| spec.period)
    });
    match class {
        Some(PeriodClass::Capacity) => sampling.capacity_interval_ms,
        Some(PeriodClass::SlowHealth) => sampling.slow_health_interval_ms,
        Some(PeriodClass::Fast) | None => sampling.interval_ms,
    }
}

/// Scheduled groups of replay hosts: `groups_per_host[i]` belongs to
/// `adapter_ids[i]`.
pub fn scheduled_replay_groups(
    adapter_ids: &[String],
    groups_per_host: &[BTreeSet<String>],
    sampling: &SamplingConfig,
) -> Vec<ScheduledGroup> {
    adapter_ids
        .iter()
        .zip(groups_per_host)
        .flat_map(|(adapter_id, groups)| {
            groups.iter().map(move |group| ScheduledGroup {
                adapter_id: adapter_id.clone(),
                read_group: group.clone(),
                period_ns: ms_to_ns(u64::from(replay_group_period_ms(group, sampling))),
            })
        })
        .collect()
}

/// Coverage of one scheduled group.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupCoverage {
    pub adapter_id: String,
    pub read_group: String,
    pub period_ms: u64,
    pub expected_slots: u64,
    pub covered_slots: u64,
}

/// Coverage over a measurement window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CoverageReport {
    pub window_ns: u64,
    pub expected_slots: u64,
    pub covered_slots: u64,
    /// `None` when nothing was scheduled (coverage cannot be claimed).
    pub percent: Option<f64>,
    /// Committed reads of groups that were not scheduled (diagnostic).
    pub unscheduled_reads: u64,
    pub groups: Vec<GroupCoverage>,
}

/// Committed reads seen so far, one entry per `(adapter, group, cycle)`.
#[derive(Debug, Default)]
pub struct CoverageLedger {
    reads: BTreeMap<(String, String), BTreeMap<u64, u64>>,
}

impl CoverageLedger {
    /// A read (any page) of `read_group` for `cycle_id` was committed; pages
    /// of one cycle count once, at their earliest finish time.
    pub fn record(&mut self, adapter_id: &str, read_group: &str, cycle_id: u64, finished_ns: u64) {
        let (base, _page) = split_page(read_group);
        let cycles = self
            .reads
            .entry((adapter_id.to_owned(), base.to_owned()))
            .or_default();
        let finished = cycles.entry(cycle_id).or_insert(finished_ns);
        *finished = (*finished).min(finished_ns);
    }

    /// Forgets reads that finished before `start_ns` (bounded memory).
    pub fn forget_before(&mut self, start_ns: u64) {
        for cycles in self.reads.values_mut() {
            cycles.retain(|_, finished| *finished >= start_ns);
        }
        self.reads.retain(|_, cycles| !cycles.is_empty());
    }

    /// Coverage of `groups` over `[start_ns, end_ns)` once it is final:
    /// `None` until `now_ns` is [`COMMIT_GRACE_NS`] past the end.
    pub fn final_report(
        &self,
        groups: &[ScheduledGroup],
        start_ns: u64,
        end_ns: u64,
        now_ns: u64,
    ) -> Option<CoverageReport> {
        (now_ns >= end_ns.saturating_add(COMMIT_GRACE_NS))
            .then(|| self.report(groups, start_ns, end_ns))
    }

    /// Coverage of `groups` over `[start_ns, end_ns)` from the reads
    /// committed so far (reads near the end may still be committing; see
    /// [`Self::final_report`]).
    #[allow(clippy::cast_precision_loss)]
    pub fn report(&self, groups: &[ScheduledGroup], start_ns: u64, end_ns: u64) -> CoverageReport {
        let window_ns = end_ns.saturating_sub(start_ns);
        let in_window = |cycles: &BTreeMap<u64, u64>| -> u64 {
            let count = cycles
                .values()
                .filter(|finished| (start_ns..end_ns).contains(*finished))
                .count();
            u64::try_from(count).unwrap_or(u64::MAX)
        };
        let per_group: Vec<GroupCoverage> = groups
            .iter()
            .map(|group| {
                let expected = window_ns.checked_div(group.period_ns).unwrap_or(0);
                let delivered = self
                    .reads
                    .get(&(group.adapter_id.clone(), group.read_group.clone()))
                    .map_or(0, in_window);
                GroupCoverage {
                    adapter_id: group.adapter_id.clone(),
                    read_group: group.read_group.clone(),
                    period_ms: group.period_ns / 1_000_000,
                    expected_slots: expected,
                    covered_slots: delivered.min(expected),
                }
            })
            .collect();
        let scheduled: BTreeSet<(&str, &str)> = groups
            .iter()
            .map(|group| (group.adapter_id.as_str(), group.read_group.as_str()))
            .collect();
        let unscheduled_reads = self
            .reads
            .iter()
            .filter(|((adapter, group), _)| {
                !scheduled.contains(&(adapter.as_str(), group.as_str()))
            })
            .map(|(_, cycles)| in_window(cycles))
            .sum();
        let expected_slots: u64 = per_group.iter().map(|group| group.expected_slots).sum();
        let covered_slots: u64 = per_group.iter().map(|group| group.covered_slots).sum();
        CoverageReport {
            window_ns,
            expected_slots,
            covered_slots,
            percent: (expected_slots > 0)
                .then(|| 100.0 * covered_slots as f64 / expected_slots as f64),
            unscheduled_reads,
            groups: per_group,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::config::Config;

    const SECOND: u64 = 1_000_000_000;

    fn sampling() -> SamplingConfig {
        Config::defaults().unwrap().sampling
    }

    fn two_groups() -> Vec<ScheduledGroup> {
        let groups = vec![
            BTreeSet::from(["procfs.cpu".to_owned()]),
            BTreeSet::from(["nvml.slow_health".to_owned()]),
        ];
        scheduled_replay_groups(
            &["replay-1".to_owned(), "replay-2".to_owned()],
            &groups,
            &sampling(),
        )
    }

    #[test]
    fn every_scheduled_slot_delivered_is_full_coverage() {
        let mut ledger = CoverageLedger::default();
        for cycle in 0..10 {
            let at = cycle * SECOND + SECOND / 2;
            ledger.record("replay-1", "procfs.cpu", cycle, at);
            ledger.record("replay-1", "procfs.cpu:1", cycle, at + 1);
            if cycle % 5 == 0 {
                ledger.record("replay-2", "nvml.slow_health", cycle, at);
            }
        }
        let report = ledger.report(&two_groups(), 0, 10 * SECOND);
        assert_eq!(report.expected_slots, 10 + 2);
        assert_eq!(report.covered_slots, 12);
        assert_eq!(report.percent, Some(100.0));
        assert_eq!(report.unscheduled_reads, 0);
    }

    #[test]
    fn missing_reads_lower_coverage_and_extra_reads_never_raise_it() {
        let mut ledger = CoverageLedger::default();
        for cycle in 0..9 {
            ledger.record("replay-1", "procfs.cpu", cycle, cycle * SECOND + 1);
        }
        ledger.record("replay-1", "procfs.cpu", 99, 9 * SECOND + 2);
        ledger.record("replay-1", "procfs.cpu", 100, 9 * SECOND + 3);
        ledger.record("replay-9", "hwmon.fast", 1, SECOND);
        let report = ledger.report(&two_groups(), 0, 10 * SECOND);
        // 10 cpu reads (capped at 10 expected) + 0 of 2 slow_health slots.
        assert_eq!(report.covered_slots, 10);
        assert_eq!(report.expected_slots, 12);
        let percent = report.percent.unwrap();
        assert!((percent - 1000.0 / 12.0).abs() < 1e-9, "{percent}");
        assert_eq!(report.unscheduled_reads, 1);
    }

    #[test]
    fn a_window_report_is_final_only_once_its_last_reads_could_commit() {
        let mut ledger = CoverageLedger::default();
        let end = 10 * SECOND;
        for cycle in 0..9 {
            ledger.record("replay-1", "procfs.cpu", cycle, cycle * SECOND + SECOND / 2);
        }
        ledger.record("replay-2", "nvml.slow_health", 0, SECOND / 2);
        ledger.record("replay-2", "nvml.slow_health", 5, 5 * SECOND + SECOND / 2);
        // The tenth cpu read finished 10 ms before the end; its pages are
        // still being collected and stored when the window closes.
        assert!(ledger.final_report(&two_groups(), 0, end, end).is_none());
        let just_short = end + COMMIT_GRACE_NS - 1;
        assert!(
            ledger
                .final_report(&two_groups(), 0, end, just_short)
                .is_none()
        );
        ledger.record("replay-1", "procfs.cpu", 9, end - SECOND / 100);
        // A read that finished after the end belongs to the next window.
        ledger.record("replay-1", "procfs.cpu", 10, end + 1);
        let report = ledger
            .final_report(&two_groups(), 0, end, end + COMMIT_GRACE_NS)
            .unwrap();
        assert_eq!((report.covered_slots, report.expected_slots), (12, 12));
        assert_eq!(report.percent, Some(100.0));
    }

    #[test]
    fn nothing_scheduled_claims_no_coverage_and_old_reads_are_forgotten() {
        let mut ledger = CoverageLedger::default();
        ledger.record("replay-1", "procfs.cpu", 1, 5);
        assert_eq!(ledger.report(&[], 0, SECOND).percent, None);
        ledger.forget_before(6);
        let report = ledger.report(&two_groups(), 0, 10 * SECOND);
        assert_eq!(report.covered_slots, 0);
        assert_eq!(report.percent, Some(0.0));
    }
}
