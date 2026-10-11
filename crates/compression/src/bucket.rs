//! What the tiered aggregation needs of a bucket (D-79): one algorithm
//! serves every series, so it knows a bucket only by its start, how two
//! merge and whether it holds anything. Each series brings its own bucket
//! type; a process run's is [`RunBucket`], an Overview series'
//! [`OverviewBucket`] (D-82).

use dgxtop_core::overview_history::OverviewBucket;
use dgxtop_core::process_history::RunBucket;
use dgxtop_core::tiers::Tier;

/// A span of time's merged samples, of any size.
pub trait Bucket: Copy {
    /// An empty bucket starting at `start_utc_ns`.
    fn new(start_utc_ns: i64) -> Self;
    /// Its start, UTC, aligned to its size's step.
    fn start_utc_ns(&self) -> i64;
    /// Takes in every sample `other` holds.
    fn merge(&mut self, other: &Self);
    /// Whether it holds no sample.
    fn is_empty(&self) -> bool;
}

/// A bucket whose samples can be cut by time: a window that cuts a bucket
/// takes the share of it that falls inside (D-97), its samples assumed
/// spread evenly over the bucket's time. Only a bucket that integrates
/// over time cuts without bias; one that counts samples does not.
pub trait Divisible: Bucket {
    /// The share `fraction` (in `(0, 1)`) of this bucket's time holds.
    #[must_use]
    fn portion(&self, fraction: f64) -> Self;
}

impl Bucket for RunBucket {
    fn new(start_utc_ns: i64) -> Self {
        RunBucket::new(start_utc_ns)
    }

    fn start_utc_ns(&self) -> i64 {
        self.start_utc_ns
    }

    fn merge(&mut self, other: &Self) {
        RunBucket::merge(self, other);
    }

    fn is_empty(&self) -> bool {
        RunBucket::is_empty(self)
    }
}

impl Bucket for OverviewBucket {
    fn new(start_utc_ns: i64) -> Self {
        OverviewBucket::new(start_utc_ns)
    }

    fn start_utc_ns(&self) -> i64 {
        self.start_utc_ns
    }

    fn merge(&mut self, other: &Self) {
        OverviewBucket::merge(self, other);
    }

    fn is_empty(&self) -> bool {
        OverviewBucket::is_empty(self)
    }
}

/// `bucket` merged into a fresh bucket starting at `start_utc_ns`: the
/// first piece of a coarser bucket.
pub(crate) fn merged_at<B: Bucket>(start_utc_ns: i64, bucket: &B) -> B {
    let mut fresh = B::new(start_utc_ns);
    fresh.merge(bucket);
    fresh
}

/// Merges each of `pieces` into the bucket of `tier` holding it (its
/// boundaries moved by `shift_ns`), keeping `groups` in time order; a piece
/// whose bucket is not there yet starts it. The one regrouping every
/// coarser view of finer buckets uses: resuming, backfilling and bars.
pub(crate) fn regroup_into<'a, B: Bucket + 'a>(
    groups: &mut Vec<B>,
    tier: Tier,
    shift_ns: i64,
    pieces: impl IntoIterator<Item = &'a B>,
) {
    for piece in pieces {
        let start = tier.align(piece.start_utc_ns(), shift_ns);
        let position = groups.partition_point(|group| group.start_utc_ns() < start);
        match groups.get_mut(position) {
            Some(group) if group.start_utc_ns() == start => group.merge(piece),
            _ => groups.insert(position, merged_at(start, piece)),
        }
    }
}

/// A test bucket that only counts samples, to show the algorithm needs
/// nothing of a run's metrics.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tally {
    pub(crate) start_utc_ns: i64,
    pub(crate) samples: u32,
}

#[cfg(test)]
impl Bucket for Tally {
    fn new(start_utc_ns: i64) -> Self {
        Self {
            start_utc_ns,
            samples: 0,
        }
    }

    fn start_utc_ns(&self) -> i64 {
        self.start_utc_ns
    }

    fn merge(&mut self, other: &Self) {
        self.samples += other.samples;
    }

    fn is_empty(&self) -> bool {
        self.samples == 0
    }
}

/// A test bucket that integrates a value over time, to show how a cut
/// bucket shares out.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Level {
    pub(crate) start_utc_ns: i64,
    /// Σ value × time.
    pub(crate) integral: f64,
    pub(crate) covered_ns: f64,
}

#[cfg(test)]
impl Level {
    pub(crate) fn mean(&self) -> Option<f64> {
        (self.covered_ns > 0.0).then(|| self.integral / self.covered_ns)
    }
}

#[cfg(test)]
impl Bucket for Level {
    fn new(start_utc_ns: i64) -> Self {
        Self {
            start_utc_ns,
            integral: 0.0,
            covered_ns: 0.0,
        }
    }

    fn start_utc_ns(&self) -> i64 {
        self.start_utc_ns
    }

    fn merge(&mut self, other: &Self) {
        self.integral += other.integral;
        self.covered_ns += other.covered_ns;
    }

    fn is_empty(&self) -> bool {
        self.covered_ns <= 0.0
    }
}

#[cfg(test)]
impl Divisible for Level {
    fn portion(&self, fraction: f64) -> Self {
        Self {
            start_utc_ns: self.start_utc_ns,
            integral: self.integral * fraction,
            covered_ns: self.covered_ns * fraction,
        }
    }
}

#[cfg(test)]
mod tests {
    use dgxtop_core::process_history::RunMetric;

    use super::*;

    #[test]
    fn a_run_bucket_is_a_bucket() {
        let mut one = <RunBucket as Bucket>::new(60);
        assert!(Bucket::is_empty(&one));
        let mut other = RunBucket::new(0);
        other.add(RunMetric::Cpu, Some(4.0));
        Bucket::merge(&mut one, &other);
        assert_eq!(Bucket::start_utc_ns(&one), 60, "the start stays");
        assert!(!Bucket::is_empty(&one));
        assert_eq!(one.value(RunMetric::Cpu).mean(), Some(4.0));
    }

    #[test]
    fn an_overview_bucket_is_a_bucket() {
        let mut hour = <OverviewBucket as Bucket>::new(0);
        assert!(Bucket::is_empty(&hour));
        let minute = OverviewBucket::gauge(60, 30, 300.0, (5.0, 15.0), 2);
        Bucket::merge(&mut hour, &minute);
        assert_eq!(Bucket::start_utc_ns(&hour), 0);
        assert_eq!(hour.mean(), Some(10.0));
        assert_eq!(hour.peak(), Some(15.0));
    }

    #[test]
    fn merged_at_starts_a_bucket_holding_the_piece() {
        let piece = Tally {
            start_utc_ns: 7,
            samples: 3,
        };
        assert_eq!(
            merged_at(0, &piece),
            Tally {
                start_utc_ns: 0,
                samples: 3
            }
        );
    }
}
