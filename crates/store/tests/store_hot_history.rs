//! AT07 (real-time history, exact counters, coverage) and AT27 (window
//! exactness, cross-minute bridges, boundary gaps).

#[path = "store_hot_support.rs"]
mod support;

use dgxtop_core::history::{HistoryMethod, HistoryStat};
use dgxtop_core::metric::Scalar;
use dgxtop_core::ports::SnapshotReader;
use support::{
    Harness, MILLI, MINUTE, SECOND, assert_well_formed, config, gauge, gauge_from, interval,
    rx_key, temperature_key, value_f64, value_u64, with_boot,
};

/// One cell spanning any test window: `resolution_ms` of a whole day.
const ONE_CELL_MS: u32 = 86_400_000;

#[test]
fn at07_counter_interval_total_is_exact() {
    let mut harness = Harness::new();
    let key = rx_key("netif:ns:2");
    // 1000 -> 1300 bytes over 3 s: the collector derived 100 B/s and the
    // exact 300 byte interval.
    let ack = harness.commit(
        vec![gauge(&key, 100.0, 3 * SECOND)],
        vec![interval(&key, 0, 3 * SECOND, 300)],
        3 * SECOND,
    );
    assert!(ack.status.is_committed());
    harness.clock.set_boot_ns(4 * SECOND);
    let total = harness
        .query(&key, (0, 10 * SECOND), ONE_CELL_MS, HistoryStat::Total, 10)
        .unwrap();
    assert_well_formed(&total);
    assert_eq!(total.points.len(), 1);
    assert_eq!(value_u64(&total.points[0]), 300);
    assert_eq!(total.points[0].method, HistoryMethod::CounterDelta);
    assert_eq!(total.covered_ns, 3 * SECOND);
    assert_eq!(total.coverage_ratio, 0.3);
    let native = harness
        .query(&key, (0, 10 * SECOND), 0, HistoryStat::Total, 10)
        .unwrap();
    assert_eq!(
        (native.points[0].start_boot_ns, native.points[0].end_boot_ns),
        (0, 3 * SECOND)
    );
    assert_eq!(native.points[0].value, Some(Scalar::from_u64(300)));
    let rate = harness
        .query(&key, (3 * SECOND, 4 * SECOND), 0, HistoryStat::Mean, 10)
        .unwrap();
    assert_eq!(value_f64(&rate.points[0]), 100.0);
}

#[test]
fn at07_changing_the_sampling_interval_keeps_old_totals() {
    let mut harness = Harness::new();
    let key = rx_key("netif:ns:2");
    for second in 1..=10u64 {
        harness.commit(
            vec![gauge(&key, 100.0, second * SECOND)],
            vec![interval(&key, (second - 1) * SECOND, second * SECOND, 100)],
            second * SECOND,
        );
    }
    let before = harness
        .query(&key, (0, 10 * SECOND), ONE_CELL_MS, HistoryStat::Total, 10)
        .unwrap();
    assert_eq!(value_u64(&before.points[0]), 1000);
    let mut slower = config().sampling;
    slower.interval_ms = 5000;
    harness.writer.set_sampling(&slower);
    for (start, end) in [(10, 15), (15, 20)] {
        harness.commit(
            vec![gauge(&key, 100.0, end * SECOND)],
            vec![interval(&key, start * SECOND, end * SECOND, 500)],
            end * SECOND,
        );
    }
    let after = harness
        .query(&key, (0, 10 * SECOND), ONE_CELL_MS, HistoryStat::Total, 10)
        .unwrap();
    assert_eq!(after.points, before.points);
    assert_eq!(after.covered_ns, before.covered_ns);
    let all = harness
        .query(&key, (0, 20 * SECOND), ONE_CELL_MS, HistoryStat::Total, 10)
        .unwrap();
    assert_eq!(value_u64(&all.points[0]), 2000);
    assert_eq!(all.coverage_ratio, 1.0);
}

