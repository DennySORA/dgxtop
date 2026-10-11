//! A window of a series cut into even cells (D-97): each cell the merge of
//! the fixed, local-clock-aligned buckets that cover it, so its mean is an
//! absolute one over the time it holds, never a sliding average. A cell on
//! the boundaries of a size (a chart's bar) is the exact merge of what the
//! sizes hold; a cell edge inside a bucket takes the share of that bucket
//! that falls in the cell ([`Divisible`]), from the finest size still
//! holding that time. Old time therefore comes from coarser buckets: each
//! size keeps about a chart's width of them (`TIER_BUCKETS`).

use dgxtop_core::tiers::Tier;

use crate::bucket::{Bucket, Divisible};
use crate::series::TieredSeries;
use crate::span::{take_bucket_at, whole_fit};

/// `[start, end)` of `series` in `cells` even cells, oldest first, each a
/// bucket starting at its cell's start; a cell no size holds anything of is
/// an empty bucket (a gap, never a zero).
pub fn cells_between<B: Divisible>(
    series: &TieredSeries<B>,
    (start, end): (i64, i64),
    cells: usize,
) -> Vec<B> {
    cell_edges((start, end), cells)
        .map(|(from, to)| cell_of(series, from, to))
        .collect()
}

/// The edges of `cells` even cells of `[start, end)`.
fn cell_edges((start, end): (i64, i64), cells: usize) -> impl Iterator<Item = (i64, i64)> {
    let span = i128::from(end) - i128::from(start);
    let span = span.max(0);
    let count = i128::try_from(cells.max(1)).unwrap_or(i128::MAX);
    let edge = move |cell: usize| {
        let offset = span * i128::try_from(cell).unwrap_or(i128::MAX) / count;
        i64::try_from(i128::from(start) + offset).unwrap_or(i64::MAX)
    };
    (0..cells).map(move |cell| (edge(cell), edge(cell + 1)))
}

/// One cell `[from, to)`: whole buckets where a held size's boundaries
/// fit, shares of the finest held size's bucket at the edges, and time no
/// size holds skipped to where one does.
fn cell_of<B: Divisible>(series: &TieredSeries<B>, from: i64, to: i64) -> B {
    let mut merged = B::new(from);
    let mut cursor = from;
    while cursor < to {
        if let Some(tier) = whole_fit(series, cursor, to) {
            take_bucket_at(series, tier, cursor, &mut merged);
            cursor = cursor.saturating_add(tier.step_i64());
            continue;
        }
        let Some(tier) = Tier::ALL
            .into_iter()
            .find(|tier| series.holds(*tier, cursor))
        else {
            match next_held(series, cursor) {
                Some(next) => cursor = next,
                None => break,
            }
            continue;
        };
        cursor = take_share(series, tier, (cursor, to), &mut merged);
    }
    merged
}

/// Merges the share of `tier`'s bucket holding `cursor` that falls before
/// `to`; returns where that share ends.
fn take_share<B: Divisible>(
    series: &TieredSeries<B>,
    tier: Tier,
    (cursor, to): (i64, i64),
    merged: &mut B,
) -> i64 {
    let step = tier.step_i64();
    let bucket_start = tier.align(cursor, series.shift_ns());
    let piece_end = bucket_start.saturating_add(step).min(to);
    let mut bucket = B::new(bucket_start);
    take_bucket_at(series, tier, bucket_start, &mut bucket);
    if !bucket.is_empty() {
        // Spans of at most a day in ns (< 2^47): exact in f64.
        #[allow(clippy::cast_precision_loss)]
        let fraction = (piece_end - cursor) as f64 / step as f64;
        merged.merge(&bucket.portion(fraction));
    }
    piece_end
}

