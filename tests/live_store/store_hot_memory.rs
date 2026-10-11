//! AT13/AT25 (retired snapshots stay accounted, only the latest is kept),
//! eviction under a tiny budget, series caps and slot generations.

#[path = "store_hot_support.rs"]
mod support;

use dgxtop_core::budget::{ByteLedger, Partition, PartitionLimits};
use dgxtop_core::canonical::CanonicalValue;
use dgxtop_core::history::HistoryStat;
use dgxtop_core::ports::{HistoryReader, SnapshotReader};
use dgxtop_core::quality::Namespace;
use support::{Harness, MINUTE, SECOND, assert_well_formed, config, gauge, temperature_key};

fn wide_batch(count: usize, value: f64, at: u64) -> Vec<CanonicalValue> {
    (0..count)
        .map(|index| gauge(&temperature_key(&format!("gpu:{index}")), value, at))
        .collect()
}

#[test]
fn at13_snapshots_held_by_a_slow_reader_stay_accounted() {
    let mut harness = Harness::new();
    harness.commit(wide_batch(100, 50.0, SECOND), vec![], SECOND);
    harness.maintain(SECOND);
    let single = harness.ledger.used(Partition::HotState);
    let held = harness.snapshots.latest();
    for second in 2..=21u64 {
        harness.commit(
            wide_batch(100, 50.0 + second as f64, second * SECOND),
            vec![],
            second * SECOND,
        );
    }
    harness.maintain(22 * SECOND);
    let with_reader = harness.ledger.used(Partition::HotState);
    assert_eq!(held.revision, 1);
    assert!(
        with_reader > single,
        "the retired revision held by the reader is still accounted"
    );
    drop(held);
    harness.maintain(23 * SECOND);
    let released = harness.ledger.used(Partition::HotState);
    assert!(released < with_reader);
    assert!(
        released <= single + single / 2,
        "only the latest snapshot remains: {released} vs {single}"
    );
}

#[test]
fn at25_the_store_retains_only_the_latest_snapshot() {
    let mut harness = Harness::new();
    harness.commit(wide_batch(50, 1.0, SECOND), vec![], SECOND);
    harness.maintain(SECOND);
    let single = harness.ledger.used(Partition::HotState);
    for second in 2..=200u64 {
        harness.commit(
            wide_batch(50, second as f64, second * SECOND),
            vec![],
            second * SECOND,
        );
        assert!(
            harness.ledger.used(Partition::HotState) <= 3 * single,
            "commits do not accumulate snapshots"
        );
    }
    harness.maintain(201 * SECOND);
    let after = harness.ledger.used(Partition::HotState);
    assert!(after <= single + single / 2, "{after} vs {single}");
    assert_eq!(harness.snapshots.latest().revision, 200);
}

#[test]
fn eviction_under_a_tiny_budget_never_exceeds_partitions_and_reports_truncation() {
    let ledger = ByteLedger::new(PartitionLimits::for_memory_limit_mib(1));
    let mut settings = config();
    settings.sampling.interval_ms = 5000;
    let mut harness = Harness::build(settings, ledger.clone(), Namespace::Live, false);
    let keys: Vec<_> = (0..10)
        .map(|index| temperature_key(&format!("gpu:{index}")))
        .collect();
    // Eighteen hours of minute rollups for ten series do not fit the
    // 512 KiB aggregates partition of a 1 MiB budget (D-84).
    let hours = 18u64;
    for second in (0..hours * 3600).step_by(5) {
        let at = second * SECOND;
        let values = keys.iter().map(|key| gauge(key, 30.0, at)).collect();
        let ack = harness.commit(values, vec![], at);
        assert!(ack.status.is_committed(), "{ack:?}");
        if second % 60 == 0 {
            harness.maintain(at);
        }
    }
    let now = hours * 3600 * SECOND;
    harness.maintain(now);
    let snapshot = harness.snapshots.latest();
    for usage in &snapshot.health.ledger.as_ref().unwrap().partitions {
        assert!(
            usage.peak <= usage.limit,
            "{:?} peak {} limit {}",
            usage.partition,
            usage.peak,
            usage.limit
        );
    }
    assert!(
        snapshot.health.truncated_history,
        "aggregates were shortened"
    );
    assert!(snapshot.health.evicted_total > 0);
    assert!(
        snapshot
            .recent_gaps
            .iter()
            .any(|gap| gap.reason == "evicted")
    );
    // A 1 MiB budget leaves 64 KiB for query results: ask for few points.
    let full = harness
        .query(&keys[0], (0, now), 60_000, HistoryStat::Mean, 100)
        .unwrap();
    assert_well_formed(&full);
    assert!(
        full.coverage_ratio < 1.0,
        "truncated history is not presented as complete"
    );
    let evicted = full
        .points
        .iter()
        .find(|point| point.value.is_none())
        .unwrap();
    assert_eq!(evicted.start_boot_ns, 0);
    assert_eq!(evicted.reason.as_deref(), Some("evicted"));
    let recent = harness
        .query(
            &keys[0],
            (now - 10 * MINUTE, now),
            60_000,
            HistoryStat::Mean,
            100,
        )
        .unwrap();
    assert!(
        recent.coverage_ratio > 0.98,
        "recent history survives eviction: {}",
        recent.coverage_ratio
    );
}

