//! What one run used, in every bucket size (D-76, D-77). A sample lands in
//! the 5 s bucket being filled; when a bucket's time is over it closes,
//! joins the run's kept buckets of its size (the latest
//! [`Tier::capacity`]) and merges into the bucket being filled of each size
//! it is the source of ([`Tier::source`]). Closed buckets of a minute and
//! more also go to the long-term store. Buckets follow the local clock
//! (`shift_ns`), so a day's bucket is a local day.

use std::collections::VecDeque;

use dgxtop_core::process_history::{RUN_METRICS, RunBucket, RunMetric, Tier};

const LEVELS: usize = Tier::ALL.len();

/// One sample: each metric's value, `None` where unknown.
pub(crate) type Sample = [Option<f64>; RUN_METRICS];

#[derive(Debug, Clone, Default)]
pub(crate) struct RunSeries {
    kept: [VecDeque<RunBucket>; LEVELS],
    filling: [Option<RunBucket>; LEVELS],
    /// Added to UTC before aligning a bucket: the local offset.
    shift_ns: i64,
}

impl RunSeries {
    pub(crate) fn new(shift_ns: i64) -> Self {
        Self {
            shift_ns,
            ..Self::default()
        }
    }

    pub(crate) fn shift_ns(&self) -> i64 {
        self.shift_ns
    }

    /// Adds `sample` taken at `utc_ns`; returns the buckets of a minute and
    /// more it closed (for the long-term store).
    pub(crate) fn add(&mut self, utc_ns: i64, sample: &Sample) -> Vec<(Tier, RunBucket)> {
        let mut closed = Vec::new();
        self.close_before(utc_ns, &mut closed);
        let start = Tier::Recent.align(utc_ns, self.shift_ns);
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
        let level = tier.level();
        self.kept[level].iter().chain(self.filling[level].as_ref())
    }

    /// What `tier` has not merged yet: the buckets being filled of its
    /// source, the source's source and so on.
    pub(crate) fn unmerged(&self, tier: Tier) -> impl Iterator<Item = &RunBucket> {
        std::iter::successors(tier.source(), |source| source.source())
            .filter_map(|source| self.filling[source.level()].as_ref())
    }

    /// The buckets being filled, for the short-term file.
    pub(crate) fn filling(&self) -> Vec<(Tier, RunBucket)> {
        Tier::ALL
            .iter()
            .zip(&self.filling)
            .filter_map(|(tier, bucket)| bucket.map(|bucket| (*tier, bucket)))
            .collect()
    }

    /// Puts back stored buckets (startup): closed ones join the kept
    /// buckets in time order, filling ones resume.
    pub(crate) fn restore(&mut self, tier: Tier, bucket: RunBucket, filling: bool) {
        let level = tier.level();
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
        trim(kept, tier.capacity());
    }

    /// After a restore: each size takes what its source holds from before
    /// its own first bucket (a size added since those buckets were stored,
    /// or one whose buckets were lost), so every size reaches back as far
    /// as the finer ones do. A `live` run's sources are still filling: what
    /// they fill reaches each size when it closes. Returns the buckets it
    /// made, for the store.
    pub(crate) fn backfill(&mut self, live: bool) -> Vec<(Tier, RunBucket)> {
        let mut made = Vec::new();
        for tier in Tier::ALL {
            let Some(source) = tier.source().filter(|source| *source != Tier::Recent) else {
                continue;
            };
            let first = self
                .buckets(tier)
                .next()
                .map_or(i64::MAX, |bucket| bucket.start_utc_ns);
            let mut older: Vec<RunBucket> = Vec::new();
            let source_filling = self.filling[source.level()].filter(|_| !live);
            for bucket in self.kept[source.level()]
                .iter()
                .chain(source_filling.as_ref())
                .filter(|bucket| bucket.start_utc_ns < first)
            {
                let start = tier.align(bucket.start_utc_ns, self.shift_ns);
                match older.last_mut() {
                    Some(last) if last.start_utc_ns == start => last.merge(bucket),
                    _ => {
                        let mut fresh = RunBucket::new(start);
                        fresh.merge(bucket);
                        older.push(fresh);
                    }
                }
            }
            // Without a later bucket of its own the latest may take more of
            // its time: it is the one being filled (the coarser sizes made
            // next see only the closed ones, so nothing counts twice).
            let filling = (live && first == i64::MAX).then(|| older.pop()).flatten();
            for bucket in older {
                made.push((tier, bucket));
                self.restore(tier, bucket, false);
            }
            if let Some(bucket) = filling {
                made.push((tier, bucket));
                self.filling[tier.level()] = Some(bucket);
            }
        }
        made
    }

    /// Whether any bucket of any size is held.
    pub(crate) fn is_empty(&self) -> bool {
        self.kept.iter().all(VecDeque::is_empty) && self.filling.iter().all(Option::is_none)
    }

