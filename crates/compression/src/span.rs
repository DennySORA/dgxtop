//! A span of a series as one bucket (D-82): the merge of the aligned
//! buckets that cover it, the coarsest that fit for its old part and finer
//! ones toward its end, down to what is still being filled. Never a sliding
//! average: every piece is a fixed bucket of the local clock, so a window's
//! mean, peak and energy are exact merges of what the sizes hold, and stay
//! right after the finer sizes (or the archive's minutes) let old time go.

use dgxtop_core::process_history::Tier;

use crate::bars::step_of;
use crate::bucket::Bucket;
use crate::series::TieredSeries;

/// `[start, end)` of `series` as one bucket starting at `start`. Each step
/// takes the coarsest size whose bucket starts there, ends by `end` and is
/// still held; time no size holds at a minute's start is skipped to the
/// next minute (a window's start before the finest size's reach is cut to
/// the next boundary a coarser size holds).
pub fn merged_between<B: Bucket>(series: &TieredSeries<B>, (start, end): (i64, i64)) -> B {
    let shift = series.shift_ns();
    let mut merged = B::new(start);
    let mut cursor = start;
    while cursor < end {
        let fitting = Tier::ALL.into_iter().rev().find(|tier| {
            tier.align(cursor, shift) == cursor
                && cursor.saturating_add(step_of(*tier)) <= end
                && series.holds(*tier, cursor)
        });
        let Some(tier) = fitting else {
            let minute = Tier::OneMinute;
            cursor = minute.align(cursor, shift).saturating_add(step_of(minute));
            continue;
        };
        take_bucket_at(series, tier, cursor, &mut merged);
        cursor = cursor.saturating_add(step_of(tier));
    }
    merged
}

/// Merges `tier`'s bucket at `start` with what its finer sizes still fill
/// of that time (not merged into it yet), each piece once.
fn take_bucket_at<B: Bucket>(series: &TieredSeries<B>, tier: Tier, start: i64, merged: &mut B) {
    if let Some(kept) = series.kept_at(tier, start) {
        merged.merge(kept);
    }
    if let Some(filling) = series
        .filling_of(tier)
        .filter(|filling| filling.start_utc_ns() == start)
    {
        merged.merge(filling);
    }
    let shift = series.shift_ns();
    for unmerged in series
        .unmerged(tier)
        .filter(|unmerged| tier.align(unmerged.start_utc_ns(), shift) == start)
    {
        merged.merge(unmerged);
    }
}

#[cfg(test)]
mod tests {
    use dgxtop_core::overview_history::OverviewBucket;

    use super::*;

    const MINUTE: i64 = 60_000_000_000;
    const HOUR: i64 = 60 * MINUTE;
    /// Midnight UTC.
    const MIDNIGHT: i64 = 1_790_208_000 * 1_000_000_000;

    #[allow(clippy::cast_precision_loss)]
    fn value_at(minute: i64) -> f64 {
        (minute % 97) as f64
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    fn minute_bucket(minute: i64) -> OverviewBucket {
        let value = value_at(minute);
        let covered = 60_000_000_000_u64;
        OverviewBucket::gauge(
            MIDNIGHT + minute * MINUTE,
            covered,
            value * covered as f64,
            (value, value),
            12,
        )
    }

    /// `minutes` minutes from midnight, every one held.
    fn fed(minutes: i64, shift: i64) -> TieredSeries<OverviewBucket> {
        let mut series = TieredSeries::new(shift);
        for minute in 0..minutes {
            series.add_bucket(Tier::OneMinute, &minute_bucket(minute));
        }
        series
    }

    /// Every minute between `from` and `to` (minutes since midnight).
    fn brute_force(from: i64, to: i64) -> OverviewBucket {
        let mut all = OverviewBucket::new(MIDNIGHT + from * MINUTE);
        for minute in from..to {
            all.merge(&minute_bucket(minute));
        }
        all
    }

    fn assert_same(merged: &OverviewBucket, expected: &OverviewBucket) {
        assert_eq!(merged.covered_ns, expected.covered_ns);
        assert_eq!(merged.samples, expected.samples);
        assert_eq!(merged.peak(), expected.peak());
        assert_eq!(merged.low(), expected.low());
        let (mean, wanted) = (merged.mean().unwrap(), expected.mean().unwrap());
        assert!((mean - wanted).abs() < 1e-9, "{mean} vs {wanted}");
    }

    /// The last 24 hours of three days: the old part from coarse buckets,
    /// the end from the finest, the minute being filled included; equal to
    /// merging every minute although the minutes keep only five hours.
    #[test]
    fn a_window_is_the_exact_merge_of_the_sizes_that_cover_it() {
        let minutes = 3 * 24 * 60 + 37;
        let series = fed(minutes, 0);
        assert!(
            series.held_from(Tier::OneMinute) > MIDNIGHT + (minutes - 24 * 60) * MINUTE,
            "the minutes no longer reach a day back"
        );
        let end = MIDNIGHT + minutes * MINUTE;
        // A day back, on a five-minute boundary the five minutes still hold.
        let start = end - 24 * HOUR - 2 * MINUTE;
        let merged = merged_between(&series, (start, end));
        assert_eq!(merged.start_utc_ns, start);
        let from = (start - MIDNIGHT) / MINUTE;
        assert_same(&merged, &brute_force(from, minutes));
    }

    /// A start no size holds at its minute is cut to the next boundary one
    /// does; a window past the data holds only the data.
    #[test]
    fn time_no_size_holds_is_left_out() {
        let minutes = 2 * 24 * 60;
        let series = fed(minutes, 0);
        let end = MIDNIGHT + minutes * MINUTE;
        let start = end - 24 * HOUR - 3 * MINUTE;
        let merged = merged_between(&series, (start, end));
        let from = (start - MIDNIGHT) / MINUTE;
        // The five minutes reach back to minutes - 1500; the start's
        // minute is not on their boundary, so the window begins at the
        // next one.
        let next_five = (from + 4) / 5 * 5;
        assert_same(&merged, &brute_force(next_five, minutes));
        let wide = merged_between(&series, (MIDNIGHT - 10 * HOUR, end + HOUR));
        assert_eq!(wide.covered_ns, brute_force(0, minutes).covered_ns);
        let empty = merged_between(&TieredSeries::<OverviewBucket>::new(0), (0, HOUR));
        assert!(empty.is_empty());
    }

    /// Windows follow the local clock: at UTC+8 the hour buckets start on
    /// local hours, and a window of local hours is merged from them.
    #[test]
    fn windows_on_the_local_clock_merge_local_buckets() {
        let shift = 8 * HOUR + 30 * MINUTE;
        let minutes = 10 * 60;
        let series = fed(minutes, shift);
        let local_hour = Tier::OneHour.align(MIDNIGHT + 4 * HOUR, shift);
        let merged = merged_between(&series, (local_hour, local_hour + 2 * HOUR));
        let from = (local_hour - MIDNIGHT) / MINUTE;
        assert_same(&merged, &brute_force(from, from + 120));
    }
}
