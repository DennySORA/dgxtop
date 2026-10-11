//! AT05 (atomic single-writer commit handshake) and AT06 (partitions visible,
//! archive independent of the hot store).

#[path = "store_hot_support.rs"]
mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use dgxtop_core::budget::{ByteLedger, Partition, PartitionLimits};
use dgxtop_core::canonical::{
    AckStatus, ArchiveState, CollectorState, CounterBase, GapRecord, ProcessTable,
};
use dgxtop_core::history::HistoryStat;
use dgxtop_core::identity::CollectorId;
use dgxtop_core::metric::{CanonicalUnit, Scalar};
use dgxtop_core::ports::{SnapshotReader, StoreWriter};
use dgxtop_core::quality::{Namespace, SampleKind};
use dgxtop_core::testing::{boot_id, metric_key, source_id};
use dgxtop_core::wire::{format_fixed_width, parse_fixed_width};
use support::{
    Harness, MINUTE, SECOND, conflict, gauge, interval, raw_input, rx_key, temperature_key,
    value_f64,
};

fn raw_point_count(harness: &Harness, key: &dgxtop_core::identity::MetricKey, end: u64) -> usize {
    harness.clock.set_boot_ns(end);
    harness
        .query(key, (0, end), 0, HistoryStat::Raw, 2000)
        .unwrap_or_else(|error| panic!("raw query failed: {error}"))
        .points
        .iter()
        .filter(|point| point.value.is_some())
        .count()
}

#[test]
fn at05_commit_applies_samples_history_and_state_atomically() {
    let mut harness = Harness::new();
    let key = rx_key("netif:ns:2");
    let mut batch = harness.batch(
        vec![gauge(&key, 100.0, 3 * SECOND)],
        vec![interval(&key, 0, 3 * SECOND, 300)],
    );
    let mut next_state = CollectorState::default();
    next_state.counters.insert(
        source_id("network:eth0:rx"),
        CounterBase {
            value: 1300,
            boot_id: boot_id("fake-boot"),
            generation: 1,
            sample_boot_ns: 3 * SECOND,
        },
    );
    batch.next_state = next_state;
    let ack = harness.commit_batch(batch, 3 * SECOND);
    assert_eq!(ack.status, AckStatus::Committed);
    assert_eq!(ack.committed_revision, 1);
    assert_eq!(ack.state_revision, 1);
    assert_eq!(ack.archive, ArchiveState::Disabled);

    let snapshot = harness.snapshots.latest();
    assert_eq!(snapshot.revision, 1);
    let entry = snapshot.sample(&key).unwrap();
    assert_eq!(entry.sample.snapshot_revision, 1);
    assert_eq!(entry.sample.committed_boot_ns, 3 * SECOND);
    assert_eq!(
        entry.last_good.as_ref().unwrap().value.to_f64_lossy(),
        100.0
    );
    let (state_revision, state) = snapshot.collector_state(&harness.collector);
    assert_eq!(state_revision, 1);
    assert_eq!(state.revision, 1);
    assert_eq!(state.counters[&source_id("network:eth0:rx")].value, 1300);

    let history = harness.history.clone();
    assert_eq!(
        dgxtop_core::ports::HistoryReader::series(&history),
        vec![key.clone()]
    );
    harness.clock.set_boot_ns(4 * SECOND);
    let total = harness
        .query(&key, (0, 10 * SECOND), 0, HistoryStat::Total, 100)
        .unwrap();
    assert_eq!(total.snapshot_revision, 1);
    let delta: Vec<u64> = total
        .points
        .iter()
        .filter_map(|point| point.value.and_then(Scalar::as_u64))
        .collect();
    assert_eq!(delta, vec![300]);
}