    /// Drops every bucket (a run listed with its figures only).
    pub(crate) fn clear(&mut self) {
        *self = Self::new(self.shift_ns);
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
        for tier in Tier::ALL {
            let over = self.filling[tier.level()].is_some_and(|bucket| {
                let step = i64::try_from(tier.step_ns()).unwrap_or(i64::MAX);
                utc_ns >= bucket.start_utc_ns.saturating_add(step)
            });
            if over {
                self.close(tier, closed);
            }
        }
    }

    /// Closes the bucket of `tier` being filled now, whatever its time,
    /// and merges it into the sizes it is the source of.
    fn close(&mut self, tier: Tier, closed: &mut Vec<(Tier, RunBucket)>) {
        let Some(bucket) = self.filling[tier.level()].take() else {
            return;
        };
        for parent in Tier::ALL
            .into_iter()
            .filter(|parent| parent.source() == Some(tier))
        {
            let start = parent.align(bucket.start_utc_ns, self.shift_ns);
            match &mut self.filling[parent.level()] {
                Some(filling) if filling.start_utc_ns == start => filling.merge(&bucket),
                _ => {
                    self.close(parent, closed);
                    let mut fresh = RunBucket::new(start);
                    fresh.merge(&bucket);
                    self.filling[parent.level()] = Some(fresh);
                }
            }
        }
        if tier != Tier::Recent {
            closed.push((tier, bucket));
        }
        let kept = &mut self.kept[tier.level()];
        kept.push_back(bucket);
        trim(kept, tier.capacity());
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
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;

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
            .buckets(Tier::OneMinute)
            .map(|bucket| bucket.start_utc_ns / SECOND)
            .collect();
        assert_eq!(
            minutes,
            vec![0, 60],
            "a minute fills as its 5 s buckets close"
        );
        let first = series.buckets(Tier::OneMinute).next().unwrap();
        assert_eq!(first.value(RunMetric::Cpu).count, 60);
        assert_eq!(first.value(RunMetric::Cpu).mean(), Some(50.0));
        assert_eq!(first.value(RunMetric::Cpu).peak(), Some(100.0));
        let tiers: Vec<(Tier, i64)> = closed
            .iter()
            .map(|(tier, bucket)| (*tier, bucket.start_utc_ns / SECOND))
            .collect();
        assert_eq!(tiers, vec![(Tier::OneMinute, 0), (Tier::OneMinute, 60)]);
    }

    /// Every size gets every sample once, through its source: 18 hours
    /// through 6, a day through 12.
    #[test]
    fn every_size_holds_every_sample_once() {
        let mut series = RunSeries::default();
        // Two days, a sample every 30 s.
        for step in 0..(2 * 24 * 120) {
            series.add(step * 30 * SECOND, &sample(1.0));
        }
        series.close_all();
        for tier in Tier::ALL.into_iter().skip(1) {
            let count: u32 = series
                .buckets(tier)
                .map(|bucket| bucket.value(RunMetric::Cpu).count)
                .sum();
            let held = series.buckets(tier).count();
            let expected = if held < tier.capacity() {
                2 * 24 * 120
            } else {
                count
            };
            assert_eq!(count, expected, "{tier:?}");
        }
        assert_eq!(series.buckets(Tier::OneDay).count(), 2);
        assert_eq!(
            series.buckets(Tier::EighteenHours).count(),
            3,
            "0, 18, 36 h"
        );
        assert_eq!(
            series.buckets(Tier::OneMinute).count(),
            TIER_BUCKETS_FOR_TEST,
            "the latest only"
        );
    }

    const TIER_BUCKETS_FOR_TEST: usize = dgxtop_core::process_history::TIER_BUCKETS;

    /// Buckets follow the local clock: at UTC+8 a day closes at 16:00 UTC.
    #[test]
    fn buckets_follow_the_local_clock() {
        let mut series = RunSeries::new(8 * HOUR);
        series.add(15 * HOUR, &sample(1.0));
        let closed = series.add(16 * HOUR + SECOND, &sample(1.0));
        assert!(
            closed
                .iter()
                .any(|(tier, bucket)| *tier == Tier::OneDay && bucket.start_utc_ns == -8 * HOUR),
            "{closed:?}"
        );
    }

    #[test]
    fn closing_all_flushes_every_size_once() {
        let mut series = RunSeries::default();
        series.add(0, &sample(10.0));
        series.add(7 * SECOND, &sample(30.0));
        let closed = series.close_all();
        let tiers: Vec<Tier> = closed.iter().map(|(tier, _)| *tier).collect();
        assert_eq!(tiers, Tier::ALL[1..].to_vec());
        assert_eq!(closed[0].1.value(RunMetric::Cpu).mean(), Some(20.0));
        assert!(series.filling().is_empty());
        assert!(series.close_all().is_empty(), "nothing left to close");
        assert_eq!(cpu_means(&series, Tier::OneDay), vec![(0, 20.0)]);
    }

