//! A whole series as one bucket (D-77, D-79): its averages and peaks over
//! everything it holds, every sample counted once.

use dgxtop_core::tiers::Tier;

use crate::bucket::Bucket;
use crate::series::TieredSeries;

/// The series as one bucket: every bucket of the size that reaches back to
/// the earliest data ([`chart_tier`]), and what its sources are filling,
/// which has not reached it yet (each size merges its source's buckets as
/// they close, so every sample counts once).
pub fn whole_series<B: Bucket>(series: &TieredSeries<B>) -> B {
    let mut whole = B::new(0);
    let Some(tier) = chart_tier(series) else {
        return whole;
    };
    for bucket in series.buckets(tier).chain(series.unmerged(tier)) {
        whole.merge(bucket);
    }
    whole
}

/// The finest size that holds the whole series: its first bucket begins
/// inside the first bucket of every coarser size held (a coarse bucket
/// starts at its boundary, before the series may have; a finer size keeps
/// only its latest buckets). The coarsest held size otherwise.
pub fn chart_tier<B: Bucket>(series: &TieredSeries<B>) -> Option<Tier> {
    let held: Vec<(Tier, i64)> = Tier::ALL
        .into_iter()
        .filter_map(|tier| Some((tier, series.buckets(tier).next()?.start_utc_ns())))
        .collect();
    held.iter()
        .find(|(tier, first)| {
            held.iter()
                .filter(|(coarser, _)| coarser > tier)
                .all(|(coarser, start)| *first < start.saturating_add(coarser.step_i64()))
        })
        .or_else(|| held.last())
        .map(|(tier, _)| *tier)
}

#[cfg(test)]
mod tests {
    use dgxtop_core::process_history::{RunBucket, RunMetric};

    use super::*;
    use crate::bucket::Tally;
    use crate::run::RunSeries;

    const MINUTE: i64 = 60 * 1_000_000_000;

    /// After a restart the coarsest size may hold only what the new run
    /// saw; the run's figures come from the size that holds it all.
    #[test]
    fn whole_run_figures_take_the_size_that_holds_everything() {
        let mut series = RunSeries::default();
        for minute in 0..3 {
            let mut bucket = RunBucket::new(minute * MINUTE);
            for _ in 0..60 {
                bucket.add(RunMetric::Memory, Some(10.0));
            }
            series.restore(Tier::OneMinute, bucket, false);
        }
        let mut late = RunBucket::new(0);
        for _ in 0..4 {
            late.add(RunMetric::Memory, Some(0.0));
        }
        series.restore(Tier::TenMinutes, late, false);
        series.restore(Tier::OneDay, late, false);
        let whole = whole_series(&series);
        assert_eq!(whole.value(RunMetric::Memory).count, 180);
        assert_eq!(whole.value(RunMetric::Memory).mean(), Some(10.0));
    }

    /// A ten-minute bucket starts at its ten-minute boundary, before the
    /// run did; the minutes, which begin inside it, still cover the run.
    #[test]
    fn aligned_coarse_buckets_do_not_hide_finer_ones() {
        let mut series = RunSeries::default();
        let bucket = |start: i64| {
            let mut bucket = RunBucket::new(start);
            bucket.add(RunMetric::Cpu, Some(1.0));
            bucket
        };
        series.restore(Tier::OneDay, bucket(0), false);
        series.restore(Tier::TenMinutes, bucket(40 * MINUTE), false);
        for start in 46..50 {
            series.restore(Tier::OneMinute, bucket(start * MINUTE), false);
        }
        series.restore(Tier::Recent, bucket(48 * MINUTE), false);
        assert_eq!(chart_tier(&series), Some(Tier::OneMinute));
    }

    /// Any bucket: a live series counts its closed buckets and what is
    /// still filling once; an empty series is an empty bucket.
    #[test]
    fn any_bucket_type_sums_to_every_sample_once() {
        let mut series = TieredSeries::<Tally>::default();
        assert_eq!(chart_tier(&series), None);
        assert!(whole_series(&series).is_empty());
        for second in 0..200 {
            series.record(second * 1_000_000_000, |tally| tally.samples += 1);
        }
        assert_eq!(whole_series(&series).samples, 200);
    }
}
