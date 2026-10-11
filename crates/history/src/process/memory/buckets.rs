//! The bucket of one series of the process memory record (D-64, D-97): a
//! process's memory on one GPU, or the time process lists were recorded.
//! Each series is a [`TieredSeries`] of the shared compression, so it has
//! the bar sizes and local-clock boundaries of every other chart, each size
//! keeping its latest buckets (about a chart's width): old time is held by
//! the coarser sizes rather than dropped after a day.

use dgxtop_compression::{Bucket, Divisible, TieredSeries};

/// What a stretch of time held: the value integrated over the time it was
/// recorded, that time and the largest value.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Held {
    pub(crate) start_utc_ns: i64,
    /// Σ value × duration, in value·ns.
    pub(crate) integral: f64,
    pub(crate) covered_ns: u64,
    pub(crate) peak: Option<u64>,
}

impl Bucket for Held {
    fn new(start_utc_ns: i64) -> Self {
        Self {
            start_utc_ns,
            ..Self::default()
        }
    }

    fn start_utc_ns(&self) -> i64 {
        self.start_utc_ns
    }

    fn merge(&mut self, other: &Self) {
        self.integral += other.integral;
        self.covered_ns = self.covered_ns.saturating_add(other.covered_ns);
        self.peak = self.peak.max(other.peak);
    }

    fn is_empty(&self) -> bool {
        self.covered_ns == 0
    }
}

impl Divisible for Held {
    /// The part of a bucket's time a query overlaps, its recording assumed
    /// spread evenly over it; the peak stays, as it may be in that part.
    fn portion(&self, fraction: f64) -> Self {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let covered_ns = (self.covered_ns as f64 * fraction).round() as u64;
        Self {
            start_utc_ns: self.start_utc_ns,
            integral: self.integral * fraction,
            covered_ns,
            peak: self.peak,
        }
    }
}

/// One series of the record.
pub(super) type HeldSeries = TieredSeries<Held>;

/// Records `value` as held over `[start, end)` (UTC). Nothing of the
/// record is stored, so the closed buckets the series hands back are
/// dropped.
pub(super) fn add_held(series: &mut HeldSeries, (start, end): (i64, i64), value: u64) {
    series.record_span((start, end), |bucket, piece_ns| {
        let piece_ns = u64::try_from(piece_ns).unwrap_or(0);
        #[allow(clippy::cast_precision_loss)]
        let integral = value as f64 * piece_ns as f64;
        bucket.merge(&Held {
            start_utc_ns: bucket.start_utc_ns,
            integral,
            covered_ns: piece_ns,
            peak: Some(value),
        });
    });
}

#[cfg(test)]
mod tests {
    use dgxtop_compression::cells_between;
    use dgxtop_core::tiers::Tier;

    use super::*;

    /// What the tests read of a stretch.
    impl Held {
        /// Time-weighted mean over the time covered.
        pub(crate) fn mean(&self) -> Option<f64> {
            #[allow(clippy::cast_precision_loss)]
            let covered = self.covered_ns as f64;
            (self.covered_ns > 0).then(|| self.integral / covered)
        }
    }

    const SECOND: i64 = 1_000_000_000;
    const GIB: u64 = 1 << 30;

    fn held_in(series: &HeldSeries, window: (i64, i64)) -> Held {
        cells_between(series, window, 1)[0]
    }

    #[test]
    fn a_value_held_over_buckets_keeps_its_integral_time_and_peak() {
        let mut series = HeldSeries::new(0);
        // 4 GiB for 12 s from t = 100 s, then 8 GiB for 3 s: crosses the
        // 5 s and the minute edges.
        add_held(&mut series, (100 * SECOND, 112 * SECOND), 4 * GIB);
        add_held(&mut series, (112 * SECOND, 115 * SECOND), 8 * GIB);
        let held = held_in(&series, (100 * SECOND, 115 * SECOND));
        assert_eq!(held.covered_ns, 15_000_000_000);
        assert_eq!(held.peak, Some(8 * GIB));
        let mean = held.mean().unwrap();
        #[allow(clippy::cast_precision_loss)]
        let expected = (4.0 * 12.0 + 8.0 * 3.0) / 15.0 * GIB as f64;
        assert!((mean - expected).abs() < 1.0, "{mean} vs {expected}");
        let first = held_in(&series, (100 * SECOND, 105 * SECOND));
        assert_eq!(first.covered_ns, 5_000_000_000);
        assert_eq!(first.peak, Some(4 * GIB));
        assert!(held_in(&series, (200 * SECOND, 300 * SECOND)).is_empty());
        let minutes: Vec<i64> = series
            .buckets(Tier::OneMinute)
            .map(|bucket| bucket.start_utc_ns / SECOND)
            .collect();
        assert_eq!(minutes, vec![60], "the minute the 5 s buckets closed into");
    }

    /// A cut bucket keeps its peak and gives the share of its time.
    #[test]
    fn a_portion_shares_time_and_integral_and_keeps_the_peak() {
        let bucket = Held {
            start_utc_ns: 5,
            integral: 10.0,
            covered_ns: 4,
            peak: Some(9),
        };
        let half = bucket.portion(0.5);
        assert_eq!(
            half,
            Held {
                start_utc_ns: 5,
                integral: 5.0,
                covered_ns: 2,
                peak: Some(9)
            }
        );
        assert!(Held::new(0).is_empty());
    }

    #[test]
    fn an_empty_span_is_not_recorded() {
        let mut series = HeldSeries::new(0);
        add_held(&mut series, (5 * SECOND, 5 * SECOND), GIB);
        add_held(&mut series, (9 * SECOND, 3 * SECOND), GIB);
        assert!(series.is_empty());
        assert_eq!(held_in(&series, (0, 10 * SECOND)).mean(), None);
    }
}
