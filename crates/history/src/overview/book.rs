//! The history's in-memory cache of the Overview series (D-82): per series
//! a [`TieredSeries`] of [`OverviewBucket`]s of every bar size, fed by the
//! minutes the history finalizes, restored from the store at startup and
//! then kept incrementally. Every chart and statistics read is answered
//! from here.
//!
//! Bounded: at most [`MAX_TIER_SERIES`] series, each at most a full set of
//! sizes (about 200 KiB, so about 32 MiB in all); a new series beyond it
//! takes the place of the one fed longest ago. While the store's startup load runs, a series'
//! live minutes wait ([`MAX_PENDING_MINUTES`] at most) so that its earlier
//! history goes in first.

use std::collections::{BTreeMap, VecDeque};

use dgxtop_compression::{TieredSeries, bars_in, merged_between};
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::MetricKey;
use dgxtop_core::overview_history::{ArchivePrune, OverviewBars, OverviewBucket, TierRow};
use dgxtop_core::process_history::Tier;

/// Series the cache holds at most: the host's charts and every GPU's,
/// disk's and interface's with room to spare.
pub(crate) const MAX_TIER_SERIES: usize = 160;
/// The most one series holds: every size full, plus what each fills.
#[cfg(test)]
pub(crate) const MAX_SERIES_BYTES: usize = (Tier::ALL.len() - 1)
    * (dgxtop_core::process_history::TIER_BUCKETS + 1)
    * std::mem::size_of::<OverviewBucket>();
/// Live minutes a series keeps while its earlier history is being read.
pub(crate) const MAX_PENDING_MINUTES: usize = 240;

/// A finalized minute of an Overview series, on the wall clock.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TierMinute {
    pub(crate) series: SeriesInEveryBoot,
    /// This boot's key of the series (its raw history is read by it).
    pub(crate) key: MetricKey,
    pub(crate) bucket: OverviewBucket,
}

/// One series in every size.
#[derive(Debug)]
pub(crate) struct TierEntry {
    tiers: TieredSeries<OverviewBucket>,
    /// Live minutes waiting for the series' earlier history.
    pending: VecDeque<OverviewBucket>,
    ready: bool,
    /// Its archived minutes are in the sizes: the archive may let the ones
    /// the minutes size no longer holds go.
    backfilled: bool,
    /// Changes when its past changed under a reader's cached bars.
    epoch: u64,
    /// End of the latest minute it was given.
    fed_through_utc_ns: i64,
    /// The archive was asked to delete its minutes before this.
    pruned_before_utc_ns: i64,
    key: Option<MetricKey>,
}

impl TierEntry {
    fn new(shift_ns: i64, ready: bool, backfilled: bool) -> Self {
        Self {
            tiers: TieredSeries::new(shift_ns),
            pending: VecDeque::new(),
            ready,
            backfilled,
            epoch: 0,
            fed_through_utc_ns: i64::MIN,
            pruned_before_utc_ns: i64::MIN,
            key: None,
        }
    }

    /// The end of the latest minute the minutes size holds, if any.
    pub(crate) fn minutes_through_utc_ns(&self) -> Option<i64> {
        self.tiers
            .buckets(Tier::OneMinute)
            .last()
            .map(|minute| minute.start_utc_ns.saturating_add(minute_ns()))
    }
}

fn minute_ns() -> i64 {
    i64::try_from(Tier::OneMinute.step_ns()).unwrap_or(i64::MAX)
}

/// Every Overview series' sizes.
#[derive(Debug)]
pub(crate) struct TierBook {
    shift_ns: i64,
    entries: BTreeMap<SeriesInEveryBoot, TierEntry>,
    /// New series wait for the store's startup load.
    waiting: bool,
    /// A series first seen after the startup has no archived minutes the
    /// startup did not list (when it could list them).
    new_backfilled: bool,
    /// Closed buckets collected for the store (only while one is kept).
    unsent: Vec<TierRow>,
    keeps_rows: bool,
}

impl TierBook {
    pub(crate) fn new() -> Self {
        Self {
            shift_ns: 0,
            entries: BTreeMap::new(),
            waiting: false,
            new_backfilled: false,
            unsent: Vec::new(),
            keeps_rows: false,
        }
    }

    /// Buckets follow the local clock `shift_ns` east of UTC. Set before
    /// any minute is fed.
    pub(crate) fn set_shift(&mut self, shift_ns: i64) {
        self.shift_ns = shift_ns;
    }