#[test]
fn series_over_the_cap_are_rejected_visibly_without_touching_the_snapshot() {
    let mut settings = config();
    settings.store.max_series = 2;
    let mut harness = Harness::with_config(settings);
    harness.commit(wide_batch(3, 20.0, SECOND), vec![], SECOND);
    let snapshot = harness.snapshots.latest();
    assert_eq!(snapshot.health.series, 2);
    assert!(
        snapshot.sample(&temperature_key("gpu:2")).is_some(),
        "the snapshot keeps the value"
    );
    assert_eq!(harness.snapshots.counters().series_rejected_total, 1);
    assert!(
        snapshot
            .recent_gaps
            .iter()
            .any(|gap| gap.reason == "series_capacity")
    );
    assert_eq!(harness.history.series().len(), 2);
}

#[test]
fn series_slots_are_never_reused_for_another_series_after_churn() {
    let mut settings = config();
    settings.store.max_series = 2;
    settings.store.raw_history_seconds = 30;
    settings.store.aggregate_retention_hours = 1;
    let mut harness = Harness::with_config(settings);
    let first = temperature_key("gpu:old-a");
    let second = temperature_key("gpu:old-b");
    for tick in 0..10u64 {
        harness.commit(
            vec![
                gauge(&first, 11.0, tick * SECOND),
                gauge(&second, 12.0, tick * SECOND),
            ],
            vec![],
            tick * SECOND,
        );
    }
    let later = 3 * 3600 * SECOND;
    harness.maintain(later);
    harness.maintain(later + MINUTE);
    assert!(
        harness.history.series().is_empty(),
        "aged-out series are reclaimed"
    );
    let replacement = temperature_key("gpu:new");
    let start = later + 2 * MINUTE;
    for tick in 0..10u64 {
        harness.commit(
            vec![gauge(&replacement, 99.0, start + tick * SECOND)],
            vec![],
            start + tick * SECOND,
        );
    }
    assert_eq!(harness.history.series(), vec![replacement.clone()]);
    harness.clock.set_boot_ns(start + MINUTE);
    let old = harness
        .query(&first, (0, start + MINUTE), 0, HistoryStat::Mean, 100)
        .unwrap();
    assert!(
        old.points.is_empty(),
        "the old identity resolves to nothing"
    );
    let new = harness
        .query(
            &replacement,
            (0, start + MINUTE),
            60_000,
            HistoryStat::Max,
            2000,
        )
        .unwrap();
    assert!(
        new.points
            .iter()
            .filter_map(|point| point.value)
            .all(|value| value.to_f64_lossy() == 99.0)
    );
    assert_eq!(new.covered_ns, 10 * SECOND + 2 * SECOND);
}

#[test]
fn relieving_pressure_sheds_raw_history_beyond_half_its_retention() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    for second in 0..600u64 {
        let at = second * SECOND;
        let mut batch = harness.batch(vec![gauge(&key, 50.0, at)], vec![]);
        batch.inputs = vec![support::raw_input(&key, "nvml:0:temp", at)];
        harness.commit_batch(batch, at);
    }
    let now = 600 * SECOND;
    harness.maintain(now);
    let raw_before = harness.ledger.used(Partition::RawHistory);
    let evicted_before = harness.snapshots.counters().evicted_records_total;
    harness.writer.relieve_pressure(now);
    assert!(harness.ledger.used(Partition::RawHistory) < raw_before);
    assert!(harness.snapshots.counters().evicted_records_total > evicted_before);
    let result = harness
        .query(&key, (0, now), 60_000, HistoryStat::Mean, 100)
        .unwrap();
    assert!(
        result.coverage_ratio > 0.97,
        "shed raw minutes remain as rollups"
    );
}

#[test]
fn idle_process_keys_and_orphan_explanations_leave_the_snapshot() {
    use dgxtop_core::canonical::Explanation;
    use dgxtop_core::metric::ids;
    use dgxtop_core::quality::SelectionState;
    use dgxtop_core::testing::metric_key;

    let mut harness = Harness::new();
    let process_key = metric_key("process:ns:42:100", ids::PROCESS_RSS, "process");
    let gpu_key = temperature_key("gpu:0");
    let orphan_key = temperature_key("gpu:7");
    let mut batch = harness.batch(
        vec![
            gauge(&process_key, 12.0, SECOND),
            gauge(&gpu_key, 50.0, SECOND),
        ],
        vec![],
    );
    batch.explanations = vec![Explanation {
        key: orphan_key.clone(),
        policy_revision: 1,
        selection_state: SelectionState::Unavailable,
        selected_source: None,
        pinned_source: None,
        candidates: Vec::new(),
        conflict_spread: None,
        tolerance: None,
        switch: None,
        reason_codes: vec!["no_candidate".to_owned()],
    }];
    harness.commit_batch(batch, SECOND);
    assert!(
        harness
            .snapshots
            .latest()
            .explanation(&orphan_key)
            .is_some()
    );
    harness.maintain(10 * SECOND);
    let snapshot = harness.snapshots.latest();
    assert!(snapshot.explanation(&orphan_key).is_none());
    assert!(snapshot.sample(&process_key).is_some());
    harness.maintain(400 * SECOND);
    let snapshot = harness.snapshots.latest();
    assert!(
        snapshot.sample(&process_key).is_none(),
        "exited process keys expire"
    );
    assert!(
        snapshot.sample(&gpu_key).is_some(),
        "device keys live for the aggregate retention"
    );
}
