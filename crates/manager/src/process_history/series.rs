//! What one run used, in three bucket sizes (D-76). A sample lands in the
//! 5 s bucket being filled; when a bucket's time is over it closes, joins
//! the run's kept buckets of its size and merges into the bucket being
//! filled one size up. Closed minute and ten-minute buckets also go to the
//! long-term store.

use std::collections::VecDeque;

use dgxtop_core::process_history::{RUN_METRICS, RunBucket, RunMetric, Tier};

/// The sizes, finest first.
const TIERS: [Tier; 3] = [Tier::Recent, Tier::Minute, Tier::TenMinutes];

/// Minute buckets kept in memory (6 hours); the long-term store keeps 24.
const MEMORY_MINUTES: usize = 360;

/// One sample: each metric's value, `None` where unknown.
pub(crate) type Sample = [Option<f64>; RUN_METRICS];

#[derive(Debug, Clone, Default)]
pub(crate) struct RunSeries {
    kept: [VecDeque<RunBucket>; 3],
    filling: [Option<RunBucket>; 3],
}

impl RunSeries {
    /// Adds `sample` taken at `utc_ns`; returns the minute and ten-minute
    /// buckets it closed (for the long-term store).
    pub(crate) fn add(&mut self, utc_ns: i64, sample: &Sample) -> Vec<(Tier, RunBucket)> {
        let mut closed = Vec::new();
        self.close_before(utc_ns, &mut closed);
        let start = Tier::Recent.align(utc_ns);
        let bucket = self.filling[0].get_or_insert_with(|| RunBucket::new(start));
        for (metric, value) in RunMetric::ALL.iter().zip(sample) {
            bucket.add(*metric, *value);
        }
        closed
    }

    /// Closes every bucket being filled, as when the run ends.
    pub(crate) fn close_all(&mut self) -> Vec<(Tier, RunBucket)> {
        let mut closed = Vec::new();
        self.close_before(i64::MAX, &mut closed);
        closed
    }

    /// The buckets of `tier` kept, oldest first, then the one being filled.
    pub(crate) fn buckets(&self, tier: Tier) -> impl Iterator<Item = &RunBucket> {
        let level = level_of(tier);
        self.kept[level].iter().chain(self.filling[level].as_ref())
    }

    /// The buckets being filled, for the short-term file.
    pub(crate) fn filling(&self) -> Vec<(Tier, RunBucket)> {
        TIERS
            .iter()
            .zip(&self.filling)
            .filter_map(|(tier, bucket)| bucket.map(|bucket| (*tier, bucket)))
            .collect()
    }

    /// Puts back stored buckets (startup): closed ones join the kept
    /// buckets in time order, filling ones resume.
    pub(crate) fn restore(&mut self, tier: Tier, bucket: RunBucket, filling: bool) {
        let level = level_of(tier);
        if filling {
            self.filling[level] = Some(bucket);
            return;
        }
        let kept = &mut self.kept[level];
        let at = kept.partition_point(|old| old.start_utc_ns < bucket.start_utc_ns);
        if kept
            .get(at)
            .is_some_and(|old| old.start_utc_ns == bucket.start_utc_ns)
        {
            kept[at] = bucket;
        } else {
            kept.insert(at, bucket);
        }
        trim(kept, capacity(tier));
    }

    /// Whether any bucket of any size is held.
    pub(crate) fn is_empty(&self) -> bool {
        self.kept.iter().all(VecDeque::is_empty) && self.filling.iter().all(Option::is_none)
    }

    /// Drops every bucket (a run listed with its figures only).
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Buckets held, all sizes.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.kept.iter().map(VecDeque::len).sum::<usize>()
            + self
                .filling
                .iter()
                .filter(|bucket| bucket.is_some())
                .count()
    }

    /// Closes, finest first, each bucket whose time is over at `utc_ns`.
    fn close_before(&mut self, utc_ns: i64, closed: &mut Vec<(Tier, RunBucket)>) {
        for (level, tier) in TIERS.iter().enumerate() {
            let over = self.filling[level].is_some_and(|bucket| {
                let step = i64::try_from(tier.step_ns()).unwrap_or(i64::MAX);
                utc_ns >= bucket.start_utc_ns.saturating_add(step)
            });
            if over {
                self.close_level(level, closed);
            }
        }
    }

    /// Closes the bucket being filled at `level` now, whatever its time.
    fn close_level(&mut self, level: usize, closed: &mut Vec<(Tier, RunBucket)>) {
        let Some(bucket) = self.filling[level].take() else {
            return;
        };
        let tier = TIERS[level];
        if let Some(&next) = TIERS.get(level + 1) {
            let start = next.align(bucket.start_utc_ns);
            match &mut self.filling[level + 1] {
                Some(parent) if parent.start_utc_ns == start => parent.merge(&bucket),
                _ => {
                    self.close_level(level + 1, closed);
                    let mut fresh = RunBucket::new(start);
                    fresh.merge(&bucket);
                    self.filling[level + 1] = Some(fresh);
                }
            }
        }
        if tier != Tier::Recent {
            closed.push((tier, bucket));
        }
        let kept = &mut self.kept[level];
        kept.push_back(bucket);
        trim(kept, capacity(tier));
    }
}

