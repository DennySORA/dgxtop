//! Time buckets of one series of the process memory record (D-64): a
//! process's memory on one GPU, or the time process lists were recorded.
//! Two resolutions keep it bounded: 5 s buckets over the last 10 minutes,
//! fine enough for the 5 minute history window, and minute buckets over
//! the 24 hours of the longest one.

use std::collections::VecDeque;

use dgxtop_core::clock::NANOS_PER_SECOND;

/// Width of a bucket and the time a tier keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TierSpec {
    width_ns: u64,
    span_ns: u64,
}

const FINE: TierSpec = TierSpec {
    width_ns: 5 * NANOS_PER_SECOND,
    span_ns: 600 * NANOS_PER_SECOND,
};
/// A minute more than 24 hours, so a 24 hour window that does not start on
/// a minute is still whole.
const COARSE: TierSpec = TierSpec {
    width_ns: 60 * NANOS_PER_SECOND,
    span_ns: (86_400 + 60) * NANOS_PER_SECOND,
};

/// What a stretch of time held: the value integrated over the time it was
/// recorded, that time and the largest value.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Held {
    /// Σ value × duration, in value·ns.
    pub(crate) integral: f64,
    pub(crate) covered_ns: u64,
    pub(crate) peak: Option<u64>,
}

impl Held {
    /// Time-weighted mean over the time covered.
    pub(crate) fn mean(&self) -> Option<f64> {
        #[allow(clippy::cast_precision_loss)]
        let covered = self.covered_ns as f64;
        (self.covered_ns > 0).then(|| self.integral / covered)
    }

    fn absorb(&mut self, other: Self) {
        self.integral += other.integral;
        self.covered_ns = self.covered_ns.saturating_add(other.covered_ns);
        self.peak = self.peak.max(other.peak);
    }

    /// The part of a bucket's time a query overlaps, assuming its
    /// recording was spread evenly over it.
    fn share(self, fraction: f64) -> Self {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let covered_ns = (self.covered_ns as f64 * fraction).round() as u64;
        Self {
            integral: self.integral * fraction,
            covered_ns,
            peak: self.peak,
        }
    }
}

/// One bucket; `first_ns..last_ns` is where in it something was recorded,
/// so a query cutting the newest, partly filled bucket counts what it cuts.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    start_ns: u64,
    first_ns: u64,
    last_ns: u64,
    held: Held,
}

impl Bucket {
    /// What of this bucket falls in `[start, end)`.
    fn held_in(&self, start: u64, end: u64) -> Held {
        let recorded = self.last_ns.saturating_sub(self.first_ns);
        let overlap = end
            .min(self.last_ns)
            .saturating_sub(start.max(self.first_ns));
        if recorded == 0 || overlap == 0 {
            return Held::default();
        }
        if overlap >= recorded {
            return self.held;
        }
        #[allow(clippy::cast_precision_loss)]
        let fraction = overlap as f64 / recorded as f64;
        self.held.share(fraction)
    }
}

/// One resolution: its buckets in time order.
#[derive(Debug, Clone)]
struct Tier {
    spec: TierSpec,
    buckets: VecDeque<Bucket>,
}

impl Tier {
    const fn new(spec: TierSpec) -> Self {
        Self {
            spec,
            buckets: VecDeque::new(),
        }
    }

    /// Records `value` as held over `[start, end)`, split at bucket edges.
    fn add(&mut self, start: u64, end: u64, value: u64) {
        let mut at = start;
        while at < end {
            let bucket_start = at - at % self.spec.width_ns;
            let piece_end = bucket_start.saturating_add(self.spec.width_ns).min(end);
            let piece_ns = piece_end - at;
            #[allow(clippy::cast_precision_loss)]
            let integral = value as f64 * piece_ns as f64;
            let bucket = self.bucket_at(bucket_start, at);
            bucket.first_ns = bucket.first_ns.min(at);
            bucket.last_ns = bucket.last_ns.max(piece_end);
            bucket.held.absorb(Held {
                integral,
                covered_ns: piece_ns,
                peak: Some(value),
            });
            at = piece_end;
        }
        self.expire(end);
    }

    /// The bucket starting at `start`; records arrive in time order, so it
    /// is the newest bucket or a new one after it.
    fn bucket_at(&mut self, start: u64, at: u64) -> &mut Bucket {
        let position = self
            .buckets
            .partition_point(|bucket| bucket.start_ns < start);
        if self
            .buckets
            .get(position)
            .is_none_or(|bucket| bucket.start_ns != start)
        {
            self.buckets.insert(
                position,
                Bucket {
                    start_ns: start,
                    first_ns: at,
                    last_ns: at,
                    held: Held::default(),
                },
            );
        }
        &mut self.buckets[position]
    }

    /// Drops buckets that ended `span` or more before `now`.
    fn expire(&mut self, now: u64) {
        let cutoff = now.saturating_sub(self.spec.span_ns);
        while self
            .buckets
            .front()
            .is_some_and(|bucket| bucket.start_ns.saturating_add(self.spec.width_ns) <= cutoff)
        {
            self.buckets.pop_front();
        }
    }

