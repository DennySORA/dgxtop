//! AT07 (real-time history, exact counters, coverage) and AT27 (exact
//! minutes, cross-minute bridges), observed where the history hands them
//! on (D-90): the Overview's reads for gauges and the archive's finished
//! minutes and bridges for exact counter totals.

#[path = "store_hot_support.rs"]
mod support;

use dgxtop_core::ports::ArchivedBucket;
use dgxtop_core::testing::UTC_ON_A_WHOLE_MINUTE_NS;
use dgxtop_core::tiers::Tier;
use support::{
    EVERYTHING_NS, Harness, MILLI, MINUTE, SECOND, archived_total, config, gauge, gauge_from,
    interval, rx_key, temperature_key,
};

/// The counter minutes of a journal as `(start s, end s, total)`.
fn counter_minutes(buckets: &[ArchivedBucket]) -> Vec<(u64, u64, u128)> {
    buckets
        .iter()
        .filter_map(|bucket| {
            bucket.counter_delta.map(|total| {
                (
                    bucket.start_boot_ns / SECOND,
                    bucket.end_boot_ns / SECOND,
                    total,
                )
            })
        })
        .collect()
}

#[test]
fn at07_counter_interval_total_is_exact() {
    let mut harness = Harness::archived(config());
    let key = rx_key("netif:ns:2");
    // 1000 -> 1300 bytes over 3 s: the collector derived 100 B/s and the
    // exact 300 byte interval.
    let ack = harness.commit(
        vec![gauge(&key, 100.0, 3 * SECOND)],
        vec![interval(&key, 0, 3 * SECOND, 300)],
        3 * SECOND,
    );
    assert!(ack.status.is_committed());
    let journal = harness.archive_through(2 * MINUTE);
    let counted: Vec<&ArchivedBucket> = journal
        .buckets
        .iter()
        .filter(|bucket| bucket.counter_delta.is_some())
        .collect();
    assert_eq!(counted.len(), 1);
    assert_eq!(counted[0].counter_delta, Some(300));
    assert_eq!(counted[0].covered_ns, 3 * SECOND);
    assert_eq!(archived_total(&journal), 300);
    let rate = harness.window(&key, EVERYTHING_NS);
    assert_eq!(rate.mean(), Some(100.0));
}

#[test]
fn at07_changing_the_sampling_interval_keeps_old_totals() {
    let mut harness = Harness::archived(config());
    let key = rx_key("netif:ns:2");
    for second in 1..=60u64 {
        harness.commit(
            vec![gauge(&key, 100.0, second * SECOND)],
            vec![interval(&key, (second - 1) * SECOND, second * SECOND, 100)],
            second * SECOND,
        );
    }
    let before = harness.archive_through(MINUTE + 30 * SECOND);
    assert_eq!(counter_minutes(&before.buckets), vec![(0, 60, 6000)]);
    let mut slower = config().sampling;
    slower.interval_ms = 5000;
    harness.writer.set_sampling(&slower);
    for start in (120..180u64).step_by(5) {
        harness.commit(
            vec![gauge(&key, 100.0, (start + 5) * SECOND)],
            vec![interval(&key, start * SECOND, (start + 5) * SECOND, 500)],
            (start + 5) * SECOND,
        );
    }
    let after = harness.archive_through(3 * MINUTE + 30 * SECOND);
    assert_eq!(
        counter_minutes(&after.buckets),
        vec![(120, 180, 6000)],
        "the finished minute is never written again"
    );
    assert_eq!(archived_total(&before) + archived_total(&after), 12_000);
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
    // Steps of 1 s, the last one held until its stale deadline (3 s).
    let held = harness.window(&key, EVERYTHING_NS);
    assert_eq!(held.covered_ns, 12 * SECOND);
    let expected = ((50..=58).sum::<u32>() as f64 + 59.0 * 3.0) / 12.0;
    assert!((held.mean().unwrap() - expected).abs() < 1e-12);
    assert_eq!(held.minimum, 50.0, "missing time is not a zero");
    assert_eq!(held.maximum, 59.0);
    let bars = harness.bars(&key, Tier::OneMinute, (0, MINUTE));
    assert_eq!(bars.len(), 1);
    assert_eq!(
        bars[0].covered_ns,
        12 * SECOND,
        "the minute's bar holds only the time it has"
    );
    assert!((bars[0].mean().unwrap() - expected).abs() < 1e-12);
}

#[test]
fn at07_open_step_is_not_extended_into_the_future() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    harness.commit(vec![gauge(&key, 70.0, 10 * SECOND)], vec![], 10 * SECOND);
    harness.clock.set_boot_ns(11 * SECOND);
    assert_eq!(harness.window(&key, EVERYTHING_NS).covered_ns, SECOND);
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
    let last_hour = harness.window(&key, 60 * MINUTE);
    assert_eq!(last_hour.covered_ns, 300 * SECOND);
    assert_eq!((last_hour.minimum, last_hour.maximum), (40.0, 40.0));
    assert_eq!(last_hour.mean(), Some(40.0));
    let minutes = harness.bars(&key, Tier::OneMinute, (now - 60 * MINUTE, now));
    assert_eq!(minutes.len(), 5, "only the minutes since the gap");
    assert!(minutes.iter().all(|bar| bar.maximum == 40.0));
    // The old samples are still answered from their finished minutes.
    let old = harness.bars(&key, Tier::OneMinute, (start, start + 11 * MINUTE));
    let old_covered: u64 = old.iter().map(|bar| bar.covered_ns).sum();
    assert_eq!(old_covered, 10 * MINUTE + 2 * SECOND);
    assert!(old.iter().all(|bar| bar.mean() == Some(90.0)));
}