#[test]
fn at07_coverage_is_exact_and_gaps_are_never_zero_filled() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    for second in 0..10u64 {
        harness.commit(
            vec![gauge(
                &key,
                50.0 + f64::from(u32::try_from(second).unwrap()),
                second * SECOND,
            )],
            vec![],
            second * SECOND,
        );
    }
    harness.clock.set_boot_ns(30 * SECOND);
    let native = harness
        .query(&key, (0, 20 * SECOND), 0, HistoryStat::Mean, 100)
        .unwrap();
    assert_well_formed(&native);
    // Steps of 1 s, the last one held until its stale deadline (3 s).
    assert_eq!(native.covered_ns, 12 * SECOND);
    assert_eq!(native.coverage_ratio, 0.6);
    let gap = native.points.last().unwrap();
    assert_eq!(
        (gap.start_boot_ns, gap.end_boot_ns),
        (12 * SECOND, 20 * SECOND)
    );
    assert!(gap.value.is_none());
    assert_eq!(gap.method, HistoryMethod::Gap);
    assert_eq!(gap.reason.as_deref(), Some("stale"));

    let one = harness
        .query(&key, (0, 20 * SECOND), 20_000, HistoryStat::Mean, 10)
        .unwrap();
    let expected = ((50..=58).sum::<u32>() as f64 + 59.0 * 3.0) / 12.0;
    assert_eq!(one.points.len(), 1);
    assert!((value_f64(&one.points[0]) - expected).abs() < 1e-12);
    assert_eq!(one.points[0].covered_ns, 12 * SECOND);
    let min = harness
        .query(&key, (0, 20 * SECOND), 20_000, HistoryStat::Min, 10)
        .unwrap();
    assert_eq!(
        value_f64(&min.points[0]),
        50.0,
        "missing time is not a zero"
    );
    let per_second = harness
        .query(&key, (0, 20 * SECOND), 1000, HistoryStat::Max, 100)
        .unwrap();
    assert_well_formed(&per_second);
    assert_eq!(
        per_second
            .points
            .iter()
            .filter(|point| point.value.is_some())
            .count(),
        12
    );
    assert!(
        per_second
            .points
            .iter()
            .filter_map(|point| point.value)
            .all(|value| value.to_f64_lossy() >= 50.0)
    );
}

#[test]
fn at07_open_step_is_not_extended_into_the_future() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    harness.commit(vec![gauge(&key, 70.0, 10 * SECOND)], vec![], 10 * SECOND);
    harness.clock.set_boot_ns(11 * SECOND);
    let result = harness
        .query(&key, (0, 20 * SECOND), 0, HistoryStat::Mean, 10)
        .unwrap();
    assert_eq!(result.covered_ns, SECOND);
}

#[test]
fn at07_two_hour_gap_does_not_leak_into_the_last_hour() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    let start = 10 * MINUTE;
    for second in 0..600u64 {
        let at = start + second * SECOND;
        harness.commit(vec![gauge(&key, 90.0, at)], vec![], at);
    }
    let resume = start + 10 * MINUTE + 2 * 60 * MINUTE;
    let mut tick = start + 10 * MINUTE;
    while tick < resume {
        harness.maintain(tick);
        tick += 5 * MINUTE;
    }
    for second in 0..300u64 {
        let at = resume + second * SECOND;
        harness.commit(vec![gauge(&key, 40.0, at)], vec![], at);
        if second % 60 == 0 {
            harness.maintain(at);
        }
    }
    let now = resume + 300 * SECOND;
    harness.maintain(now);
    for resolution in [1000, 60_000, ONE_CELL_MS] {
        let last_hour = harness
            .query(
                &key,
                (now - 60 * MINUTE, now),
                resolution,
                HistoryStat::Max,
                2000,
            )
            .unwrap();
        assert_well_formed(&last_hour);
        assert_eq!(
            last_hour.covered_ns,
            300 * SECOND,
            "resolution {resolution}"
        );
        assert!(
            last_hour
                .points
                .iter()
                .filter_map(|point| point.value)
                .all(|value| value.to_f64_lossy() == 40.0)
        );
    }
    let mean = harness
        .query(
            &key,
            (now - 60 * MINUTE, now),
            ONE_CELL_MS,
            HistoryStat::Mean,
            10,
        )
        .unwrap();
    let value = mean.points.iter().find_map(|point| point.value).unwrap();
    assert_eq!(value.to_f64_lossy(), 40.0);
    // The old samples are still answered from minute rollups.
    let old = harness
        .query(
            &key,
            (start, start + 10 * MINUTE),
            60_000,
            HistoryStat::Mean,
            100,
        )
        .unwrap();
    assert_eq!(old.covered_ns, 10 * MINUTE);
    assert!(
        old.points
            .iter()
            .all(|point| point.method == HistoryMethod::TimeWeightedStep)
    );
}