    fn held(&self, start: u64, end: u64) -> Held {
        let first = self
            .buckets
            .partition_point(|bucket| bucket.start_ns.saturating_add(self.spec.width_ns) <= start);
        let mut total = Held::default();
        for bucket in self
            .buckets
            .range(first..)
            .take_while(|bucket| bucket.start_ns < end)
        {
            total.absorb(bucket.held_in(start, end));
        }
        total
    }
}

/// A series at both resolutions.
#[derive(Debug, Clone)]
pub(super) struct TimeBuckets {
    fine: Tier,
    coarse: Tier,
    /// End of the newest record.
    newest_ns: u64,
}

impl Default for TimeBuckets {
    fn default() -> Self {
        Self {
            fine: Tier::new(FINE),
            coarse: Tier::new(COARSE),
            newest_ns: 0,
        }
    }
}

impl TimeBuckets {
    /// Records `value` as held over `[start, end)` on the boot clock.
    pub(super) fn add(&mut self, start: u64, end: u64, value: u64) {
        if end <= start {
            return;
        }
        self.fine.add(start, end, value);
        self.coarse.add(start, end, value);
        self.newest_ns = self.newest_ns.max(end);
    }

    /// What `[start, end)` held: from the 5 s buckets while they reach back
    /// to `start`, else from the minutes.
    pub(super) fn held(&self, start: u64, end: u64) -> Held {
        if end <= start {
            return Held::default();
        }
        if start >= self.newest_ns.saturating_sub(FINE.span_ns) {
            self.fine.held(start, end)
        } else {
            self.coarse.held(start, end)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: u64 = NANOS_PER_SECOND;
    const GIB: u64 = 1 << 30;

    #[test]
    fn a_value_held_over_buckets_keeps_its_integral_time_and_peak() {
        let mut series = TimeBuckets::default();
        // 4 GiB for 12 s from t = 100 s, then 8 GiB for 3 s: crosses the
        // 5 s and the minute edges.
        series.add(100 * SECOND, 112 * SECOND, 4 * GIB);
        series.add(112 * SECOND, 115 * SECOND, 8 * GIB);
        let held = series.held(100 * SECOND, 115 * SECOND);
        assert_eq!(held.covered_ns, 15 * SECOND);
        assert_eq!(held.peak, Some(8 * GIB));
        let mean = held.mean().unwrap();
        #[allow(clippy::cast_precision_loss)]
        let expected = (4.0 * 12.0 + 8.0 * 3.0) / 15.0 * GIB as f64;
        assert!((mean - expected).abs() < 1.0, "{mean} vs {expected}");
        // A query of part of the time counts that part only.
        let first = series.held(100 * SECOND, 105 * SECOND);
        assert_eq!(first.covered_ns, 5 * SECOND);
        assert_eq!(first.peak, Some(4 * GIB));
        assert_eq!(series.held(200 * SECOND, 300 * SECOND), Held::default());
        assert_eq!(series.held(110 * SECOND, 110 * SECOND), Held::default());
    }

    /// The newest bucket is partly filled: a query cutting it counts the
    /// time it cuts of what was recorded, not of the bucket's width.
    #[test]
    fn a_query_cutting_a_partly_filled_bucket_counts_what_it_cuts() {
        let mut series = TimeBuckets::default();
        series.add(60 * SECOND, 62 * SECOND, GIB);
        let held = series.held(61 * SECOND, 70 * SECOND);
        assert_eq!(held.covered_ns, SECOND);
        assert!((held.mean().unwrap() - f64::from(1u32 << 30)).abs() < 1e-3);
    }

    /// Older than the fine tier keeps, a query is answered by the minutes;
    /// older than 24 hours, nothing is left.
    #[test]
    fn old_time_comes_from_the_minutes_and_a_day_is_the_limit() {
        let mut series = TimeBuckets::default();
        let start = 1_000 * SECOND;
        // An hour at 2 GiB, one record a second as the recorder sends them.
        for second in 0..3_600 {
            series.add(
                start + second * SECOND,
                start + (second + 1) * SECOND,
                2 * GIB,
            );
        }
        assert!(series.fine.buckets.len() <= 121);
        let early = series.held(start, start + 600 * SECOND);
        assert_eq!(early.covered_ns, 600 * SECOND);
        assert_eq!(early.peak, Some(2 * GIB));
        let whole = series.held(start, start + 3_600 * SECOND);
        assert_eq!(whole.covered_ns, 3_600 * SECOND);
        // A day and more later the hour has expired.
        let later = start + 3_600 * SECOND + 86_500 * SECOND;
        series.add(later, later + SECOND, GIB);
        assert_eq!(series.held(start, start + 3_600 * SECOND), Held::default());
        assert_eq!(series.held(later, later + SECOND).covered_ns, SECOND);
    }

    #[test]
    fn an_empty_span_is_not_recorded() {
        let mut series = TimeBuckets::default();
        series.add(5 * SECOND, 5 * SECOND, GIB);
        series.add(9 * SECOND, 3 * SECOND, GIB);
        assert!(series.coarse.buckets.is_empty() && series.fine.buckets.is_empty());
        assert_eq!(series.held(0, 10 * SECOND).mean(), None);
    }
}
