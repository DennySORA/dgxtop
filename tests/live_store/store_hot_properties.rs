//! Property tests: time-weighted means against a reference computation,
//! exact totals split into minutes and bridges, and finished minutes
//! agreeing with the raw steps they were cut from (read where the history
//! hands them on, D-90).

#[path = "store_hot_support.rs"]
mod support;

use dgxtop_core::process_history::Tier;
use proptest::prelude::*;
use support::{
    EVERYTHING_NS, Harness, MILLI, MINUTE, SECOND, archived_total, config, gauge, interval, rx_key,
    temperature_key, utc_of,
};
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
        clock_after_ms in 0u64..10_000,
    ) {
        let mut harness = Harness::new();
        let key = temperature_key("gpu:0");
        let samples = commit_samples(&mut harness, &key, &gaps_ms, &values);
        let last = samples.last().map_or(0, |(time, _)| *time);
        let now = last + clock_after_ms * MILLI;
        harness.maintain(now);
        // Finished minutes and the ones still filling, up to now.
        let held = harness.window(&key, EVERYTHING_NS);
        let (covered, mean) = reference(&samples, (0, now), now);
        prop_assert_eq!(held.covered_ns, covered);
        match held.mean() {
            None => prop_assert_eq!(covered, 0),
            Some(value) => prop_assert!(
                (value - mean).abs() <= 1e-9 * mean.abs().max(1.0),
                "{} vs {}", value, mean
            ),
        }
    }

    /// The minutes are disjoint windows: their exact totals and the
    /// bridges between them add up to the total of every interval, each
    /// counted once.
    #[test]
    fn totals_of_disjoint_windows_add_up_to_their_union(
        durations_ms in proptest::collection::vec(100u64..5000, 2..60),
        deltas in proptest::collection::vec(0u64..1_000_000_000_000, 60),
        offset_ms in 0u64..120_000,
    ) {
        let mut harness = Harness::archived(config());
        let key = rx_key("netif:ns:2");
        let mut end = offset_ms * MILLI;
        for (duration, delta) in durations_ms.iter().zip(&deltas) {
            let start = end;
            end = start + duration * MILLI;
            harness.commit(vec![gauge(&key, 1.0, end)], vec![interval(&key, start, end, *delta)], end);
        }
        let journal = harness.archive_through(end + 2 * MINUTE);
        let expected: u128 = deltas.iter().take(durations_ms.len()).map(|delta| u128::from(*delta)).sum();
        prop_assert_eq!(archived_total(&journal), expected);
        prop_assert!(journal.bridges.iter().all(|bridge| bridge.end_boot_ns - bridge.start_boot_ns <= 5 * SECOND));
    }

    /// A minute's bar is the same whether it is still cut from the raw
    /// steps (not finalized yet) or read from its finished minute after
    /// the raw history is gone.
    #[test]
    fn rollups_agree_with_raw_history_on_minute_windows(
        gaps_ms in proptest::collection::vec(200u64..4000, 60..160),
        values in proptest::collection::vec(0.0f64..500.0, 160),
    ) {
        let mut settings = config();
        settings.store.raw_history_seconds = 30;
        let mut harness = Harness::with_config(settings);
        let key = temperature_key("gpu:0");
        let samples = commit_samples(&mut harness, &key, &gaps_ms, &values);
        let last = samples.last().map_or(0, |(time, _)| *time);
        let span = (0, last + MINUTE);
        // Commits finalize each minute once its grace has passed; the last
        // minutes are still cut from the raw steps at this read.
        harness.clock.set_boot_ns(last + MINUTE);
        let before = harness.bars(&key, Tier::OneMinute, span);
        // Finalize every minute and evict the raw history behind them.
        harness.maintain(last + 10 * MINUTE);
        harness.clock.set_boot_ns(last + 10 * MINUTE);
        let after = harness.bars(&key, Tier::OneMinute, span);
        prop_assert_eq!(before.len(), after.len());
        for (raw, rolled) in before.iter().zip(&after) {
            prop_assert_eq!(raw.start_utc_ns, rolled.start_utc_ns);
            prop_assert_eq!(raw.covered_ns, rolled.covered_ns);
            let (Some(raw_mean), Some(rolled_mean)) = (raw.mean(), rolled.mean()) else {
                return Err(TestCaseError::fail("a bar without a value"));
            };
            prop_assert!((raw_mean - rolled_mean).abs() <= 1e-9 * raw_mean.abs().max(1.0));
        }
        for bar in &after {
            let index = u64::try_from(bar.start_utc_ns - utc_of(0)).unwrap() / MINUTE;
            let (covered, mean) = reference(&samples, (index * MINUTE, (index + 1) * MINUTE), u64::MAX);
            prop_assert_eq!(bar.covered_ns, covered);
            let value = bar.mean().unwrap_or(f64::NAN);
            prop_assert!((value - mean).abs() <= 1e-9 * mean.abs().max(1.0), "{} vs {}", value, mean);
        }
    }
}