#[test]
fn at05_stale_epoch_rejects_the_whole_batch_and_changes_nothing() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    harness.commit(vec![gauge(&key, 50.0, SECOND)], vec![], SECOND);
    let before = harness.snapshots.latest();
    let history_before = raw_point_count(&harness, &key, 10 * SECOND);

    // Batch computed for epoch 1 while the Manager already moved to epoch 2.
    let batch = harness.batch(vec![gauge(&key, 99.0, 2 * SECOND)], vec![]);
    let mut guard = Harness::guard_for(&batch);
    guard.expected_epoch = 2;
    let ack = harness.writer.commit_collection(batch, &guard, 2 * SECOND);
    assert_eq!(ack.status, AckStatus::RejectedStaleEpoch);
    assert!(ack.reason.unwrap().contains("epoch"));
    assert_eq!(ack.committed_revision, 1);
    assert_eq!(ack.state_revision, 1);

    // An announced epoch makes guards expecting another epoch stale too.
    harness.writer.set_epoch(3);
    harness.epoch = 2;
    let ack = harness.commit(vec![gauge(&key, 98.0, 3 * SECOND)], vec![], 3 * SECOND);
    assert_eq!(ack.status, AckStatus::RejectedStaleEpoch);

    let after = harness.snapshots.latest();
    assert!(
        Arc::ptr_eq(&before, &after),
        "no new revision was published"
    );
    assert_eq!(raw_point_count(&harness, &key, 10 * SECOND), history_before);
    assert_eq!(after.collector_state(&harness.collector).0, 1);
    assert_eq!(harness.snapshots.counters().rejected_batches_total, 2);

    // Once the batch uses the announced epoch it commits; older epochs then stay stale.
    harness.epoch = 3;
    assert_eq!(
        harness
            .commit(vec![gauge(&key, 51.0, 4 * SECOND)], vec![], 4 * SECOND)
            .status,
        AckStatus::Committed
    );
    harness.epoch = 2;
    harness.writer.set_epoch(2);
    let ack = harness.commit(vec![gauge(&key, 52.0, 5 * SECOND)], vec![], 5 * SECOND);
    assert_eq!(ack.status, AckStatus::RejectedStaleEpoch);
}

#[test]
fn at05_stale_base_rejects_the_whole_batch() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    harness.commit(vec![gauge(&key, 50.0, SECOND)], vec![], SECOND);
    let mut batch = harness.batch(vec![gauge(&key, 70.0, 2 * SECOND)], vec![]);
    batch.state_base_revision = 0;
    let before = harness.snapshots.latest();
    let ack = harness.commit_batch(batch, 2 * SECOND);
    assert_eq!(ack.status, AckStatus::RejectedStaleBase);
    assert!(Arc::ptr_eq(&before, &harness.snapshots.latest()));
    assert_eq!(raw_point_count(&harness, &key, 10 * SECOND), 1);

    // Guard and batch disagreeing about the base is stale as well.
    let batch = harness.batch(vec![gauge(&key, 70.0, 2 * SECOND)], vec![]);
    let mut guard = Harness::guard_for(&batch);
    guard.state_base_revision += 5;
    let ack = harness.writer.commit_collection(batch, &guard, 2 * SECOND);
    assert_eq!(ack.status, AckStatus::RejectedStaleBase);

    // The collector base only advances on a commit.
    assert_eq!(harness.base(), 1);
    assert_eq!(
        harness
            .commit(vec![gauge(&key, 70.0, 2 * SECOND)], vec![], 2 * SECOND)
            .state_revision,
        2
    );
}

#[test]
fn at05_duplicate_batch_returns_the_original_ack_without_duplicate_history() {
    let mut harness = Harness::new();
    let key = rx_key("netif:ns:2");
    let batch = harness.batch(
        vec![gauge(&key, 100.0, 3 * SECOND)],
        vec![interval(&key, 0, 3 * SECOND, 300)],
    );
    let resend = batch.clone();
    let original = harness.commit_batch(batch, 3 * SECOND);
    assert_eq!(original.status, AckStatus::Committed);
    let duplicate = harness.commit_batch(resend, 4 * SECOND);
    assert_eq!(duplicate.status, AckStatus::Duplicate);
    assert_eq!(duplicate.committed_revision, original.committed_revision);
    assert_eq!(duplicate.state_revision, original.state_revision);
    assert!(duplicate.status.is_committed());
    assert_eq!(harness.snapshots.latest().revision, 1);
    assert_eq!(raw_point_count(&harness, &key, 10 * SECOND), 1);
    harness.clock.set_boot_ns(5 * SECOND);
    let total = harness
        .query(&key, (0, 10 * SECOND), 10_000, HistoryStat::Total, 10)
        .unwrap();
    assert_eq!(
        support::value_u64(&total.points[0]),
        300,
        "the interval is not counted twice"
    );
    assert_eq!(harness.snapshots.counters().duplicate_batches_total, 1);
}