/// The earliest time after `cursor` a size holds: no size holds `cursor`,
/// so each that was given buckets holds from its oldest one on.
fn next_held<B: Bucket>(series: &TieredSeries<B>, cursor: i64) -> Option<i64> {
    Tier::ALL
        .into_iter()
        .filter(|tier| series.buckets(*tier).next().is_some())
        .map(|tier| series.held_from(tier))
        .filter(|from| *from > cursor)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket::Level;

    const SECOND: i64 = 1_000_000_000;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;
    /// Midnight UTC.
    const MIDNIGHT: i64 = 1_790_208_000 * SECOND;

    /// A value read every 10 s for `minutes` from `start`, each held until
    /// the next reading.
    fn held(
        series: &mut TieredSeries<Level>,
        (start, minutes): (i64, i64),
        value: impl Fn(i64) -> f64,
    ) {
        for reading in 0..(minutes * 6) {
            let at = start + reading * 10 * SECOND;
            let level = value(reading / 6);
            series.record_span((at, at + 10 * SECOND), |bucket, piece_ns| {
                #[allow(clippy::cast_precision_loss)]
                let piece = piece_ns as f64;
                bucket.integral += level * piece;
                bucket.covered_ns += piece;
            });
        }
    }

    fn means(cells: &[Level]) -> Vec<Option<f64>> {
        cells.iter().map(Level::mean).collect()
    }

    #[allow(clippy::cast_precision_loss)]
    fn by_minute(minute: i64) -> f64 {
        (minute % 7) as f64
    }

    /// The mean of `by_minute` over minutes `[from, to)`.
    #[allow(clippy::cast_precision_loss)]
    fn brute_mean(from: i64, to: i64) -> f64 {
        (from..to).map(by_minute).sum::<f64>() / (to - from) as f64
    }

    fn close(left: Option<f64>, right: f64) -> bool {
        left.is_some_and(|left| (left - right).abs() < 1e-9)
    }

    /// Cells on a size's boundaries are the exact, absolute means of their
    /// time, the one being filled included.
    #[test]
    fn aligned_cells_are_the_exact_means_of_their_bars() {
        let mut series = TieredSeries::<Level>::new(0);
        held(&mut series, (MIDNIGHT, 3 * 60 + 20), by_minute);
        let cells = cells_between(&series, (MIDNIGHT, MIDNIGHT + 4 * HOUR), 4);
        let starts: Vec<i64> = cells
            .iter()
            .map(|cell| (cell.start_utc_ns - MIDNIGHT) / HOUR)
            .collect();
        assert_eq!(starts, vec![0, 1, 2, 3], "one cell a bar");
        for hour in 0..3 {
            let mean = cells[usize::try_from(hour).unwrap()].mean();
            assert!(close(mean, brute_mean(hour * 60, hour * 60 + 60)), "{hour}");
        }
        assert!(
            close(cells[3].mean(), brute_mean(180, 200)),
            "the hour being filled is the mean of the 20 minutes it holds"
        );
        assert!(
            (cells[3].covered_ns - 20.0 * 60e9).abs() < 1.0,
            "not padded"
        );
    }

    /// A cell edge inside a bucket takes the share of it in the cell.
    #[test]
    fn a_cut_bucket_shares_out_by_time() {
        let mut series = TieredSeries::<Level>::new(0);
        // 4 for 10 s from 0: buckets [0, 5) and [5, 10) s.
        series.record_span((MIDNIGHT, MIDNIGHT + 10 * SECOND), |bucket, piece| {
            #[allow(clippy::cast_precision_loss)]
            let piece = piece as f64;
            bucket.integral += 4.0 * piece;
            bucket.covered_ns += piece;
        });
        let window = (
            MIDNIGHT + 2 * SECOND + SECOND / 2,
            MIDNIGHT + 7 * SECOND + SECOND / 2,
        );
        let cells = cells_between(&series, window, 2);
        for cell in &cells {
            assert!((cell.covered_ns - 2.5e9).abs() < 1.0, "{cell:?}");
            assert!(close(cell.mean(), 4.0));
        }
        let tail = cells_between(&series, (MIDNIGHT, MIDNIGHT + 2 * SECOND), 1);
        assert!((tail[0].covered_ns - 2e9).abs() < 1.0, "2 s of the first 5");
    }

    /// Cells before the series and after it are empty: gaps, not zeros.
    #[test]
    fn cells_without_data_are_empty() {
        let mut series = TieredSeries::<Level>::new(0);
        held(&mut series, (MIDNIGHT + HOUR, 30), |_| 2.0);
        let cells = cells_between(&series, (MIDNIGHT, MIDNIGHT + 3 * HOUR), 3);
        assert_eq!(means(&cells), vec![None, Some(2.0), None]);
        assert!(cells[0].is_empty() && cells[2].is_empty());
        let none = cells_between(&TieredSeries::<Level>::new(0), (0, HOUR), 2);
        assert_eq!(means(&none), vec![None, None]);
        assert!(cells_between(&series, (0, HOUR), 0).is_empty());
        let inverted = cells_between(&series, (HOUR, 0), 2);
        assert_eq!(means(&inverted), vec![None, None], "an empty window");
    }

    /// Cells follow the local clock: at UTC+8:30 local hours are whole
    /// buckets, and each cell starts at its local hour.
    #[test]
    fn cells_on_the_local_clock_take_local_buckets() {
        let shift = 8 * HOUR + 30 * MINUTE;
        let mut series = TieredSeries::<Level>::new(shift);
        held(&mut series, (MIDNIGHT, 6 * 60), by_minute);
        let local_hour = Tier::OneHour.align(MIDNIGHT + 2 * HOUR, shift);
        let cells = cells_between(&series, (local_hour, local_hour + 2 * HOUR), 2);
        let first = (local_hour - MIDNIGHT) / MINUTE;
        assert_eq!(cells[0].start_utc_ns, local_hour);
        assert!(close(cells[0].mean(), brute_mean(first, first + 60)));
        assert!(close(cells[1].mean(), brute_mean(first + 60, first + 120)));
    }

    /// Two days in: the minutes reach back five hours, the five minutes
    /// 25 hours. A day's cell comes whole from the day's bucket; an older
    /// cell whose edges cut ten-minute buckets takes their shares.
    #[test]
    fn old_cells_come_from_coarser_buckets() {
        let mut series = TieredSeries::<Level>::new(0);
        held(&mut series, (MIDNIGHT, 2 * 24 * 60), |_| 3.0);
        assert!(series.held_from(Tier::OneMinute) > MIDNIGHT + 24 * HOUR);
        let day = cells_between(&series, (MIDNIGHT, MIDNIGHT + 24 * HOUR), 1);
        assert!(close(day[0].mean(), 3.0));
        assert!(
            (day[0].covered_ns - 24.0 * 3_600e9).abs() < 1.0,
            "whole day"
        );
        // 02:02-02:12 cuts the ten minutes 02:00 and 02:10.
        let from = MIDNIGHT + 2 * HOUR + 2 * MINUTE;
        let cut = cells_between(&series, (from, from + 10 * MINUTE), 1);
        assert!(close(cut[0].mean(), 3.0));
        assert!((cut[0].covered_ns - 600e9).abs() < 1.0, "{:?}", cut[0]);
    }
}