    /// From now on series wait for the store's startup load and closed
    /// buckets are collected for it.
    pub(crate) fn keep_on_disk(&mut self) {
        self.waiting = true;
        self.keeps_rows = true;
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Takes the closed buckets collected for the store.
    pub(crate) fn take_unsent(&mut self) -> Vec<TierRow> {
        std::mem::take(&mut self.unsent)
    }

    pub(crate) fn unsent_len(&self) -> usize {
        self.unsent.len()
    }

    /// Puts back rows the store could not take yet, before the ones
    /// collected since.
    pub(crate) fn return_unsent(&mut self, mut rows: Vec<TierRow>) {
        rows.append(&mut self.unsent);
        self.unsent = rows;
    }

    /// Drops the oldest collected rows past `keep` (the store fell
    /// behind); returns whether any went.
    pub(crate) fn shed_unsent(&mut self, keep: usize) -> bool {
        let excess = self.unsent.len().saturating_sub(keep);
        self.unsent.drain(..excess);
        excess > 0
    }

    /// A finalized minute: into the sizes, or waiting while the series'
    /// earlier history is read.
    pub(crate) fn feed(&mut self, minute: TierMinute) {
        let TierMinute {
            series,
            key,
            bucket,
        } = minute;
        let entry = self.entry(&series);
        entry.key = Some(key);
        if entry.ready {
            self.add(&series, &bucket);
            return;
        }
        let entry = self.entry(&series);
        if entry.pending.len() >= MAX_PENDING_MINUTES {
            entry.pending.pop_front();
        }
        entry.pending.push_back(bucket);
    }

    /// The series' entry, made (waiting while the store loads) when new;
    /// beyond [`MAX_TIER_SERIES`] the one fed longest ago gives way.
    fn entry(&mut self, series: &SeriesInEveryBoot) -> &mut TierEntry {
        if !self.entries.contains_key(series) && self.entries.len() >= MAX_TIER_SERIES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.fed_through_utc_ns)
                .map(|(series, _)| series.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        let (shift, ready, backfilled) = (self.shift_ns, !self.waiting, self.new_backfilled);
        self.entries
            .entry(series.clone())
            .or_insert_with(|| TierEntry::new(shift, ready, backfilled && ready))
    }

    /// Adds one minute to the sizes and collects what it closed.
    fn add(&mut self, series: &SeriesInEveryBoot, minute: &OverviewBucket) {
        let Some(entry) = self.entries.get_mut(series) else {
            return;
        };
        let closed = entry.tiers.add_bucket(Tier::OneMinute, minute);
        let start = Tier::OneMinute.align(minute.start_utc_ns, self.shift_ns);
        entry.fed_through_utc_ns = entry
            .fed_through_utc_ns
            .max(start.saturating_add(minute_ns()));
        self.collect(series, closed);
    }

    fn collect(&mut self, series: &SeriesInEveryBoot, closed: Vec<(Tier, OverviewBucket)>) {
        if !self.keeps_rows {
            return;
        }
        self.unsent
            .extend(closed.into_iter().map(|(tier, bucket)| TierRow {
                series: series.clone(),
                tier,
                bucket,
            }));
    }

    /// Puts back what the store kept of a series (startup).
    pub(crate) fn restore(
        &mut self,
        series: &SeriesInEveryBoot,
        buckets: &[(Tier, OverviewBucket)],
    ) {
        let entry = self.entry(series);
        for (tier, bucket) in buckets {
            entry.tiers.restore(*tier, *bucket, false);
        }
        if let Some(through) = entry.minutes_through_utc_ns() {
            entry.fed_through_utc_ns = entry.fed_through_utc_ns.max(through);
        }
    }

    /// Where the archive's minutes the series lacks begin: after the latest
    /// minute it holds.
    pub(crate) fn archive_from_utc_ns(&self, series: &SeriesInEveryBoot) -> i64 {
        self.entries
            .get(series)
            .and_then(TierEntry::minutes_through_utc_ns)
            .unwrap_or(0)
    }

    /// Archived minutes of a series, oldest first, into its sizes. The
    /// first page goes on from what the store kept.
    pub(crate) fn add_archived(
        &mut self,
        series: &SeriesInEveryBoot,
        minutes: &[OverviewBucket],
        first_page: bool,
    ) {
        if first_page {
            self.resume(series);
        }
        for minute in minutes {
            self.add(series, minute);
        }
    }

    /// Rebuilds what the series' sizes were filling when it was stored.
    fn resume(&mut self, series: &SeriesInEveryBoot) {
        let made = self.entry(series).tiers.resume();
        self.collect(series, made);
    }

    /// The series' earlier history is in (or could not be read): its
    /// waiting minutes follow and it is fed live from now on.
    pub(crate) fn finish(&mut self, series: &SeriesInEveryBoot, backfilled: bool, resumed: bool) {
        if !resumed {
            self.resume(series);
        }
        let entry = self.entry(series);
        let pending: Vec<OverviewBucket> = entry.pending.drain(..).collect();
        entry.ready = true;
        entry.backfilled = backfilled;
        entry.epoch = entry.epoch.saturating_add(1);
        for minute in &pending {
            self.add(series, minute);
        }
    }

    /// The store's startup is over: every series still waiting is fed live
    /// (`backfilled` when the archive was listed, so a series it did not
    /// list has no earlier minutes), and new series no longer wait.
    pub(crate) fn release(&mut self, backfilled: bool) {
        self.waiting = false;
        self.new_backfilled = backfilled;
        let waiting: Vec<SeriesInEveryBoot> = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.ready)
            .map(|(series, _)| series.clone())
            .collect();
        for series in waiting {
            self.finish(&series, backfilled, false);
        }
    }

