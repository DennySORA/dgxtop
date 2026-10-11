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
    use dgxtop_core::canonical::{CollectorState, CounterInterval};
    use dgxtop_core::config::Config;
    use dgxtop_core::identity::CollectorId;
    use dgxtop_core::metric::{CanonicalUnit, METRIC_CATALOG, MetricDescriptor, Scalar, ids};
    use dgxtop_core::quality::{Freshness, Quality};
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

    const SECOND: u64 = 1_000_000_000;

    fn timing() -> SamplingTiming {
        SamplingTiming::from_sampling(&Config::defaults().unwrap().sampling)
    }

    fn descriptor(metric_id: &str) -> &'static MetricDescriptor {
        dgxtop_core::metric::descriptor(metric_id).unwrap()
    }

    fn no_history() -> &'static MetricDescriptor {
        METRIC_CATALOG.iter().find(|known| !known.history).unwrap()
    }

    fn temperature(at: u64) -> CanonicalValue {
        CanonicalValue {
            key: metric_key("gpu:0", ids::GPU_TEMPERATURE, "gpu"),
            cycle_id: 1,
            source_id: Some(source_id("nvml:0:temp")),
            source_generation: Some(1),
            unit: CanonicalUnit::DegC,
            value: Some(Scalar::from_f64(50.0).unwrap()),
            sample_kind: SampleKind::Measured,
            selection_state: SelectionState::Selected,
            quality: Quality::Verified,
            freshness: Freshness::Fresh,
            measurement_freshness: Freshness::Unknown,
            observed_boot_ns: None,
            read_finished_boot_ns: at,
            policy_revision: 3,
            candidate_source_ids: Vec::new(),
            reason_codes: Vec::new(),
            eligible_for_history: true,
        }
    }

    fn rx_interval() -> CounterInterval {
        CounterInterval {
            key: metric_key("netif:ns:2", ids::NET_RX_BPS, "netif"),
            source_id: source_id("network:eth0:rx"),
            source_generation: 1,
            start_boot_ns: 0,
            end_boot_ns: SECOND,
            delta: Scalar::from_u64(100),
            unit: CanonicalUnit::Bytes,
        }
    }

    fn batch(canonical: Vec<CanonicalValue>, intervals: Vec<CounterInterval>) -> CollectionBatch {
        CollectionBatch {
            batch_id: uuid::Uuid::nil(),
            collector_id: CollectorId::new("gpu").unwrap(),
            cycle_id: 1,
            epoch: 1,
            config_revision: 1,
            catalog_revision: 1,
            policy_revision: 0,
            state_base_revision: 0,
            canonical,
            explanations: Vec::new(),
            counter_intervals: intervals,
            inputs: Vec::new(),
            process_table: None,
            next_state: CollectorState::default(),
            failures: Vec::new(),
        }
    }

    fn checked(
        values: Vec<&'static MetricDescriptor>,
        intervals: Vec<&'static MetricDescriptor>,
    ) -> CheckedBatch {
        CheckedBatch {
            value_descriptors: values,
            interval_descriptors: intervals,
            new_keys: Vec::new(),
        }
    }

    /// An eligible value is a step held until its stale deadline (the
    /// source's period, else the fast period); a value of a metric without
    /// history is left out.
    #[test]
    fn eligible_values_become_steps_until_their_stale_deadline() {
        let timing = timing();
        let batch = batch(
            vec![temperature(5 * SECOND), temperature(6 * SECOND)],
            vec![],
        );
        let checked = checked(vec![descriptor(ids::GPU_TEMPERATURE), no_history()], vec![]);
        let fallback = history_appends(&batch, &checked, &BTreeMap::new(), &timing);
        assert_eq!(fallback.gauges.len(), 1, "the second has no history");
        assert!(fallback.cuts.is_empty());
        let step = &fallback.gauges[0];
        assert_eq!(step.time_ns, 5 * SECOND);
        assert_eq!(step.identity.policy_revision, 3);
        assert_eq!(
            step.stale_deadline_ns,
            5 * SECOND + u64::from(timing.stale_factor) * timing.fast_period_ns
        );
        let periods = BTreeMap::from([(source_id("nvml:0:temp"), 10 * SECOND)]);
        let known = history_appends(&batch, &checked, &periods, &timing);
        assert_eq!(
            known.gauges[0].stale_deadline_ns,
            5 * SECOND + u64::from(timing.stale_factor) * 10 * SECOND
        );
    }

    /// A value that cannot extend the series cuts it, with the first
    /// reason code, else its selection state's reason.
    #[test]
    fn ineligible_values_cut_with_their_reason() {
        let timing = timing();
        let mut coded = temperature(SECOND);
        coded.eligible_for_history = false;
        coded.reason_codes = vec!["sensor_glitch".to_owned(), "later".to_owned()];
        let mut conflict = temperature(SECOND);
        conflict.value = None;
        conflict.selection_state = SelectionState::Conflict;
        let mut unavailable = temperature(SECOND);
        unavailable.source_id = None;
        unavailable.selection_state = SelectionState::Unavailable;
        let mut degraded = temperature(SECOND);
        degraded.source_generation = None;
        degraded.selection_state = SelectionState::Degraded;
        let mut selected = temperature(SECOND);
        selected.eligible_for_history = false;
        let values = vec![coded, conflict, unavailable, degraded, selected];
        let descriptors = vec![descriptor(ids::GPU_TEMPERATURE); values.len()];
        let appends = history_appends(
            &batch(values, vec![]),
            &checked(descriptors, vec![]),
            &BTreeMap::new(),
            &timing,
        );
        assert!(appends.gauges.is_empty());
        let reasons: Vec<&str> = appends.cuts.iter().map(|cut| cut.reason.as_str()).collect();
        assert_eq!(
            reasons,
            vec![
                "sensor_glitch",
                "conflict",
                "unavailable",
                "not_history_eligible",
                "not_history_eligible",
            ]
        );
        assert!(
            appends
                .cuts
                .iter()
                .all(|cut| cut.gap_end_ns == SECOND + timing.fast_period_ns)
        );
    }

    /// Intervals keep the kind of their key's value, else count as derived,
    /// and a batch without a policy revision files them under revision 1.
    #[test]
    fn intervals_take_their_values_kind_or_count_as_derived() {
        let timing = timing();
        let rate = descriptor(ids::NET_RX_BPS);
        let alone = history_appends(
            &batch(vec![], vec![rx_interval()]),
            &checked(vec![], vec![rate]),
            &BTreeMap::new(),
            &timing,
        );
        let interval = &alone.intervals[0];
        assert_eq!(interval.identity.sample_kind, SampleKind::Derived);
        assert_eq!(interval.identity.policy_revision, 1);
        assert_eq!(interval.identity.method, HistoryMethod::CounterDelta);
        assert_eq!((interval.start_ns, interval.end_ns), (0, SECOND));

        let mut measured = temperature(SECOND);
        measured.key = rx_interval().key;
        let mut with_value = batch(vec![measured], vec![rx_interval()]);
        with_value.policy_revision = 4;
        let paired = history_appends(
            &with_value,
            &checked(vec![rate], vec![rate]),
            &BTreeMap::new(),
            &timing,
        );
        assert_eq!(
            paired.intervals[0].identity.sample_kind,
            SampleKind::Measured
        );
        assert_eq!(paired.intervals[0].identity.policy_revision, 4);

        let dropped = history_appends(
            &batch(vec![], vec![rx_interval()]),
            &checked(vec![], vec![no_history()]),
            &BTreeMap::new(),
            &timing,
        );
        assert!(dropped.intervals.is_empty(), "no history, no interval");
    }
}
