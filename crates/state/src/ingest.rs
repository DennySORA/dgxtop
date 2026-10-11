//! Turns a validated batch into history appends.

use std::collections::BTreeMap;
use std::sync::Arc;

use dgxtop_core::canonical::{CanonicalValue, CollectionBatch};
use dgxtop_core::clock::ms_to_ns;
use dgxtop_core::history::HistoryMethod;
use dgxtop_core::identity::{MetricKey, SourceId};
use dgxtop_core::naming::{PeriodClass, READ_GROUPS, is_replay_adapter};
use dgxtop_core::ports::{
    CutAppend, GaugeAppend, HistoryAppends, IngressBatch, IntervalAppend, SegmentIdentity,
};
use dgxtop_core::quality::{SampleKind, SelectionState};

use super::validate::CheckedBatch;
use dgxtop_core::sampling::SamplingTiming;
use dgxtop_core::sampling::stale_deadline;

/// Period of an adapter read group (replay groups mirror `<adapter>.<group>`;
/// the `:<page>` suffix of paged groups is ignored, see
/// `dgxtop_core::observation::split_page`). Unknown groups use the fast
/// period, which can only shorten (never stretch) a stale deadline.
fn group_period_ns(timing: &SamplingTiming, adapter: &str, read_group: &str) -> u64 {
    let (unpaged, _page) = dgxtop_core::observation::split_page(read_group);
    let (adapter, base) = if is_replay_adapter(adapter) {
        unpaged.split_once('.').unwrap_or((adapter, unpaged))
    } else {
        (adapter, unpaged)
    };
    let class = READ_GROUPS
        .iter()
        .find(|spec| spec.adapter == adapter && spec.group == base)
        .map(|spec| spec.period);
    match class {
        Some(PeriodClass::Capacity) => timing.capacity_period_ns,
        Some(PeriodClass::SlowHealth) => timing.slow_period_ns,
        Some(PeriodClass::Fast) | None => timing.fast_period_ns,
    }
}

/// Known period of every source read by the batch's inputs:
/// `max(read group period, source update interval)`, as for freshness.
pub(crate) fn source_periods(
    inputs: &[Arc<IngressBatch>],
    timing: &SamplingTiming,
) -> BTreeMap<SourceId, u64> {
    let mut periods = BTreeMap::new();
    for input in inputs {
        let batch = input.batch();
        let group = group_period_ns(timing, batch.adapter_id.as_str(), batch.read_group.as_str());
        for observation in &batch.records {
            let update = observation
                .source_update_interval_ms
                .map_or(0, |interval| ms_to_ns(u64::from(interval)));
            let period = group.max(update);
            periods
                .entry(observation.provenance.source_id.clone())
                .and_modify(|known: &mut u64| *known = (*known).max(period))
                .or_insert(period);
        }
    }
    periods
}

fn cut_reason(value: &CanonicalValue) -> String {
    if let Some(code) = value.reason_codes.first() {
        return code.clone();
    }
    match value.selection_state {
        SelectionState::Conflict => "conflict",
        SelectionState::Unavailable => "unavailable",
        SelectionState::Selected | SelectionState::Degraded => "not_history_eligible",
    }
    .to_owned()
}

/// History appends of a batch: eligible values become steps, other values of
/// history metrics become cuts (the previous value is not extended), and
/// counter intervals of history metrics become exact intervals.
pub(crate) fn history_appends(
    batch: &CollectionBatch,
    checked: &CheckedBatch,
    periods: &BTreeMap<SourceId, u64>,
    timing: &SamplingTiming,
) -> HistoryAppends {
    let mut appends = HistoryAppends::default();
    for (value, descriptor) in batch.canonical.iter().zip(&checked.value_descriptors) {
        if !descriptor.history {
            continue;
        }
        let time = value.sample_time_boot_ns();
        let period = value
            .source_id
            .as_ref()
            .and_then(|source| periods.get(source))
            .copied()
            .unwrap_or(timing.fast_period_ns);
        match (value.value, &value.source_id, value.source_generation) {
            (Some(scalar), Some(source), Some(generation)) if value.eligible_for_history => {
                appends.gauges.push(GaugeAppend {
                    key: value.key.clone(),
                    descriptor,
                    identity: SegmentIdentity {
                        source_id: source.clone(),
                        source_generation: generation,
                        policy_revision: value.policy_revision,
                        sample_kind: value.sample_kind,
                        method: HistoryMethod::TimeWeightedStep,
                    },
                    unit: value.unit,
                    time_ns: time,
                    stale_deadline_ns: stale_deadline(time, timing.stale_factor, period),
                    value: scalar,
                });
            }
            _ => appends.cuts.push(CutAppend {
                key: value.key.clone(),
                time_ns: time,
                gap_end_ns: time.saturating_add(period),
                reason: cut_reason(value),
            }),
        }
    }
    let kinds: BTreeMap<&MetricKey, SampleKind> = batch
        .canonical
        .iter()
        .map(|value| (&value.key, value.sample_kind))
        .collect();
    for (interval, descriptor) in batch
        .counter_intervals
        .iter()
        .zip(&checked.interval_descriptors)
    {
        if !descriptor.history {
            continue;
        }
        appends.intervals.push(IntervalAppend {
            key: interval.key.clone(),
            descriptor,
            identity: SegmentIdentity {
                source_id: interval.source_id.clone(),
                source_generation: interval.source_generation,
                policy_revision: batch.policy_revision.max(1),
                sample_kind: kinds
                    .get(&interval.key)
                    .copied()
                    .unwrap_or(SampleKind::Derived),
                method: HistoryMethod::CounterDelta,
            },
            unit: interval.unit,
            start_ns: interval.start_boot_ns,
            end_ns: interval.end_boot_ns,
            delta: interval.delta,
        });
    }
    appends
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::config::Config;
    use dgxtop_core::testing::{ObservationBuilder, batch_of, metric_key, source_id};

    #[test]
    fn periods_follow_read_groups_and_update_intervals() {
        let timing = SamplingTiming::from_sampling(&Config::defaults().unwrap().sampling);
        assert_eq!(
            group_period_ns(&timing, "nvml", "fast"),
            timing.fast_period_ns
        );
        assert_eq!(
            group_period_ns(&timing, "nvml", "slow_health"),
            timing.slow_period_ns
        );
        assert_eq!(
            group_period_ns(&timing, "filesystem", "capacity:2"),
            timing.capacity_period_ns
        );
        assert_eq!(
            group_period_ns(&timing, "replay-1", "filesystem.capacity:1"),
            timing.capacity_period_ns
        );
        assert_eq!(
            group_period_ns(&timing, "replay-1", "filesystem.capacity"),
            timing.capacity_period_ns
        );
        assert_eq!(
            group_period_ns(&timing, "unknown", "x"),
            timing.fast_period_ns
        );

        let key = metric_key("gpu:0", "gpu.temperature", "gpu");
        let mut slow = ObservationBuilder::new(key.clone(), "nvml", "nvml:0:temp")
            .f64(1.0)
            .build();
        slow.source_update_interval_ms = Some(10_000);
        let fast = ObservationBuilder::new(key, "nvml", "nvml:0:power")
            .f64(1.0)
            .build();
        let input = Arc::new(IngressBatch::new(
            batch_of("nvml", "fast", 1, vec![slow, fast]),
            None,
        ));
        let periods = source_periods(&[input], &timing);
        assert_eq!(
            periods[&source_id("nvml:0:temp")],
            10 * timing.fast_period_ns
        );
        assert_eq!(periods[&source_id("nvml:0:power")], timing.fast_period_ns);
    }
}