#[test]
fn at05_same_batch_id_with_different_content_is_a_conflict() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    let batch = harness.batch(vec![gauge(&key, 50.0, SECOND)], vec![]);
    let mut altered = batch.clone();
    altered.canonical[0].value = Some(Scalar::from_f64(51.0).unwrap());
    harness.commit_batch(batch, SECOND);
    let ack = harness.commit_batch(altered, 2 * SECOND);
    assert_eq!(ack.status, AckStatus::Conflict);
    assert!(!ack.status.is_committed());
    assert_eq!(
        harness
            .snapshots
            .latest()
            .sample(&key)
            .unwrap()
            .sample
            .value,
        Some(Scalar::from_f64(50.0).unwrap())
    );
}

#[test]
fn at05_invalid_namespace_unit_and_unknown_metrics_reject_the_whole_batch() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    let good = gauge(&key, 50.0, SECOND);

    let mut simulated = gauge(&temperature_key("gpu:1"), 40.0, SECOND);
    simulated.sample_kind = SampleKind::Simulated;
    simulated.eligible_for_history = false;
    let mut wrong_unit = gauge(&temperature_key("gpu:2"), 40.0, SECOND);
    wrong_unit.unit = CanonicalUnit::Watts;
    let unknown = gauge(
        &metric_key("gpu:3", "gpu.no_such_metric", "gpu"),
        1.0,
        SECOND,
    );
    let mut invalid = gauge(&temperature_key("gpu:4"), 40.0, SECOND);
    invalid.policy_revision = 0;
    let duplicate_key = gauge(&key, 51.0, SECOND);
    let mut negative = interval(&rx_key("netif:ns:2"), 0, SECOND, 0);
    negative.delta = Scalar::from_f64(-1.0).unwrap();
    let mut bad_interval_unit = interval(&rx_key("netif:ns:2"), 0, SECOND, 5);
    bad_interval_unit.unit = CanonicalUnit::Count;
    let backwards = interval(&rx_key("netif:ns:2"), SECOND, SECOND, 5);

    let cases = vec![
        (vec![good.clone(), simulated], vec![], "namespace"),
        (vec![good.clone(), wrong_unit], vec![], "unit"),
        (vec![good.clone(), unknown], vec![], "unknown metric"),
        (vec![good.clone(), invalid], vec![], "policy_revision"),
        (vec![good.clone(), duplicate_key], vec![], "twice"),
        (vec![good.clone()], vec![negative], "negative"),
        (vec![good.clone()], vec![bad_interval_unit], "integrate"),
        (vec![good.clone()], vec![backwards], "follow"),
    ];
    for (values, intervals, expected) in cases {
        let ack = harness.commit(values, intervals, SECOND);
        assert_eq!(ack.status, AckStatus::RejectedInvalid, "{expected}");
        assert!(
            ack.reason.as_deref().unwrap().contains(expected),
            "{expected}: {:?}",
            ack.reason
        );
    }
    assert_eq!(harness.snapshots.latest().revision, 0);
    assert!(dgxtop_core::ports::HistoryReader::series(&harness.history).is_empty());
    assert_eq!(
        harness.commit(vec![good], vec![], SECOND).status,
        AckStatus::Committed
    );
}

#[test]
fn at05_replay_store_rejects_live_values() {
    let mut harness = Harness::build(
        support::config(),
        ByteLedger::for_memory_limit_mib(256),
        Namespace::Replay,
        false,
    );
    let key = temperature_key("gpu:0");
    assert_eq!(
        harness
            .commit(vec![gauge(&key, 50.0, SECOND)], vec![], SECOND)
            .status,
        AckStatus::RejectedInvalid
    );
    let mut replayed = gauge(&key, 50.0, SECOND);
    replayed.sample_kind = SampleKind::Replay;
    replayed.eligible_for_history = false;
    assert_eq!(
        harness.commit(vec![replayed], vec![], SECOND).status,
        AckStatus::Committed
    );
    assert!(
        dgxtop_core::ports::HistoryReader::series(&harness.history).is_empty(),
        "replay values never enter history"
    );
}

