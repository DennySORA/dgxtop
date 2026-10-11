//! One series in every bucket size (D-76, D-77, D-79). A sample lands in
//! the 5 s bucket being filled; when a bucket's time is over it closes,
//! joins the series' kept buckets of its size (the latest
//! [`Tier::capacity`]) and merges into the bucket being filled of each size
//! it is the source of ([`Tier::source`]). Closed buckets of a minute and
//! more are handed back for the long-term store. Buckets follow the local
//! clock (`shift_ns`), so a day's bucket is a local day.
//!
//! A series can also be fed whole buckets of a size ([`TieredSeries::add_bucket`]:
//! the Overview's finalized minutes, D-82) and continue after a restore of
//! closed buckets ([`TieredSeries::resume`]).

use std::collections::VecDeque;

use dgxtop_core::tiers::Tier;

use crate::bucket::{Bucket, merged_at, regroup_into};

const LEVELS: usize = Tier::ALL.len();

/// A series' buckets of every size, generic over what a bucket holds.
#[derive(Debug, Clone)]
pub struct TieredSeries<B> {
    kept: [VecDeque<B>; LEVELS],
    filling: [Option<B>; LEVELS],
    /// Added to UTC before aligning a bucket: the local offset.
    shift_ns: i64,
}

impl<B: Bucket> Default for TieredSeries<B> {
    fn default() -> Self {
        Self::new(0)
    }
}

impl<B: Bucket> TieredSeries<B> {
    pub fn new(shift_ns: i64) -> Self {
        Self {
            kept: std::array::from_fn(|_| VecDeque::new()),
            filling: [None; LEVELS],
            shift_ns,
        }
    }

    pub fn shift_ns(&self) -> i64 {
        self.shift_ns
    }

    /// Lets `fill` add a sample taken at `utc_ns` to the 5 s bucket that
    /// holds it; returns the buckets of a minute and more it closed (for
    /// the long-term store).
    pub fn record(&mut self, utc_ns: i64, fill: impl FnOnce(&mut B)) -> Vec<(Tier, B)> {
        let mut closed = Vec::new();
        self.close_before(utc_ns, &mut closed);
        let start = Tier::Recent.align(utc_ns, self.shift_ns);
        fill(self.filling[0].get_or_insert_with(|| B::new(start)));
        closed
    }

    /// Adds a whole bucket of `tier` (one that started at
    /// `bucket.start_utc_ns`, aligned or not) to the bucket of `tier` being
    /// filled at its aligned start; returns the buckets of a minute and
    /// more it closed. Pieces of one bucket (two sources in one minute)
    /// merge. Buckets come in time order: one older than the bucket being
    /// filled is left out.
    pub fn add_bucket(&mut self, tier: Tier, bucket: &B) -> Vec<(Tier, B)> {
        let mut closed = Vec::new();
        let start = tier.align(bucket.start_utc_ns(), self.shift_ns);
        let level = tier.level();
        if self.filling[level].is_some_and(|filling| start < filling.start_utc_ns())
            || self.kept[level]
                .back()
                .is_some_and(|last| start <= last.start_utc_ns())
        {
            return closed;
        }
        self.close_before(start, &mut closed);
        match &mut self.filling[level] {
            Some(filling) => filling.merge(bucket),
            None => self.filling[level] = Some(merged_at(start, bucket)),
        }
        closed
    }

    /// After a restore of closed buckets only: each size takes its source's
    /// closed buckets that came after its own latest bucket (what it was
    /// filling when the series was stored, and any it never closed), so the
    /// series goes on where it stopped. All but the latest become closed
    /// buckets, returned for the store; the latest is being filled.
    pub fn resume(&mut self) -> Vec<(Tier, B)> {
        let mut made = Vec::new();
        for tier in Tier::ALL {
            let Some(source) = tier.source() else {
                continue;
            };
            let level = tier.level();
            if self.filling[level].is_some() {
                continue;
            }
            let after = self.kept[level].back().map_or(i64::MIN, |last| {
                last.start_utc_ns().saturating_add(tier.step_i64())
            });
            let mut groups: Vec<B> = Vec::new();
            regroup_into(
                &mut groups,
                tier,
                self.shift_ns,
                self.kept[source.level()]
                    .iter()
                    .filter(|bucket| bucket.start_utc_ns() >= after),
            );
            self.filling[level] = groups.pop();
            for bucket in groups {
                made.push((tier, bucket));
                self.kept[level].push_back(bucket);
                self.keep_capacity(tier);
            }
        }
        made
    }