#[test]
fn at07_other_boots_are_separate_series_and_unknown_boots_are_empty() {
    let mut harness = Harness::new();
    let key_a = temperature_key("gpu:0");
    let key_b = with_boot(&key_a, "boot-b");
    for second in 0..10u64 {
        harness.commit(
            vec![
                gauge(&key_a, 10.0, second * SECOND),
                gauge(&key_b, 20.0, second * SECOND),
            ],
            vec![],
            second * SECOND,
        );
    }
    harness.clock.set_boot_ns(20 * SECOND);
    let other = harness
        .query(&key_b, (0, 20 * SECOND), ONE_CELL_MS, HistoryStat::Mean, 10)
        .unwrap();
    assert_eq!(value_f64(&other.points[0]), 20.0);
    let unknown = harness
        .query(
            &with_boot(&key_a, "boot-c"),
            (0, 20 * SECOND),
            ONE_CELL_MS,
            HistoryStat::Mean,
            10,
        )
        .unwrap();
    assert!(unknown.points.is_empty());
    assert_eq!(unknown.covered_ns, 0);
    assert_eq!(unknown.coverage_ratio, 0.0);
    assert_eq!(
        unknown.snapshot_revision,
        harness.snapshots.latest().revision
    );
}

#[test]
fn at07_generation_changes_cut_segments_and_utc_steps_are_discontinuities() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    harness
        .writer
        .observe_wall_clock(0, 1_790_000_000_000_000_000);
    for second in 0..10u64 {
        let generation = if second < 5 { 1 } else { 2 };
        harness.commit(
            vec![gauge_from(
                &key,
                30.0,
                second * SECOND,
                "nvml:0:temp",
                generation,
            )],
            vec![],
            second * SECOND,
        );
        if second == 5 {
            harness.writer.observe_wall_clock(
                5 * SECOND,
                1_790_000_005_000_000_000 - 86_400 * SECOND as i128,
            );
        }
    }
    harness.clock.set_boot_ns(20 * SECOND);
    let result = harness
        .query(&key, (0, 20 * SECOND), 0, HistoryStat::Mean, 100)
        .unwrap();
    assert_eq!(result.source_changes, vec![5 * SECOND]);
    assert_eq!(result.clock_discontinuities, vec![5 * SECOND]);
    assert_eq!(
        result.covered_ns,
        12 * SECOND,
        "the UTC step does not change boot-time coverage"
    );
}

/// Intervals `[k + 0.5 s, k + 1.5 s)` of `k + 1` bytes for k in 0..count,
/// with raw history kept only 30 s so a later maintenance evicts it.
fn bridged_counters(count: u64) -> (Harness, dgxtop_core::identity::MetricKey) {
    let mut settings = config();
    settings.store.raw_history_seconds = 30;
    let mut harness = Harness::with_config(settings);
    let key = rx_key("netif:ns:2");
    for k in 0..count {
        let start = k * SECOND + 500 * MILLI;
        let end = start + SECOND;
        harness.commit(
            vec![gauge(&key, (k + 1) as f64, end)],
            vec![interval(&key, start, end, k + 1)],
            end,
        );
    }
    (harness, key)
}

