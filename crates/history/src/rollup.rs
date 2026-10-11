//! Minute rollups derived from raw history.
//!
//! Buckets are computed once, when the finalization horizon has passed them,
//! from exactly the same step model the query engine uses on raw data. Counter
//! intervals fully inside one bucket go into that bucket's exact total;
//! intervals that cross a bucket boundary become bridges and are never split
//! proportionally or counted in two buckets.

use std::collections::BTreeMap;

use super::grid::MinuteGrid;
use super::records::{CounterBucket, CounterSample, GaugeBucket, GaugeSample};
use super::steps::Step;

/// Gauge buckets (one per minute of `grid` and segment) for the steps
/// clipped to `[from_ns, to_ns)`, in `(start, segment)` order. `samples`
/// must yield the records starting inside the same range.
pub(crate) fn gauge_buckets<'r>(
    steps: impl Iterator<Item = Step>,
    samples: impl Iterator<Item = &'r GaugeSample>,
    (from_ns, to_ns): (u64, u64),
    grid: MinuteGrid,
) -> Vec<GaugeBucket> {
    let mut buckets: BTreeMap<(u64, u32), GaugeBucket> = BTreeMap::new();
    for step in steps {
        let start = step.start_ns.max(from_ns);
        let end = step.end_ns.min(to_ns);
        let value = step.value.to_f64_lossy();
        let mut cursor = start;
        while cursor < end {
            let bucket_start = grid.floor(cursor);
            let bucket_end = grid.end_of(bucket_start);
            let piece_end = end.min(bucket_end);
            buckets
                .entry((bucket_start, step.segment))
                .or_insert_with(|| GaugeBucket::empty(bucket_start, bucket_end, step.segment))
                .add_step(value, piece_end - cursor);
            cursor = piece_end;
        }
    }
    for sample in samples {
        if sample.value.is_none() || sample.time_ns < from_ns || sample.time_ns >= to_ns {
            continue;
        }
        let bucket_start = grid.floor(sample.time_ns);
        let bucket = buckets
            .entry((bucket_start, sample.segment))
            .or_insert_with(|| {
                GaugeBucket::empty(bucket_start, grid.end_of(bucket_start), sample.segment)
            });
        bucket.sample_count = bucket.sample_count.saturating_add(1);
    }
    buckets.into_values().collect()
}

/// Counter rollup output for one finalization pass.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct CounterRollup {
    pub buckets: Vec<CounterBucket>,
    pub bridges: Vec<CounterSample>,
}