    /// From when `tier` holds everything it was given: a size at its
    /// capacity may have let older buckets go (they live on in the coarser
    /// sizes), one below it holds all (`i64::MIN`).
    pub fn held_from(&self, tier: Tier) -> i64 {
        let kept = &self.kept[tier.level()];
        if kept.len() < tier.capacity() {
            return i64::MIN;
        }
        kept.front().map_or(i64::MIN, Bucket::start_utc_ns)
    }

    /// Whether `tier` holds the time at `utc_ns`: it was given buckets and
    /// let none of that time go.
    pub(crate) fn holds(&self, tier: Tier, utc_ns: i64) -> bool {
        let level = tier.level();
        let given = !self.kept[level].is_empty() || self.filling[level].is_some();
        given && utc_ns >= self.held_from(tier)
    }

    /// The kept bucket of `tier` starting at `start`, if any.
    pub(crate) fn kept_at(&self, tier: Tier, start: i64) -> Option<&B> {
        let kept = &self.kept[tier.level()];
        let at = kept.partition_point(|bucket| bucket.start_utc_ns() < start);
        kept.get(at).filter(|bucket| bucket.start_utc_ns() == start)
    }

    /// The bucket of `tier` being filled.
    pub(crate) fn filling_of(&self, tier: Tier) -> Option<&B> {
        self.filling[tier.level()].as_ref()
    }

    /// Closes every bucket being filled, as when the series ends.
    pub fn close_all(&mut self) -> Vec<(Tier, B)> {
        let mut closed = Vec::new();
        self.close_before(i64::MAX, &mut closed);
        closed
    }

    /// The buckets of `tier` kept, oldest first, then the one being filled.
    pub fn buckets(&self, tier: Tier) -> impl Iterator<Item = &B> {
        let level = tier.level();
        self.kept[level].iter().chain(self.filling[level].as_ref())
    }

    /// What `tier` has not merged yet: the buckets being filled of its
    /// source, the source's source and so on.
    pub fn unmerged(&self, tier: Tier) -> impl Iterator<Item = &B> {
        std::iter::successors(tier.source(), |source| source.source())
            .filter_map(|source| self.filling[source.level()].as_ref())
    }

    /// The buckets being filled, for the short-term file.
    pub fn filling(&self) -> Vec<(Tier, B)> {
        Tier::ALL
            .iter()
            .zip(&self.filling)
            .filter_map(|(tier, bucket)| bucket.map(|bucket| (*tier, bucket)))
            .collect()
    }

    /// Puts back stored buckets (startup): closed ones join the kept
    /// buckets in time order, filling ones resume.
    pub fn restore(&mut self, tier: Tier, bucket: B, filling: bool) {
        let level = tier.level();
        if filling {
            self.filling[level] = Some(bucket);
            return;
        }
        let start = bucket.start_utc_ns();
        let kept = &mut self.kept[level];
        let at = kept.partition_point(|old| old.start_utc_ns() < start);
        if kept.get(at).is_some_and(|old| old.start_utc_ns() == start) {
            kept[at] = bucket;
        } else {
            kept.insert(at, bucket);
        }
        self.keep_capacity(tier);
    }

    /// After a restore: each size takes what its source holds from before
    /// its own first bucket (a size added since those buckets were stored,
    /// or one whose buckets were lost), so every size reaches back as far
    /// as the finer ones do. A `live` series' sources are still filling:
    /// what they fill reaches each size when it closes. Returns the buckets
    /// it made, for the store.
    pub fn backfill(&mut self, live: bool) -> Vec<(Tier, B)> {
        let mut made = Vec::new();
        for tier in Tier::ALL {
            let Some(source) = tier.source().filter(|source| *source != Tier::Recent) else {
                continue;
            };
            let first = self
                .buckets(tier)
                .next()
                .map_or(i64::MAX, Bucket::start_utc_ns);
            let older = self.regrouped_before(source, tier, first, live);
            self.take_backfilled(tier, older, live && first == i64::MAX, &mut made);
        }
        made
    }