    /// A gap longer than a bucket (dgxtop suspended) closes the old buckets
    /// and starts new ones where the samples resume.
    #[test]
    fn a_long_gap_closes_buckets_without_bridging() {
        let mut series = RunSeries::default();
        series.add(0, &sample(10.0));
        let closed = series.add(HOUR + 3 * SECOND, &sample(90.0));
        assert_eq!(
            closed.iter().map(|(tier, _)| *tier).collect::<Vec<_>>(),
            vec![
                Tier::OneMinute,
                Tier::FiveMinutes,
                Tier::TenMinutes,
                Tier::ThirtyMinutes,
                Tier::OneHour
            ]
        );
        assert_eq!(
            cpu_means(&series, Tier::Recent),
            vec![(0, 10.0), (3_600, 90.0)]
        );
        assert_eq!(cpu_means(&series, Tier::OneHour), vec![(0, 10.0)]);
        assert_eq!(cpu_means(&series, Tier::TwoHours), vec![(0, 10.0)]);
    }

    #[test]
    fn kept_buckets_are_bounded_and_restored_in_order() {
        let mut series = RunSeries::default();
        let minutes = Tier::OneMinute.capacity();
        for minute in 0..(i64::try_from(minutes).unwrap() + 10) {
            series.add(minute * MINUTE, &sample(1.0));
        }
        assert_eq!(series.kept[1].len(), minutes, "the latest minutes");
        let mut restored = RunSeries::default();
        for start in [120, 0, 60, 60] {
            let mut bucket = RunBucket::new(start * SECOND);
            bucket.add(
                RunMetric::Cpu,
                Some(f64::from(u8::try_from(start / 60).unwrap())),
            );
            restored.restore(Tier::OneMinute, bucket, false);
        }
        assert_eq!(
            cpu_means(&restored, Tier::OneMinute),
            vec![(0, 0.0), (60, 1.0), (120, 2.0)],
            "ordered, a repeated start replaced"
        );
        let mut filling = RunBucket::new(180 * SECOND);
        filling.add(RunMetric::Cpu, Some(5.0));
        restored.restore(Tier::OneMinute, filling, true);
        assert_eq!(restored.len(), 4);
        restored.clear();
        assert!(restored.is_empty());
    }

    /// A version 2 run kept minutes and ten minutes only: the backfill
    /// makes the other sizes from them, each from its source.
    #[test]
    fn a_restore_backfills_the_sizes_it_lacks() {
        let mut series = RunSeries::default();
        for start in 0..36 {
            let mut bucket = RunBucket::new(start * 10 * MINUTE);
            bucket.add(RunMetric::Cpu, Some(2.0));
            series.restore(Tier::TenMinutes, bucket, false);
        }
        let made = series.backfill(true);
        assert_eq!(cpu_means(&series, Tier::ThirtyMinutes).len(), 12);
        assert_eq!(cpu_means(&series, Tier::OneHour).len(), 6);
        assert_eq!(cpu_means(&series, Tier::SixHours), vec![(0, 2.0)]);
        // The day has none of its own yet: what its sources fill is it.
        let day: u32 = series
            .buckets(Tier::OneDay)
            .chain(series.unmerged(Tier::OneDay))
            .map(|bucket| bucket.value(RunMetric::Cpu).count)
            .sum();
        assert_eq!(day, 36);
        assert!(made.iter().any(|(tier, _)| *tier == Tier::SixHours));
        assert!(series.backfill(true).is_empty(), "once");
        // The next ten minutes join the hour and the day being filled.
        series.add(360 * MINUTE + SECOND, &sample(4.0));
        let closed = series.add(370 * MINUTE + SECOND, &sample(4.0));
        assert!(closed.iter().all(|(tier, _)| *tier <= Tier::TenMinutes));
        let closed = series.close_all();
        let hours: Vec<i64> = series
            .buckets(Tier::SixHours)
            .map(|bucket| bucket.start_utc_ns / HOUR)
            .collect();
        assert_eq!(hours, vec![0, 6]);
        let day: Vec<&RunBucket> = series.buckets(Tier::OneDay).collect();
        assert_eq!(day.len(), 1, "one day, not two");
        assert_eq!(day[0].value(RunMetric::Cpu).count, 38);
        assert!(closed.iter().any(|(tier, _)| *tier == Tier::OneDay));
    }

    /// An ended run's buckets are all closed: every size takes all of them.
    #[test]
    fn an_ended_run_backfills_closed_buckets() {
        let mut series = RunSeries::default();
        let mut bucket = RunBucket::new(0);
        bucket.add(RunMetric::Cpu, Some(1.0));
        series.restore(Tier::OneMinute, bucket, false);
        let made = series.backfill(false);
        assert_eq!(made.len(), Tier::ALL.len() - 2);
        assert!(series.filling().is_empty());
        assert_eq!(cpu_means(&series, Tier::OneDay), vec![(0, 1.0)]);
    }
}