/// Classifies the intervals ending inside `(from_ns, to_ns]` by the
/// minutes of `grid`.
pub(crate) fn counter_rollup<'r>(
    intervals: impl Iterator<Item = &'r CounterSample>,
    (from_ns, to_ns): (u64, u64),
    grid: MinuteGrid,
) -> CounterRollup {
    let mut buckets: BTreeMap<(u64, u32), CounterBucket> = BTreeMap::new();
    let mut bridges = Vec::new();
    for interval in intervals {
        if interval.end_ns <= from_ns || interval.end_ns > to_ns {
            continue;
        }
        let bucket_start = grid.floor(interval.start_ns);
        if bucket_start == grid.floor(interval.end_ns.saturating_sub(1)) {
            buckets
                .entry((bucket_start, interval.segment))
                .or_insert_with(|| {
                    CounterBucket::empty(bucket_start, grid.end_of(bucket_start), interval.segment)
                })
                .add_interval(interval);
        } else {
            bridges.push(*interval);
        }
    }
    CounterRollup {
        buckets: buckets.into_values().collect(),
        bridges,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::clock::NANOS_PER_SECOND;
    use dgxtop_core::metric::Scalar;

    const SECOND: u64 = NANOS_PER_SECOND;

    fn step(start_s: f64, end_s: f64, value: f64, segment: u32) -> Step {
        let to_ns = |seconds: f64| {
            let nanos = seconds * 1e9;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let nanos = nanos as u64;
            nanos
        };
        Step {
            start_ns: to_ns(start_s),
            end_ns: to_ns(end_s),
            value: Scalar::from_f64(value).unwrap(),
            segment,
            ended_by_deadline: false,
        }
    }

    #[test]
    fn steps_split_at_bucket_boundaries_and_segments() {
        let steps = vec![step(50.0, 70.0, 10.0, 0), step(70.0, 80.0, 20.0, 1)];
        let samples = [
            GaugeSample {
                time_ns: 50 * SECOND,
                stale_deadline_ns: 80 * SECOND,
                value: Some(Scalar::from_f64(10.0).unwrap()),
                segment: 0,
            },
            GaugeSample {
                time_ns: 70 * SECOND,
                stale_deadline_ns: 90 * SECOND,
                value: Some(Scalar::from_f64(20.0).unwrap()),
                segment: 1,
            },
        ];
        let buckets = gauge_buckets(
            steps.into_iter(),
            samples.iter(),
            (0, 120 * SECOND),
            MinuteGrid::BOOT_CLOCK,
        );
        assert_eq!(buckets.len(), 3);
        assert_eq!((buckets[0].start_ns, buckets[0].segment), (0, 0));
        assert_eq!(buckets[0].covered_ns, 10 * SECOND);
        assert_eq!(buckets[0].sample_count, 1);
        assert_eq!((buckets[1].start_ns, buckets[1].segment), (60 * SECOND, 0));
        assert_eq!(buckets[1].covered_ns, 10 * SECOND);
        assert_eq!(buckets[1].sample_count, 0);
        assert_eq!((buckets[2].start_ns, buckets[2].segment), (60 * SECOND, 1));
        assert_eq!(buckets[2].covered_ns, 10 * SECOND);
        assert_eq!(buckets[2].sample_count, 1);
    }

    /// A boot 37 s into a UTC minute: the head minute ends at boot 23 s,
    /// and every bucket carries the end its grid gives it, so a pin or
    /// a seek by end never reads a neighbour's time as its own.
    #[test]
    fn buckets_carry_the_end_of_their_minute_on_the_grid() {
        use crate::records::TimeSpan;

        let grid = MinuteGrid::on_utc_minutes(0, 37 * i128::from(SECOND));
        let buckets = gauge_buckets(
            vec![step(0.0, 90.0, 1.0, 0)].into_iter(),
            std::iter::empty(),
            (0, 90 * SECOND),
            grid,
        );
        let spans: Vec<(u64, u64)> = buckets
            .iter()
            .map(|bucket| (bucket.start_ns, TimeSpan::end_ns(bucket)))
            .collect();
        assert_eq!(
            spans,
            vec![
                (0, 23 * SECOND),
                (23 * SECOND, 83 * SECOND),
                (83 * SECOND, 143 * SECOND)
            ]
        );
        let interval = CounterSample {
            start_ns: SECOND,
            end_ns: 2 * SECOND,
            delta: Scalar::from_u64(1),
            segment: 0,
        };
        let rollup = counter_rollup([interval].iter(), (0, 23 * SECOND), grid);
        assert_eq!(TimeSpan::end_ns(&rollup.buckets[0]), 23 * SECOND);
    }

    #[test]
    fn steps_are_clipped_to_the_finalized_range() {
        let buckets = gauge_buckets(
            vec![step(50.0, 130.0, 1.0, 0)].into_iter(),
            std::iter::empty(),
            (60 * SECOND, 120 * SECOND),
            MinuteGrid::BOOT_CLOCK,
        );
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].start_ns, 60 * SECOND);
        assert_eq!(buckets[0].covered_ns, 60 * SECOND);
    }

    #[test]
    fn crossing_interval_becomes_a_bridge_exactly_once() {
        let interval = |start_ms: u64, end_ms: u64, delta: u64| CounterSample {
            start_ns: start_ms * 1_000_000,
            end_ns: end_ms * 1_000_000,
            delta: Scalar::from_u64(delta),
            segment: 0,
        };
        let intervals = [
            interval(58_500, 59_500, 7),
            interval(59_500, 60_500, 300),
            interval(60_500, 61_500, 5),
        ];
        let rollup = counter_rollup(intervals.iter(), (0, 120 * SECOND), MinuteGrid::BOOT_CLOCK);
        assert_eq!(rollup.bridges, vec![intervals[1]]);
        assert_eq!(rollup.buckets.len(), 2);
        assert_eq!(rollup.buckets[0].total.integral(), Some(7));
        assert_eq!(rollup.buckets[1].total.integral(), Some(5));
        assert_eq!(rollup.buckets[0].covered_ns, SECOND);
        // An interval ending exactly on a boundary belongs to the bucket before.
        let edge = [interval(59_000, 60_000, 9)];
        let rollup = counter_rollup(edge.iter(), (0, 60 * SECOND), MinuteGrid::BOOT_CLOCK);
        assert!(rollup.bridges.is_empty());
        assert_eq!(rollup.buckets[0].start_ns, 0);
        // Intervals outside (from, to] are left for another pass.
        assert_eq!(
            counter_rollup(
                edge.iter(),
                (60 * SECOND, 120 * SECOND),
                MinuteGrid::BOOT_CLOCK
            ),
            CounterRollup::default()
        );
    }
}