    /// `source`'s buckets before `first` (a `live` series' filling one
    /// left out), merged into buckets of `tier`.
    fn regrouped_before(&self, source: Tier, tier: Tier, first: i64, live: bool) -> Vec<B> {
        let mut older: Vec<B> = Vec::new();
        let source_filling = self.filling[source.level()].filter(|_| !live);
        regroup_into(
            &mut older,
            tier,
            self.shift_ns,
            self.kept[source.level()]
                .iter()
                .chain(source_filling.as_ref())
                .filter(|bucket| bucket.start_utc_ns() < first),
        );
        older
    }

    /// Keeps the backfilled `older` buckets of `tier`. Without a later
    /// bucket of its own (`latest_fills`) the latest may take more of its
    /// time: it is the one being filled (the coarser sizes made next see
    /// only the closed ones, so nothing counts twice).
    fn take_backfilled(
        &mut self,
        tier: Tier,
        mut older: Vec<B>,
        latest_fills: bool,
        made: &mut Vec<(Tier, B)>,
    ) {
        let filling = latest_fills.then(|| older.pop()).flatten();
        for bucket in older {
            made.push((tier, bucket));
            self.restore(tier, bucket, false);
        }
        if let Some(bucket) = filling {
            made.push((tier, bucket));
            self.filling[tier.level()] = Some(bucket);
        }
    }

    /// Whether no bucket of any size is held.
    pub fn is_empty(&self) -> bool {
        self.kept.iter().all(VecDeque::is_empty) && self.filling.iter().all(Option::is_none)
    }

    /// Buckets held, all sizes.
    pub fn len(&self) -> usize {
        self.kept.iter().map(VecDeque::len).sum::<usize>()
            + self
                .filling
                .iter()
                .filter(|bucket| bucket.is_some())
                .count()
    }

    /// Drops every bucket (a series listed with its figures only).
    pub fn clear(&mut self) {
        *self = Self::new(self.shift_ns);
    }

    /// Closes, finest first, each bucket whose time is over at `utc_ns`.
    fn close_before(&mut self, utc_ns: i64, closed: &mut Vec<(Tier, B)>) {
        for tier in Tier::ALL {
            let over = self.filling[tier.level()].is_some_and(|bucket| {
                let step = tier.step_i64();
                utc_ns >= bucket.start_utc_ns().saturating_add(step)
            });
            if over {
                self.close(tier, closed);
            }
        }
    }

    /// Closes the bucket of `tier` being filled now, whatever its time,
    /// and merges it into the sizes it is the source of.
    fn close(&mut self, tier: Tier, closed: &mut Vec<(Tier, B)>) {
        let Some(bucket) = self.filling[tier.level()].take() else {
            return;
        };
        for parent in Tier::ALL
            .into_iter()
            .filter(|parent| parent.source() == Some(tier))
        {
            let start = parent.align(bucket.start_utc_ns(), self.shift_ns);
            match &mut self.filling[parent.level()] {
                Some(filling) if filling.start_utc_ns() == start => filling.merge(&bucket),
                _ => {
                    self.close(parent, closed);
                    self.filling[parent.level()] = Some(merged_at(start, &bucket));
                }
            }
        }
        if tier != Tier::Recent {
            closed.push((tier, bucket));
        }
        self.kept[tier.level()].push_back(bucket);
        self.keep_capacity(tier);
    }