const fn level_of(tier: Tier) -> usize {
    match tier {
        Tier::Recent => 0,
        Tier::Minute => 1,
        Tier::TenMinutes => 2,
    }
}

const fn capacity(tier: Tier) -> usize {
    match tier {
        Tier::Minute => MEMORY_MINUTES,
        Tier::Recent | Tier::TenMinutes => tier.capacity(),
    }
}

fn trim(kept: &mut VecDeque<RunBucket>, capacity: usize) {
    while kept.len() > capacity {
        kept.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: i64 = 1_000_000_000;

    fn sample(cpu: f64) -> Sample {
        let mut sample = [None; RUN_METRICS];
        sample[RunMetric::Cpu.index()] = Some(cpu);
        sample
    }

    fn cpu_means(series: &RunSeries, tier: Tier) -> Vec<(i64, f64)> {
        series
            .buckets(tier)
            .map(|bucket| {
                (
                    bucket.start_utc_ns / SECOND,
                    bucket.value(RunMetric::Cpu).mean().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn samples_fill_five_second_buckets_that_roll_into_minutes() {
        let mut series = RunSeries::default();
        let mut closed = Vec::new();
        for second in 0..125 {
            let busy = if second % 2 == 1 { 100.0 } else { 0.0 };
            closed.extend(series.add(second * SECOND, &sample(busy)));
        }
        let recent = cpu_means(&series, Tier::Recent);
        assert_eq!(recent.len(), 25, "24 closed and the one being filled");
        assert_eq!(recent[0], (0, 40.0), "0,100,0,100,0");
        let minutes: Vec<i64> = series
            .buckets(Tier::Minute)
            .map(|bucket| bucket.start_utc_ns / SECOND)
            .collect();
        assert_eq!(
            minutes,
            vec![0, 60],
            "a minute fills as its 5 s buckets close"
        );
        let first = series.buckets(Tier::Minute).next().unwrap();
        assert_eq!(first.value(RunMetric::Cpu).count, 60);
        assert_eq!(first.value(RunMetric::Cpu).mean(), Some(50.0));
        assert_eq!(first.value(RunMetric::Cpu).peak(), Some(100.0));
        let tiers: Vec<(Tier, i64)> = closed
            .iter()
            .map(|(tier, bucket)| (*tier, bucket.start_utc_ns / SECOND))
            .collect();
        assert_eq!(tiers, vec![(Tier::Minute, 0), (Tier::Minute, 60)]);
    }

    #[test]
    fn closing_all_flushes_every_size_once() {
        let mut series = RunSeries::default();
        series.add(0, &sample(10.0));
        series.add(7 * SECOND, &sample(30.0));
        let closed = series.close_all();
        let tiers: Vec<Tier> = closed.iter().map(|(tier, _)| *tier).collect();
        assert_eq!(tiers, vec![Tier::Minute, Tier::TenMinutes]);
        assert_eq!(closed[0].1.value(RunMetric::Cpu).mean(), Some(20.0));
        assert!(series.filling().is_empty());
        assert!(series.close_all().is_empty(), "nothing left to close");
        assert_eq!(cpu_means(&series, Tier::TenMinutes), vec![(0, 20.0)]);
    }

    /// A gap longer than a bucket (dgxtop suspended) closes the old buckets
    /// and starts new ones where the samples resume.
    #[test]
    fn a_long_gap_closes_buckets_without_bridging() {
        let mut series = RunSeries::default();
        series.add(0, &sample(10.0));
        let closed = series.add(3_600 * SECOND + 3 * SECOND, &sample(90.0));
        assert_eq!(
            closed.iter().map(|(tier, _)| *tier).collect::<Vec<_>>(),
            vec![Tier::Minute, Tier::TenMinutes]
        );
        assert_eq!(
            cpu_means(&series, Tier::Recent),
            vec![(0, 10.0), (3_600, 90.0)]
        );
        assert_eq!(cpu_means(&series, Tier::TenMinutes), vec![(0, 10.0)]);
    }

    #[test]
    fn kept_buckets_are_bounded_and_restored_in_order() {
        let mut series = RunSeries::default();
        for minute in 0..(i64::try_from(MEMORY_MINUTES).unwrap() + 10) {
            series.add(minute * 60 * SECOND, &sample(1.0));
        }
        assert_eq!(series.kept[1].len(), MEMORY_MINUTES);
        let mut restored = RunSeries::default();
        for start in [120, 0, 60, 60] {
            let mut bucket = RunBucket::new(start * SECOND);
            bucket.add(
                RunMetric::Cpu,
                Some(f64::from(u8::try_from(start / 60).unwrap())),
            );
            restored.restore(Tier::Minute, bucket, false);
        }
        assert_eq!(
            cpu_means(&restored, Tier::Minute),
            vec![(0, 0.0), (60, 1.0), (120, 2.0)],
            "ordered, a repeated start replaced"
        );
        let mut filling = RunBucket::new(180 * SECOND);
        filling.add(RunMetric::Cpu, Some(5.0));
        restored.restore(Tier::Minute, filling, true);
        assert_eq!(restored.len(), 4);
        restored.clear();
        assert!(restored.is_empty());
    }
}
