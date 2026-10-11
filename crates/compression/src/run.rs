//! The tiered series of a process run (D-76, D-77): its buckets are
//! [`RunBucket`]s and a sample is each [`RunMetric`]'s value.

use dgxtop_core::process_history::{RUN_METRICS, RunBucket, RunMetric, Tier};

use crate::series::TieredSeries;

/// One sample of a run: each metric's value, `None` where unknown.
pub type RunSample = [Option<f64>; RUN_METRICS];

/// What one run used, in every bucket size.
pub type RunSeries = TieredSeries<RunBucket>;

impl TieredSeries<RunBucket> {
    /// Adds `sample` taken at `utc_ns`; returns the buckets of a minute and
    /// more it closed (for the long-term store).
    pub fn add(&mut self, utc_ns: i64, sample: &RunSample) -> Vec<(Tier, RunBucket)> {
        self.record(utc_ns, |bucket| {
            for (metric, value) in RunMetric::ALL.iter().zip(sample) {
                bucket.add(*metric, *value);
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_adds_each_known_metric() {
        let mut series = RunSeries::default();
        let mut sample: RunSample = [None; RUN_METRICS];
        sample[RunMetric::Cpu.index()] = Some(25.0);
        sample[RunMetric::Swap.index()] = Some(f64::NAN);
        assert!(series.add(0, &sample).is_empty(), "nothing closed yet");
        let bucket = series.buckets(Tier::Recent).next().unwrap();
        assert_eq!(bucket.value(RunMetric::Cpu).mean(), Some(25.0));
        assert_eq!(bucket.value(RunMetric::Memory).count, 0, "unknown");
        assert_eq!(bucket.value(RunMetric::Swap).count, 0, "not finite");
    }
}