#[test]
fn at05_capacity_rejection_leaves_no_partial_state() {
    let ledger = ByteLedger::new(PartitionLimits::for_memory_limit_mib(1));
    let mut harness = Harness::build(support::config(), ledger.clone(), Namespace::Live, false);
    let values: Vec<_> = (0..400)
        .map(|index| gauge(&temperature_key(&format!("gpu:{index}")), 40.0, SECOND))
        .collect();
    let hot_state_before = ledger.used(Partition::HotState);
    let ack = harness.commit(values, vec![], SECOND);
    assert_eq!(ack.status, AckStatus::RejectedCapacity);
    assert!(ack.reason.unwrap().contains("hot_state"));
    assert_eq!(harness.snapshots.latest().revision, 0);
    assert!(dgxtop_core::ports::HistoryReader::series(&harness.history).is_empty());
    assert_eq!(ledger.used(Partition::RawHistory), 0);
    assert_eq!(ledger.used(Partition::HotState), hot_state_before);
    let small = harness.commit(
        vec![gauge(&temperature_key("gpu:0"), 40.0, SECOND)],
        vec![],
        SECOND,
    );
    assert_eq!(small.status, AckStatus::Committed);
}

#[test]
fn keys_are_owned_by_one_collector_and_process_tables_are_replaced() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    harness.commit(vec![gauge(&key, 50.0, SECOND)], vec![], SECOND);
    let other = CollectorId::new("gpu").unwrap();
    let mut batch = harness.batch(vec![gauge(&key, 50.0, 2 * SECOND)], vec![]);
    batch.collector_id = other.clone();
    batch.state_base_revision = 0;
    let ack = harness.commit_batch(batch, 2 * SECOND);
    assert_eq!(ack.status, AckStatus::RejectedInvalid);
    assert!(ack.reason.unwrap().contains("owned by collector network"));

    let mut batch = harness.batch(vec![], vec![]);
    batch.process_table = Some(ProcessTable {
        total_seen: 3,
        cycle_id: 9,
        ..ProcessTable::default()
    });
    assert_eq!(
        harness.commit_batch(batch, 3 * SECOND).status,
        AckStatus::Committed
    );
    assert_eq!(harness.snapshots.latest().process_table.total_seen, 3);
    let batch = harness.batch(vec![], vec![]);
    harness.commit_batch(batch, 4 * SECOND);
    assert_eq!(
        harness.snapshots.latest().process_table.total_seen,
        3,
        "absent table keeps the previous one"
    );
}

#[test]
fn missing_values_never_refresh_last_good() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    harness.commit(vec![gauge(&key, 61.0, SECOND)], vec![], SECOND);
    harness.commit(vec![conflict(&key, 2 * SECOND)], vec![], 2 * SECOND);
    let snapshot = harness.snapshots.latest();
    let entry = snapshot.sample(&key).unwrap();
    assert!(entry.sample.value.is_none());
    let last_good = entry.last_good.as_ref().unwrap();
    assert_eq!(last_good.value.to_f64_lossy(), 61.0);
    assert_eq!(last_good.sample_boot_ns, SECOND);
    // The conflict cut the step and left a visible gap with its reason.
    harness.clock.set_boot_ns(10 * SECOND);
    let result = harness
        .query(&key, (0, 10 * SECOND), 0, HistoryStat::Mean, 100)
        .unwrap();
    assert_eq!(result.covered_ns, SECOND);
    let gap = result
        .points
        .iter()
        .find(|point| point.start_boot_ns == 2 * SECOND)
        .unwrap();
    assert!(gap.value.is_none());
    assert_eq!(gap.reason.as_deref(), Some("source_conflict"));
    assert!(
        snapshot
            .recent_gaps
            .iter()
            .any(|gap| gap.reason == "source_conflict")
    );
}

