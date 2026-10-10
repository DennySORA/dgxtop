//! Fixed-width history records.
//!
//! None of these records owns heap memory: a chunk of records is a single
//! allocation whose size is known before it is made, and no per-sample string
//! is ever stored (sources are referenced through a per-series segment
//! ordinal).

use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::metric::{Scalar, ScalarView};

/// Width of one rollup bucket. `store.aggregate_seconds` is fixed at 60 by
/// configuration validation, so the bucket width is a constant.
pub(crate) const BUCKET_NS: u64 = 60 * NANOS_PER_SECOND;

/// Start of the minute bucket containing `time_ns`.
pub(crate) const fn floor_bucket(time_ns: u64) -> u64 {
    time_ns - time_ns % BUCKET_NS
}

/// First bucket boundary at or after `time_ns`.
pub(crate) const fn ceil_bucket(time_ns: u64) -> u64 {
    let floor = floor_bucket(time_ns);
    if floor == time_ns {
        floor
    } else {
        floor.saturating_add(BUCKET_NS)
    }
}

/// Time extent of a record; logs are ordered by both start and end.
pub(crate) trait TimeSpan: Copy {
    fn start_ns(&self) -> u64;
    fn end_ns(&self) -> u64;
}

/// One accepted canonical gauge sample. `value: None` is a cut marker: the
/// series had no valid value from `time_ns` on (conflict or unavailable), so
/// the previous step must not be extended past it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GaugeSample {
    pub time_ns: u64,
    /// `time_ns + stale_factor x period`, fixed when the sample was accepted
    /// so later configuration changes never rewrite stored history.
    pub stale_deadline_ns: u64,
    pub value: Option<Scalar>,
    pub segment: u32,
}

impl TimeSpan for GaugeSample {
    fn start_ns(&self) -> u64 {
        self.time_ns
    }

    fn end_ns(&self) -> u64 {
        self.time_ns
    }
}

/// One exact counter movement over the complete interval `[start, end)`.
/// Also used for bridges (intervals that cross a bucket boundary).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CounterSample {
    pub start_ns: u64,
    pub end_ns: u64,
    pub delta: Scalar,
    pub segment: u32,
}

impl TimeSpan for CounterSample {
    fn start_ns(&self) -> u64 {
        self.start_ns
    }

    fn end_ns(&self) -> u64 {
        self.end_ns
    }
}

/// Time-weighted aggregate of one segment's steps inside one minute bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GaugeBucket {
    pub start_ns: u64,
    pub covered_ns: u64,
    /// `sum(value x covered_ns)` in value-nanoseconds.
    pub weighted_sum: f64,
    pub minimum: f64,
    pub maximum: f64,
    pub segment: u32,
    pub sample_count: u32,
}

impl GaugeBucket {
    pub(crate) const fn empty(start_ns: u64, segment: u32) -> Self {
        Self {
            start_ns,
            covered_ns: 0,
            weighted_sum: 0.0,
            minimum: f64::INFINITY,
            maximum: f64::NEG_INFINITY,
            segment,
            sample_count: 0,
        }
    }

    /// Adds `covered_ns` of a step holding `value`.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn add_step(&mut self, value: f64, covered_ns: u64) {
        self.covered_ns = self.covered_ns.saturating_add(covered_ns);
        self.weighted_sum += value * covered_ns as f64;
        self.minimum = self.minimum.min(value);
        self.maximum = self.maximum.max(value);
    }
}

impl TimeSpan for GaugeBucket {
    fn start_ns(&self) -> u64 {
        self.start_ns
    }

    fn end_ns(&self) -> u64 {
        self.start_ns.saturating_add(BUCKET_NS)
    }
}

/// Exact counter total. `u64` deltas are summed in `u128` with checked
/// arithmetic; non-integral (`f64`) deltas are kept apart so an integral total
/// is never silently converted to floating point.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct CounterTotal {
    pub exact: u128,
    pub float: f64,
    pub float_count: u32,
    pub overflowed: bool,
}

impl CounterTotal {
    pub(crate) fn add_delta(&mut self, delta: Scalar) {
        match delta.view() {
            ScalarView::U64(value) => self.add_exact(u128::from(value)),
            ScalarView::F64(value) => {
                self.float += value;
                self.float_count = self.float_count.saturating_add(1);
            }
        }
    }

    pub(crate) fn add_total(&mut self, other: &CounterTotal) {
        self.add_exact(other.exact);
        self.overflowed |= other.overflowed;
        self.float += other.float;
        self.float_count = self.float_count.saturating_add(other.float_count);
    }