    /// Lets the oldest buckets of `tier` go past its capacity.
    fn keep_capacity(&mut self, tier: Tier) {
        let kept = &mut self.kept[tier.level()];
        while kept.len() > tier.capacity() {
            kept.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use dgxtop_core::process_history::{RUN_METRICS, RunBucket, RunMetric};
    use dgxtop_core::tiers::TIER_BUCKETS;

    use super::*;
    use crate::bucket::Tally;
    use crate::run::{RunSample, RunSeries};

    const SECOND: i64 = 1_000_000_000;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;

    fn sample(cpu: f64) -> RunSample {
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
            TIER_BUCKETS,
            "the latest only"
        );
    }

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
        assert_eq!(restored.len(), 0);
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

    fn count_one(tally: &mut Tally) {
        tally.samples += 1;
    }

    fn tallies(series: &TieredSeries<Tally>, tier: Tier) -> Vec<(i64, u32)> {
        series
            .buckets(tier)
            .map(|tally| (tally.start_utc_ns / SECOND, tally.samples))
            .collect()
    }

    /// The algorithm works for any bucket: a bucket that only counts.
    #[test]
    fn any_bucket_type_rolls_up_through_every_size() {
        let mut series = TieredSeries::<Tally>::default();
        assert!(series.is_empty());
        let mut closed = Vec::new();
        for second in 0..(2 * 3_600) {
            closed.extend(series.record(second * SECOND, count_one));
        }
        closed.extend(series.close_all());
        assert_eq!(
            tallies(&series, Tier::OneHour),
            vec![(0, 3_600), (3_600, 3_600)]
        );
        assert_eq!(tallies(&series, Tier::OneDay), vec![(0, 7_200)]);
        let minutes: u32 = closed
            .iter()
            .filter(|(tier, _)| *tier == Tier::OneMinute)
            .map(|(_, tally)| tally.samples)
            .sum();
        assert_eq!(minutes, 7_200, "every sample handed to the store once");
        assert!(closed.iter().all(|(tier, _)| *tier != Tier::Recent));
    }

    /// Restoring and backfilling a generic bucket: a repeated start is
    /// replaced, and every coarser size is made from the restored minutes.
    #[test]
    fn any_bucket_type_restores_and_backfills() {
        let mut series = TieredSeries::<Tally>::new(0);
        for (start, samples) in [(60, 2), (0, 1), (60, 3)] {
            let tally = Tally {
                start_utc_ns: start * SECOND,
                samples,
            };
            series.restore(Tier::OneMinute, tally, false);
        }
        assert_eq!(tallies(&series, Tier::OneMinute), vec![(0, 1), (60, 3)]);
        let made = series.backfill(false);
        assert_eq!(made.len(), Tier::ALL.len() - 2, "one bucket per size");
        assert_eq!(tallies(&series, Tier::OneDay), vec![(0, 4)]);
        assert!(series.backfill(false).is_empty(), "nothing left to make");
    }

    /// A restored bucket of a size past its capacity drops the oldest.
    #[test]
    fn a_restore_past_capacity_keeps_the_latest() {
        let mut series = TieredSeries::<Tally>::default();
        let capacity = i64::try_from(Tier::Recent.capacity()).unwrap();
        for start in 0..=capacity {
            let tally = Tally {
                start_utc_ns: start * 5 * SECOND,
                samples: 1,
            };
            series.restore(Tier::Recent, tally, false);
        }
        let kept = tallies(&series, Tier::Recent);
        assert_eq!(kept.len(), Tier::Recent.capacity());
        assert_eq!(kept[0].0, 5, "the first one gave way");
    }

    fn minute_tally(minute: i64, samples: u32) -> Tally {
        Tally {
            start_utc_ns: minute * MINUTE + 17 * SECOND,
            samples,
        }
    }

    /// Whole minutes feed every size once, each at its aligned start; a
    /// second piece of a minute merges, an older minute is left out.
    #[test]
    fn closed_minutes_feed_every_size() {
        let mut series = TieredSeries::<Tally>::new(0);
        let mut closed = Vec::new();
        for minute in 0..(3 * 60) {
            closed.extend(series.add_bucket(Tier::OneMinute, &minute_tally(minute, 1)));
        }
        closed.extend(series.add_bucket(Tier::OneMinute, &minute_tally(179, 2)));
        assert!(
            series
                .add_bucket(Tier::OneMinute, &minute_tally(100, 5))
                .is_empty()
        );
        assert_eq!(
            tallies(&series, Tier::OneHour),
            vec![(0, 60), (3_600, 60), (7_200, 30)],
            "the hour being filled holds its closed half hours"
        );
        assert_eq!(series_minutes(&series), 182);
        let minutes: Vec<i64> = series
            .buckets(Tier::OneMinute)
            .map(|tally| tally.start_utc_ns / SECOND)
            .collect();
        assert_eq!(minutes[0], 0, "aligned to the minute");
        assert_eq!(
            series.filling()[0].0,
            Tier::OneMinute,
            "the latest minute fills"
        );
        let stored: u32 = closed
            .iter()
            .filter(|(tier, _)| *tier == Tier::OneMinute)
            .map(|(_, tally)| tally.samples)
            .sum();
        assert_eq!(stored, 179, "every closed minute handed to the store once");
    }

    /// A series stored as its closed buckets and restored goes on where it
    /// stopped: the sizes it was filling are rebuilt from their sources,
    /// and feeding on gives what a series never restarted holds.
    #[test]
    fn a_restored_series_resumes_where_it_stopped() {
        let mut original = TieredSeries::<Tally>::new(0);
        let mut stored = Vec::new();
        for minute in 0..(26 * 60 + 7) {
            stored.extend(original.add_bucket(Tier::OneMinute, &minute_tally(minute, 1)));
        }
        let mut restored = TieredSeries::<Tally>::new(0);
        for (tier, tally) in &stored {
            restored.restore(*tier, *tally, false);
        }
        let made = restored.resume();
        assert!(made.is_empty(), "nothing was left unclosed: {made:?}");
        // The minute being filled was not stored: it comes again (from the
        // archive), then both go on alike.
        restored.add_bucket(Tier::OneMinute, &minute_tally(26 * 60 + 6, 1));
        for minute in (26 * 60 + 7)..(50 * 60) {
            original.add_bucket(Tier::OneMinute, &minute_tally(minute, 1));
            restored.add_bucket(Tier::OneMinute, &minute_tally(minute, 1));
        }
        for tier in Tier::ALL.into_iter().skip(1) {
            assert_eq!(
                tallies(&restored, tier),
                tallies(&original, tier),
                "{tier:?}"
            );
        }
        assert_eq!(series_minutes(&restored), 50 * 60);
        assert!(
            restored.resume().is_empty(),
            "a live series resumes nothing"
        );
    }

    /// Buckets a size never closed (the series was stored while they were
    /// filling, then time went on) are made from their sources at resume.
    #[test]
    fn a_resume_closes_what_a_size_never_closed() {
        let mut restored = TieredSeries::<Tally>::new(0);
        for minute in 0..130 {
            restored.restore(Tier::OneMinute, minute_tally(minute, 1), false);
        }
        let made = restored.resume();
        let hours: Vec<(i64, u32)> = made
            .iter()
            .filter(|(tier, _)| *tier == Tier::OneHour)
            .map(|(_, tally)| (tally.start_utc_ns / SECOND, tally.samples))
            .collect();
        assert_eq!(hours, vec![(0, 60)], "the hour that closed");
        assert_eq!(
            tallies(&restored, Tier::OneHour),
            vec![(0, 60), (3_600, 30)],
            "the latest hour fills with its closed half hours"
        );
        assert_eq!(series_minutes(&restored), 130, "every minute once");
    }

    fn series_minutes(series: &TieredSeries<Tally>) -> u32 {
        series
            .buckets(Tier::OneDay)
            .chain(series.unmerged(Tier::OneDay))
            .map(|tally| tally.samples)
            .sum()
    }

    /// A size at its capacity holds only from its oldest bucket on.
    #[test]
    fn a_full_size_holds_from_its_oldest_bucket() {
        let mut series = TieredSeries::<Tally>::new(0);
        assert_eq!(series.held_from(Tier::OneMinute), i64::MIN);
        assert!(!series.holds(Tier::OneMinute, 0), "nothing given yet");
        for minute in 0..400 {
            series.add_bucket(Tier::OneMinute, &minute_tally(minute, 1));
        }
        let from = series.held_from(Tier::OneMinute);
        assert_eq!(from, (399 - 300) * MINUTE);
        assert!(series.holds(Tier::OneMinute, from));
        assert!(!series.holds(Tier::OneMinute, from - 1));
        assert!(series.holds(Tier::OneHour, 0));
        assert!(series.kept_at(Tier::OneHour, HOUR).is_some());
        assert!(series.kept_at(Tier::OneHour, HOUR + 1).is_none());
    }
}