#[test]
fn at07_generation_changes_cut_segments_and_utc_steps_are_discontinuities() {
    let mut harness = Harness::archived(config());
    let key = temperature_key("gpu:0");
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
                UTC_ON_A_WHOLE_MINUTE_NS + 5 * SECOND as i128 - 86_400 * SECOND as i128,
            );
        }
    }
    let journal = harness.archive_through(2 * MINUTE);
    let generations: Vec<u32> = journal
        .segments
        .iter()
        .map(|segment| segment.source_generation)
        .collect();
    assert_eq!(
        generations,
        vec![1, 2],
        "the generation change cuts a segment"
    );
    let minute: Vec<&ArchivedBucket> = journal
        .buckets
        .iter()
        .filter(|bucket| bucket.start_boot_ns == 0)
        .collect();
    assert_eq!(minute.len(), 2, "one row of the minute per segment");
    assert_eq!(
        minute.iter().map(|bucket| bucket.covered_ns).sum::<u64>(),
        12 * SECOND,
        "the UTC step does not change boot-time coverage"
    );
}

/// Intervals `[k + 0.5 s, k + 1.5 s)` of `k + 1` bytes for k in 0..count,
/// with raw history kept only 30 s so a later maintenance evicts it.
fn bridged_counters(count: u64) -> (Harness, dgxtop_core::identity::MetricKey) {
    let mut settings = config();
    settings.store.raw_history_seconds = 30;
    let mut harness = Harness::archived(settings);
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
    let (mut harness, _key) = bridged_counters(120);
    let journal = harness.archive_through(300 * SECOND);
    // Minutes hold only intervals inside them; the intervals crossing a
    // minute boundary are bridges, counted in no minute, never split.
    assert_eq!(
        counter_minutes(&journal.buckets),
        vec![(0, 60, (1..=59).sum()), (60, 120, (61..=119).sum())]
    );
    let bridges: Vec<(u64, u128)> = journal
        .bridges
        .iter()
        .map(|bridge| (bridge.start_boot_ns, bridge.delta))
        .collect();
    assert_eq!(
        bridges,
        vec![
            (59 * SECOND + 500 * MILLI, 60),
            (119 * SECOND + 500 * MILLI, 120)
        ]
    );
    assert_eq!(
        archived_total(&journal),
        (1..=120).sum::<u128>(),
        "buckets plus bridges, each interval once"
    );
}

/// D-82: a boot 37 s into a UTC minute cuts its minutes at boot 23 s,
/// 83 s, 143 s… (whole UTC minutes), the first 23 s being a head minute.
/// The finished minutes stay exact: each holds only the intervals inside
/// it, the intervals across a boundary are bridges, each counted once.
#[test]
fn at27_minutes_on_whole_utc_minutes_keep_every_window_exact() {
    let mut settings = config();
    settings.store.raw_history_seconds = 30;
    let mut harness = Harness::unpaired(
        settings,
        dgxtop_core::budget::ByteLedger::for_memory_limit_mib(256),
        dgxtop_core::quality::Namespace::Live,
        true,
    );
    let key = rx_key("netif:ns:2");
    for k in 0..200u64 {
        let start = k * SECOND + 500 * MILLI;
        let end = start + SECOND;
        harness.commit(
            vec![gauge(&key, (k + 1) as f64, end)],
            vec![interval(&key, start, end, k + 1)],
            end,
        );
    }
    // The pairing arrives late; nothing was finished before it.
    harness.writer.observe_wall_clock(
        250 * SECOND,
        UTC_ON_A_WHOLE_MINUTE_NS + i128::from(37 * SECOND + 250 * SECOND),
    );
    let journal = harness.archive_through(400 * SECOND);
    assert_eq!(
        counter_minutes(&journal.buckets),
        vec![
            (0, 23, (1..=22).sum()),
            (23, 83, (24..=82).sum()),
            (83, 143, (84..=142).sum()),
            (143, 203, (144..=200).sum()),
        ]
    );
    let bridges: Vec<u128> = journal.bridges.iter().map(|bridge| bridge.delta).collect();
    assert_eq!(bridges, vec![23, 83, 143]);
    assert_eq!(archived_total(&journal), (1..=200).sum::<u128>());
    let maxima: Vec<(u64, f64)> = journal
        .buckets
        .iter()
        .filter_map(|bucket| Some((bucket.start_boot_ns / SECOND, bucket.maximum?)))
        .collect();
    // The last value (at 200.5 s) holds until its stale deadline, 203.5 s.
    assert_eq!(
        maxima,
        vec![
            (0, 22.0),
            (23, 82.0),
            (83, 142.0),
            (143, 200.0),
            (203, 200.0)
        ]
    );
    assert!(
        journal
            .buckets
            .iter()
            .filter(|bucket| bucket.start_boot_ns > 0)
            .all(|bucket| bucket.start_utc.ends_with(":00.000000000Z")),
        "minutes after the head minute start on whole UTC minutes"
    );
}

#[test]
fn at27_interval_ending_on_a_minute_boundary_is_counted_in_the_total() {
    let mut harness = Harness::archived(config());
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
    let journal = harness.archive_through(3 * MINUTE);
    assert_eq!(
        counter_minutes(&journal.buckets),
        vec![(0, 60, 1), (60, 120, 0)]
    );
    assert!(journal.bridges.is_empty());
    assert_eq!(archived_total(&journal), 1);
}