    fn add_exact(&mut self, value: u128) {
        match self.exact.checked_add(value) {
            Some(sum) => self.exact = sum,
            None => {
                self.exact = u128::MAX;
                self.overflowed = true;
            }
        }
    }

    /// The exact integral total when no `f64` delta contributed.
    pub(crate) fn integral(self) -> Option<u128> {
        (self.float_count == 0 && !self.overflowed).then_some(self.exact)
    }

    /// Scalar value plus a reason when the total could not stay an exact `u64`.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn to_scalar(self) -> Option<(Scalar, Option<&'static str>)> {
        if let Some(exact) = self.integral() {
            return Some(match u64::try_from(exact) {
                Ok(value) => (Scalar::from_u64(value), None),
                Err(_) => (
                    Scalar::from_f64(exact as f64).ok()?,
                    Some("total_exceeds_u64"),
                ),
            });
        }
        let reason = if self.overflowed {
            "total_overflowed_u128"
        } else {
            "non_integral_delta"
        };
        let value = Scalar::from_f64(self.exact as f64 + self.float).ok()?;
        Some((value, Some(reason)))
    }
}

/// Exact counter total of the intervals fully inside one minute bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CounterBucket {
    pub total: CounterTotal,
    pub start_ns: u64,
    /// Summed length of the counted intervals (they never overlap).
    pub covered_ns: u64,
    pub segment: u32,
    pub interval_count: u32,
}

impl CounterBucket {
    pub(crate) fn empty(start_ns: u64, segment: u32) -> Self {
        Self {
            total: CounterTotal::default(),
            start_ns,
            covered_ns: 0,
            segment,
            interval_count: 0,
        }
    }

    pub(crate) fn add_interval(&mut self, interval: &CounterSample) {
        self.total.add_delta(interval.delta);
        self.covered_ns = self
            .covered_ns
            .saturating_add(interval.end_ns.saturating_sub(interval.start_ns));
        self.interval_count = self.interval_count.saturating_add(1);
    }
}

impl TimeSpan for CounterBucket {
    fn start_ns(&self) -> u64 {
        self.start_ns
    }

    fn end_ns(&self) -> u64 {
        self.start_ns.saturating_add(BUCKET_NS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_alignment() {
        assert_eq!(floor_bucket(0), 0);
        assert_eq!(floor_bucket(BUCKET_NS - 1), 0);
        assert_eq!(floor_bucket(BUCKET_NS), BUCKET_NS);
        assert_eq!(ceil_bucket(0), 0);
        assert_eq!(ceil_bucket(1), BUCKET_NS);
        assert_eq!(ceil_bucket(BUCKET_NS), BUCKET_NS);
        assert_eq!(ceil_bucket(u64::MAX), u64::MAX);
    }

    #[test]
    fn integral_totals_stay_exact_and_floats_are_flagged() {
        let mut total = CounterTotal::default();
        total.add_delta(Scalar::from_u64(u64::MAX));
        total.add_delta(Scalar::from_u64(1));
        assert_eq!(total.integral(), Some(u128::from(u64::MAX) + 1));
        let (value, reason) = total.to_scalar().unwrap();
        assert_eq!(reason, Some("total_exceeds_u64"));
        assert!(value.as_u64().is_none());

        let mut small = CounterTotal::default();
        small.add_delta(Scalar::from_u64(300));
        assert_eq!(small.to_scalar().unwrap(), (Scalar::from_u64(300), None));
        small.add_delta(Scalar::from_f64(0.5).unwrap());
        let (value, reason) = small.to_scalar().unwrap();
        assert_eq!(reason, Some("non_integral_delta"));
        assert_eq!(value.to_f64_lossy(), 300.5);
        assert_eq!(small.integral(), None);
    }

    #[test]
    fn u128_overflow_is_detected_not_wrapped() {
        let mut total = CounterTotal {
            exact: u128::MAX - 1,
            ..CounterTotal::default()
        };
        total.add_delta(Scalar::from_u64(5));
        assert!(total.overflowed);
        assert_eq!(total.integral(), None);
        assert_eq!(total.to_scalar().unwrap().1, Some("total_overflowed_u128"));
    }

    #[test]
    fn gauge_bucket_tracks_extremes() {
        let mut bucket = GaugeBucket::empty(0, 0);
        bucket.add_step(10.0, 2);
        bucket.add_step(-4.0, 1);
        assert_eq!(bucket.covered_ns, 3);
        assert_eq!(bucket.weighted_sum, 16.0);
        assert_eq!(bucket.minimum, -4.0);
        assert_eq!(bucket.maximum, 10.0);
    }
}
