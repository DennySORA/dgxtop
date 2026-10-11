//! What the tiered aggregation needs of a bucket (D-79): one algorithm
//! serves every series, so it knows a bucket only by its start, how two
//! merge and whether it holds anything. Each series brings its own bucket
//! type; a process run's is [`RunBucket`].

use dgxtop_core::process_history::RunBucket;

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

/// `bucket` merged into a fresh bucket starting at `start_utc_ns`: the
/// first piece of a coarser bucket.
pub(crate) fn merged_at<B: Bucket>(start_utc_ns: i64, bucket: &B) -> B {
    let mut fresh = B::new(start_utc_ns);
    fresh.merge(bucket);
    fresh
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