    /// Deletions the archive may make: for every backfilled series, its
    /// minutes the minutes size let go (the coarser sizes hold them).
    pub(crate) fn prunes(&mut self) -> Vec<ArchivePrune> {
        let mut prunes = Vec::new();
        for (series, entry) in &mut self.entries {
            let held_from = entry.tiers.held_from(Tier::OneMinute);
            if !entry.backfilled || held_from <= entry.pruned_before_utc_ns {
                continue;
            }
            entry.pruned_before_utc_ns = held_from;
            prunes.push(ArchivePrune {
                series: series.clone(),
                before_utc_ns: held_from,
            });
        }
        prunes
    }

    /// This boot's key of a series, for its raw history.
    pub(crate) fn key_of(&self, series: &SeriesInEveryBoot) -> Option<MetricKey> {
        self.entries.get(series).and_then(|entry| entry.key.clone())
    }

    /// The bars of `tier` starting in `window`, with `extra` minutes (the
    /// ones not finalized yet) merged into the bars holding them.
    pub(crate) fn bars(
        &self,
        series: &SeriesInEveryBoot,
        tier: Tier,
        window: (i64, i64),
        extra: &[OverviewBucket],
    ) -> OverviewBars {
        let Some(entry) = self.entries.get(series) else {
            return OverviewBars {
                epoch: 0,
                settled_through_utc_ns: i64::MIN,
                bars: merge_extra(Vec::new(), tier, self.shift_ns, window, extra),
                kept_on_disk: false,
                store_issue: None,
            };
        };
        let bars = bars_in(&entry.tiers, tier, window);
        let waiting: Vec<OverviewBucket> = entry
            .pending
            .iter()
            .chain(self.after_fed(entry, extra))
            .copied()
            .collect();
        OverviewBars {
            epoch: entry.epoch,
            settled_through_utc_ns: if entry.ready {
                entry.fed_through_utc_ns
            } else {
                i64::MIN
            },
            bars: merge_extra(bars, tier, self.shift_ns, window, &waiting),
            kept_on_disk: false,
            store_issue: None,
        }
    }

    /// Each span ending at `end`, merged from the aligned buckets that
    /// cover it and the `extra` minutes inside it.
    pub(crate) fn windows(
        &self,
        series: &SeriesInEveryBoot,
        spans_ns: &[u64],
        end: i64,
        extra: &[OverviewBucket],
    ) -> Vec<OverviewBucket> {
        let entry = self.entries.get(series);
        spans_ns
            .iter()
            .map(|span| {
                let start = end.saturating_sub(i64::try_from(*span).unwrap_or(i64::MAX));
                let mut merged = entry.map_or_else(
                    || OverviewBucket::new(start),
                    |entry| merged_between(&entry.tiers, (start, end)),
                );
                let pending = entry.into_iter().flat_map(|entry| entry.pending.iter());
                let unfinalized = entry.map_or(extra.iter().collect::<Vec<_>>(), |entry| {
                    self.after_fed(entry, extra).collect()
                });
                for minute in pending.chain(unfinalized) {
                    let at = Tier::OneMinute.align(minute.start_utc_ns, self.shift_ns);
                    if at >= start && at < end {
                        merged.merge(minute);
                    }
                }
                merged
            })
            .collect()
    }

    /// The `extra` minutes after the latest one the series was given (a
    /// minute finalized meanwhile is not counted twice).
    fn after_fed<'a>(
        &self,
        entry: &TierEntry,
        extra: &'a [OverviewBucket],
    ) -> impl Iterator<Item = &'a OverviewBucket> {
        let (shift, fed_through) = (self.shift_ns, entry.fed_through_utc_ns);
        extra
            .iter()
            .filter(move |minute| Tier::OneMinute.align(minute.start_utc_ns, shift) >= fed_through)
    }

    #[cfg(test)]
    pub(crate) fn entry_of(&self, series: &SeriesInEveryBoot) -> Option<&TierEntry> {
        self.entries.get(series)
    }

    #[cfg(test)]
    pub(crate) fn tiers_of(
        &self,
        series: &SeriesInEveryBoot,
    ) -> Option<&TieredSeries<OverviewBucket>> {
        self.entries.get(series).map(|entry| &entry.tiers)
    }
}

/// `bars` with each extra minute merged into the bar of `tier` holding it
/// (a new bar where none was), in time order, the window kept.
fn merge_extra(
    mut bars: Vec<OverviewBucket>,
    tier: Tier,
    shift_ns: i64,
    (start, end): (i64, i64),
    extra: &[OverviewBucket],
) -> Vec<OverviewBucket> {
    for minute in extra.iter().filter(|minute| !minute.is_empty()) {
        let at = tier.align(minute.start_utc_ns, shift_ns);
        if at < start || at >= end {
            continue;
        }
        let position = bars.partition_point(|bar| bar.start_utc_ns < at);
        match bars.get_mut(position) {
            Some(bar) if bar.start_utc_ns == at => bar.merge(minute),
            _ => {
                let mut bar = OverviewBucket::new(at);
                bar.merge(minute);
                bars.insert(position, bar);
            }
        }
    }
    bars
}