#[test]
fn at27_cross_minute_interval_is_a_bridge_counted_exactly_once() {
    let (mut harness, key) = bridged_counters(120);
    let expected: u64 = (1..=120).sum();
    let window = (500 * MILLI, 120 * SECOND + 500 * MILLI);
    let raw_total = harness
        .query(&key, window, ONE_CELL_MS, HistoryStat::Total, 10)
        .unwrap();
    assert_eq!(value_u64(&raw_total.points[0]), expected);
    assert_eq!(raw_total.covered_ns, 120 * SECOND);

    harness.maintain(300 * SECOND);
    let raw = harness
        .query(&key, (0, 180 * SECOND), 0, HistoryStat::Raw, 100)
        .unwrap();
    assert_eq!(
        raw.points[0].reason.as_deref(),
        Some("raw_history_expired"),
        "raw intervals were evicted"
    );
    let rollup_total = harness
        .query(&key, (0, 180 * SECOND), ONE_CELL_MS, HistoryStat::Total, 10)
        .unwrap();
    assert_well_formed(&rollup_total);
    assert_eq!(
        value_u64(&rollup_total.points[0]),
        expected,
        "buckets plus bridges, each interval once"
    );
    assert_eq!(rollup_total.points[0].method, HistoryMethod::BucketExact);
    assert_eq!(rollup_total.covered_ns, 120 * SECOND);

    let per_minute = harness
        .query(&key, (0, 180 * SECOND), 60_000, HistoryStat::Total, 10)
        .unwrap();
    assert_well_formed(&per_minute);
    let values: Vec<Option<u64>> = per_minute
        .points
        .iter()
        .map(|point| point.value.and_then(Scalar::as_u64))
        .collect();
    // Minute points hold only intervals inside them; the two bridges cross
    // minute boundaries and are counted in none of them, never split.
    assert_eq!(
        values,
        vec![Some((1..=59).sum()), Some((61..=119).sum()), None]
    );
    assert_eq!(per_minute.covered_ns, 118 * SECOND);
}

#[test]
fn at27_boundary_without_raw_is_a_boundary_gap_never_proportional() {
    let (mut harness, key) = bridged_counters(120);
    harness.maintain(300 * SECOND);
    let result = harness
        .query(
            &key,
            (30 * SECOND, 180 * SECOND),
            ONE_CELL_MS,
            HistoryStat::Total,
            10,
        )
        .unwrap();
    assert_well_formed(&result);
    let boundary = &result.points[0];
    assert_eq!(boundary.method, HistoryMethod::BoundaryGap);
    assert!(boundary.value.is_none());
    assert_eq!(
        (boundary.start_boot_ns, boundary.end_boot_ns),
        (30 * SECOND, 59 * SECOND + 500 * MILLI)
    );
    let data = &result.points[1];
    // Bridge across 60 s (60) + whole minute 60..120 s + bridge across 120 s (120).
    assert_eq!(value_u64(data), 60 + (61..=119).sum::<u64>() + 120);
    assert_eq!(result.covered_ns, 61 * SECOND);
    assert!(result.coverage_ratio < 1.0);

    // While raw data still exists the same edge is exact.
    let (fresh, fresh_key) = bridged_counters(120);
    let exact = fresh
        .query(
            &fresh_key,
            (30 * SECOND + 500 * MILLI, 120 * SECOND + 500 * MILLI),
            ONE_CELL_MS,
            HistoryStat::Total,
            10,
        )
        .unwrap();
    assert_eq!(value_u64(&exact.points[0]), (31..=120).sum::<u64>());
}

#[test]
fn at27_interval_ending_on_a_minute_boundary_is_counted_in_the_total() {
    let mut harness = Harness::new();
    let key = rx_key("netif:ns:2");
    let first = 58 * SECOND + 581 * MILLI;
    let boundary = MINUTE;
    let last = MINUTE + 100 * MILLI;
    harness.commit(
        vec![gauge(&key, 1.0, boundary)],
        vec![interval(&key, first, boundary, 1)],
        boundary,
    );
    harness.commit(
        vec![gauge(&key, 1.0, last)],
        vec![interval(&key, boundary, last, 0)],
        last,
    );
    let result = harness
        .query(&key, (first, last), ONE_CELL_MS, HistoryStat::Total, 10)
        .unwrap();
    assert_well_formed(&result);
    let data = result
        .points
        .iter()
        .find(|point| point.value.is_some())
        .unwrap_or_else(|| panic!("no data point in {result:?}"));
    assert_eq!(value_u64(data), 1);
    assert_eq!(result.covered_ns, last - first);
}