#[test]
fn at06_partition_usage_is_visible_in_store_health() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    for second in 1..=5u64 {
        let mut batch = harness.batch(vec![gauge(&key, 50.0, second * SECOND)], vec![]);
        batch.inputs = vec![raw_input(&key, "nvml:0:temp", second * SECOND)];
        harness.commit_batch(batch, second * SECOND);
    }
    let health = harness.snapshots.latest().health.clone();
    let ledger = health.ledger.unwrap();
    let used: BTreeMap<Partition, u64> = ledger
        .partitions
        .iter()
        .map(|usage| (usage.partition, usage.used))
        .collect();
    for partition in [
        Partition::HotState,
        Partition::Catalog,
        Partition::RawHistory,
        Partition::Aggregates,
    ] {
        assert!(used[&partition] > 0, "{partition:?} usage is visible");
    }
    assert_eq!(health.series, 1);
    // The commit's raw batches are not kept alive by the store.
    assert_eq!(harness.ledger.used(Partition::Ingress), 0);
}

#[test]
fn at06_archive_disabled_does_not_affect_the_hot_store() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    for second in 0..200u64 {
        let ack = harness.commit(
            vec![gauge(&key, 50.0, second * SECOND)],
            vec![],
            second * SECOND,
        );
        assert_eq!(ack.archive, ArchiveState::Disabled);
    }
    let reserve_before = harness.ledger.used(Partition::Reserve);
    harness.maintain(200 * SECOND);
    assert!(harness.writer.take_archive_journal(1 << 20).is_none());
    let result = harness
        .query(&key, (0, 200 * SECOND), 60_000, HistoryStat::Mean, 100)
        .unwrap();
    assert!(result.coverage_ratio > 0.99);
    assert_eq!(
        harness.ledger.used(Partition::Reserve),
        reserve_before,
        "nothing is queued for a disabled archive"
    );
}

#[test]
fn at06_archive_enabled_gets_finished_rows_and_at27_keys_sort_numerically() {
    let mut harness = Harness::unpaired(
        support::config(),
        ByteLedger::for_memory_limit_mib(256),
        Namespace::Live,
        true,
    );
    let key = rx_key("netif:ns:2");
    for second in 1..=200u64 {
        let start = (second - 1) * SECOND + SECOND / 2;
        let end = second * SECOND + SECOND / 2;
        let ack = harness.commit(
            vec![gauge(&key, 300.0, end)],
            vec![interval(&key, start, end, 300)],
            end,
        );
        assert_eq!(ack.archive, ArchiveState::Pending);
    }
    harness.writer.record_gap(GapRecord {
        key: Some(key.clone()),
        adapter_id: None,
        start_boot_ns: 10 * SECOND,
        end_boot_ns: 11 * SECOND,
        reason: "timeout".to_owned(),
        affected_count: 1,
    });
    assert!(
        harness.writer.take_archive_journal(1 << 20).is_none(),
        "no UTC mapping yet"
    );
    harness
        .writer
        .observe_wall_clock(0, support::UTC_AT_BOOT_NS);
    harness.maintain(250 * SECOND);
    let journal = harness.writer.take_archive_journal(8 << 20).unwrap();
    assert_eq!(journal.host_id, "test-host");
    assert_eq!(journal.boot_id, "fake-boot");
    assert_eq!(
        journal.segments.len(),
        2,
        "one gauge and one counter segment"
    );
    assert!(journal.buckets.len() >= 6);
    assert!(journal.bridges.iter().all(|bridge| bridge.delta == 300));
    assert!(
        journal
            .bridges
            .iter()
            .any(|bridge| bridge.start_boot_ns == 59 * SECOND + SECOND / 2)
    );
    assert!(journal.gaps.iter().any(|gap| gap.reason == "timeout"));
    let counter_total: u128 = journal
        .buckets
        .iter()
        .filter_map(|bucket| bucket.counter_delta)
        .sum();
    let bridge_total: u128 = journal.bridges.iter().map(|bridge| bridge.delta).sum();
    // Minutes before 180 s are finished; the 179 intervals ending by then
    // (including the bridges across 60 s and 120 s) are each counted once.
    assert_eq!(counter_total + bridge_total, 300 * 179);
    assert!(journal.permit.is_some());

    // Fixed-width keys of the archived rows sort lexically like numbers.
    let mut keys: Vec<String> = journal
        .buckets
        .iter()
        .map(|bucket| format_fixed_width(bucket.start_boot_ns))
        .chain(
            journal
                .bridges
                .iter()
                .map(|bridge| format_fixed_width(bridge.start_boot_ns)),
        )
        .collect();
    keys.sort();
    let numbers: Vec<u64> = keys
        .iter()
        .map(|key| parse_fixed_width(key).unwrap())
        .collect();
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    assert_eq!(numbers, sorted);
    let mut small: Vec<String> = [100u64, 2, 10].map(format_fixed_width).to_vec();
    small.sort();
    let small: Vec<u64> = small
        .iter()
        .map(|key| parse_fixed_width(key).unwrap())
        .collect();
    assert_eq!(small, vec![2, 10, 100]);
    // Archived minutes start on whole UTC minutes (D-82).
    assert_eq!(
        journal.buckets[0].start_utc,
        "2026-09-21T14:14:00.000000000Z"
    );
    assert!(harness.writer.take_archive_journal(8 << 20).is_none());
    let reserve_with_journal = harness.ledger.used(Partition::Reserve);
    let journal_bytes = journal.permit.as_ref().map_or(0, |permit| permit.bytes());
    assert!(journal_bytes > 0);
    drop(journal);
    assert_eq!(
        harness.ledger.used(Partition::Reserve),
        reserve_with_journal - journal_bytes,
        "the journal's reservation is released with it"
    );
}

