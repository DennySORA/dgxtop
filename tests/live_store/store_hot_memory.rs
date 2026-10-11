//! AT13/AT25 (retired snapshots stay accounted, only the latest is kept),
//! eviction under a tiny budget, series caps and slot generations.

#[path = "store_hot_support.rs"]
mod support;

use dgxtop_core::budget::{ByteLedger, Partition, PartitionLimits};
use dgxtop_core::canonical::CanonicalValue;
use dgxtop_core::ports::SnapshotReader;
use dgxtop_core::quality::Namespace;
use dgxtop_core::tiers::Tier;
use support::{EVERYTHING_NS, Harness, MINUTE, SECOND, config, gauge, temperature_key};

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

/// Finished minutes are handed on, not kept (D-90): eighteen hours of ten
/// series fit a 1 MiB budget without evicting anything, every partition
/// stays under its limit and the Overview still holds the whole time.
#[test]
fn a_tiny_budget_holds_hours_of_history_within_its_partitions() {
    let ledger = ByteLedger::new(PartitionLimits::for_memory_limit_mib(1));
    let mut settings = config();
    settings.sampling.interval_ms = 5000;
    let mut harness = Harness::build(settings, ledger.clone(), Namespace::Live, false);
    let keys: Vec<_> = (0..10)
        .map(|index| temperature_key(&format!("gpu:{index}")))
        .collect();
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
    assert!(!snapshot.health.truncated_history);
    assert!(
        !snapshot
            .recent_gaps
            .iter()
            .any(|gap| gap.reason == "evicted"),
        "nothing was lost to the budget"
    );
    let held = harness.window(&keys[0], EVERYTHING_NS);
    assert!(
        held.covered_ns >= now - MINUTE,
        "the Overview holds the whole time: {} ns",
        held.covered_ns
    );
    let recent = harness.window(&keys[0], 10 * MINUTE);
    assert!(
        recent.covered_ns > 10 * MINUTE * 98 / 100,
        "recent history: {} ns",
        recent.covered_ns
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
    assert_eq!(
        harness
            .window(&temperature_key("gpu:2"), EVERYTHING_NS)
            .samples,
        0,
        "the rejected series holds no history"
    );
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
    assert_eq!(
        harness.snapshots.latest().health.series,
        0,
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
    assert_eq!(harness.snapshots.latest().health.series, 1);
    harness.clock.set_boot_ns(start + MINUTE);
    let old = harness.bars(&first, Tier::OneMinute, (0, start + MINUTE));
    assert!(
        old.iter().all(|bar| bar.maximum == 11.0),
        "the old series keeps only its own values"
    );
    let new = harness.bars(&replacement, Tier::OneMinute, (0, start + MINUTE));
    assert!(new.iter().all(|bar| bar.maximum == 99.0));
    assert_eq!(
        new.iter().map(|bar| bar.covered_ns).sum::<u64>(),
        10 * SECOND + 2 * SECOND
    );
}

#[test]
fn idle_process_keys_and_orphan_explanations_leave_the_snapshot() {
    use dgxtop_core::canonical::Explanation;
    use dgxtop_core::metric::ids;
    use dgxtop_core::quality::SelectionState;
    use dgxtop_core::testing::metric_key;

    let mut harness = Harness::new();
    let process_key = metric_key("process:ns:42:100", ids::PROCESS_CPU_PERCENT, "process");
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
