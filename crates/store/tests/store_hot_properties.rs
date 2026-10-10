//! Property tests: time-weighted means against a reference computation,
//! additivity of exact totals, and rollups agreeing with raw history.

#[path = "store_hot_support.rs"]
mod support;

use dgxtop_core::history::HistoryStat;
use proptest::prelude::*;
use support::{
    Harness, MILLI, MINUTE, SECOND, config, gauge, interval, rx_key, temperature_key, value_u64,
};

/// One cell spanning any test window.
const ONE_CELL_MS: u32 = 86_400_000;
/// Stale deadline of the default configuration: 3 x 1 s.
const STALE_NS: u64 = 3 * SECOND;

/// Reference time-weighted mean over `[start, end)` of samples `(t, v)`.
#[allow(clippy::cast_precision_loss)]
fn reference(samples: &[(u64, f64)], (start, end): (u64, u64), now: u64) -> (u64, f64) {
    let mut covered = 0u64;
    let mut weighted = 0.0;
    for (index, (time, value)) in samples.iter().enumerate() {
        let next = samples.get(index + 1).map_or(u64::MAX, |(next, _)| *next);
        let step_end = next.min(time + STALE_NS).min(now).min(end);
        let step_start = (*time).max(start);
        if step_end > step_start {
            covered += step_end - step_start;
            weighted += value * (step_end - step_start) as f64;
        }
    }
    (
        covered,
        if covered == 0 {
            0.0
        } else {
            weighted / covered as f64
        },
    )
}

fn commit_samples(
    harness: &mut Harness,
    key: &dgxtop_core::identity::MetricKey,
    gaps_ms: &[u64],
    values: &[f64],
) -> Vec<(u64, f64)> {
    let mut time = 0u64;
    let mut samples = Vec::new();
    for (gap, value) in gaps_ms.iter().zip(values) {
        time += gap * MILLI;
        harness.commit(vec![gauge(key, *value, time)], vec![], time);
        samples.push((time, *value));
    }
    samples
}

proptest! {
    #[test]
    fn gauge_time_weighted_mean_matches_the_reference(
        gaps_ms in proptest::collection::vec(100u64..8000, 1..40),
        values in proptest::collection::vec(-1000.0f64..1000.0, 40),
        window in (0u64..400_000, 1u64..400_000),
        clock_after_ms in 0u64..10_000,
    ) {
        let mut harness = Harness::new();
        let key = temperature_key("gpu:0");
        let samples = commit_samples(&mut harness, &key, &gaps_ms, &values);
        let last = samples.last().map_or(0, |(time, _)| *time);
        let now = last + clock_after_ms * MILLI;
        harness.clock.set_boot_ns(now);
        let start = window.0 * MILLI;
        let end = start + window.1 * MILLI;
        let result = harness.query(&key, (start, end), ONE_CELL_MS, HistoryStat::Mean, 10).unwrap();
        let (covered, mean) = reference(&samples, (start, end), now);
        prop_assert_eq!(result.covered_ns, covered);
        let values: Vec<f64> = result.points.iter().filter_map(|point| point.value).map(|value| value.to_f64_lossy()).collect();
        if covered == 0 {
            prop_assert!(values.is_empty());
        } else {
            prop_assert_eq!(values.len(), 1);
            prop_assert!((values[0] - mean).abs() <= 1e-9 * mean.abs().max(1.0), "{} vs {}", values[0], mean);
        }
    }

    #[test]
    fn totals_of_disjoint_windows_add_up_to_their_union(
        durations_ms in proptest::collection::vec(100u64..5000, 2..60),
        deltas in proptest::collection::vec(0u64..1_000_000_000_000, 60),
        split in 1usize..59,
        offset_ms in 0u64..120_000,
    ) {
        let mut harness = Harness::new();
        let key = rx_key("netif:ns:2");
        let mut boundaries = vec![offset_ms * MILLI];
        for (duration, delta) in durations_ms.iter().zip(&deltas) {
            let start = *boundaries.last().unwrap();
            let end = start + duration * MILLI;
            harness.commit(vec![gauge(&key, 1.0, end)], vec![interval(&key, start, end, *delta)], end);
            boundaries.push(end);
        }
        let first = boundaries[0];
        let last = *boundaries.last().unwrap();
        let middle = boundaries[split.min(boundaries.len() - 2).max(1)];
        let total = |window: (u64, u64)| {
            let result = harness.query(&key, window, ONE_CELL_MS, HistoryStat::Total, 10).unwrap();
            result.points.iter().find(|point| point.value.is_some()).map_or(0, value_u64)
        };
        let expected: u64 = deltas.iter().take(durations_ms.len()).sum();
        prop_assert_eq!(total((first, last)), expected);
        prop_assert_eq!(total((first, middle)) + total((middle, last)), expected);
    }

    #[test]
    fn rollups_agree_with_raw_history_on_minute_windows(
        gaps_ms in proptest::collection::vec(200u64..4000, 60..160),
        values in proptest::collection::vec(0.0f64..500.0, 160),
        first_minute in 0u64..2,
        minutes in 1u64..3,
    ) {
        let mut settings = config();
        settings.store.raw_history_seconds = 30;
        let mut harness = Harness::with_config(settings);
        let key = temperature_key("gpu:0");
        let samples = commit_samples(&mut harness, &key, &gaps_ms, &values);
        let last = samples.last().map_or(0, |(time, _)| *time);
        let window = (first_minute * MINUTE, (first_minute + minutes) * MINUTE);
        harness.clock.set_boot_ns(last + MINUTE);
        let before = harness.query(&key, window, ONE_CELL_MS, HistoryStat::Mean, 10).unwrap();
        // Evict raw history: only finalized minute rollups remain.
        harness.maintain(last + 10 * MINUTE);
        harness.clock.set_boot_ns(last + 10 * MINUTE);
        let after = harness.query(&key, window, ONE_CELL_MS, HistoryStat::Mean, 10).unwrap();
        let (covered, mean) = reference(&samples, window, u64::MAX);
        prop_assert_eq!(before.covered_ns, covered);
        prop_assert_eq!(after.covered_ns, covered);
        let value = |result: &dgxtop_core::history::HistoryResult| {
            result.points.iter().find_map(|point| point.value).map(|value| value.to_f64_lossy())
        };
        if covered > 0 {
            let (Some(raw), Some(rolled)) = (value(&before), value(&after)) else {
                return Err(TestCaseError::fail("missing value"));
            };
            prop_assert!((raw - mean).abs() <= 1e-9 * mean.abs().max(1.0));
            prop_assert!((rolled - mean).abs() <= 1e-9 * mean.abs().max(1.0));
        }
    }
}