#[test]
fn recorded_gaps_are_visible_and_invalid_gaps_are_counted() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    harness.commit(vec![gauge(&key, 50.0, SECOND)], vec![], SECOND);
    harness.writer.record_gap(GapRecord {
        key: Some(key.clone()),
        adapter_id: None,
        start_boot_ns: 5 * SECOND,
        end_boot_ns: 8 * SECOND,
        reason: "timeout".to_owned(),
        affected_count: 2,
    });
    harness.writer.record_gap(GapRecord {
        key: None,
        adapter_id: None,
        start_boot_ns: 9 * SECOND,
        end_boot_ns: 9 * SECOND,
        reason: "bad".to_owned(),
        affected_count: 1,
    });
    let snapshot = harness.snapshots.latest();
    assert_eq!(
        snapshot.revision, 2,
        "only the valid gap published a revision"
    );
    assert_eq!(snapshot.health.gaps_total, 1);
    assert_eq!(harness.snapshots.counters().invalid_gaps_total, 1);
    assert_eq!(snapshot.recent_gaps.len(), 1);
    harness.clock.set_boot_ns(MINUTE);
    let result = harness
        .query(&key, (4 * SECOND, 9 * SECOND), 1000, HistoryStat::Mean, 100)
        .unwrap();
    assert!(
        result
            .points
            .iter()
            .any(|point| point.reason.as_deref() == Some("timeout"))
    );
    assert_eq!(result.snapshot_revision, 2);
    let mean = harness
        .query(&key, (SECOND, 2 * SECOND), 0, HistoryStat::Mean, 10)
        .unwrap();
    assert_eq!(value_f64(&mean.points[0]), 50.0);
}

#[test]
fn a_sustained_outage_is_one_merged_gap() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    harness.commit(vec![gauge(&key, 50.0, SECOND)], vec![], SECOND);
    for second in 2..=12u64 {
        harness.commit(
            vec![conflict(&key, second * SECOND)],
            vec![],
            second * SECOND,
        );
    }
    let snapshot = harness.snapshots.latest();
    assert_eq!(snapshot.health.gaps_total, 1, "repeats extend the same gap");
    let conflicts: Vec<_> = snapshot
        .recent_gaps
        .iter()
        .filter(|gap| gap.reason == "source_conflict")
        .collect();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].start_boot_ns, 2 * SECOND);
    assert_eq!(conflicts[0].end_boot_ns, 13 * SECOND);
}
