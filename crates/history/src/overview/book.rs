//! The history's in-memory cache of the Overview series (D-82): per series
//! a [`TieredSeries`] of [`OverviewBucket`]s of every bar size, fed by the
//! minutes the history finalizes, restored from the store at startup and
//! then kept incrementally. Every chart and statistics read is answered
//! from here.
//!
//! Bounded by bytes (D-95, [`super::budget`]): past its budget the series
//! given a minute longest ago give way ([`TierBook::keep_within_budget`]),
//! and so do as many as the store's budget asks ([`TierBook::give_way`]).
//! A series that gives way is gone: its closed buckets not sent yet go with
//! it and, while a store is kept, the eviction is noted so that the store
//! and the archive delete it too ([`Eviction`]); if it comes back it starts
//! fresh. While the store's startup load runs, a series' live minutes wait
//! ([`MAX_PENDING_BYTES`] of them at most) so that its earlier history
//! goes in first.

use std::collections::{BTreeMap, VecDeque};
use std::mem::size_of;

use dgxtop_compression::{TieredSeries, bars_in, merged_between};
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::MetricKey;
use dgxtop_core::overview_history::{ArchivePrune, OverviewBars, OverviewBucket, TierRow};
use dgxtop_core::tiers::Tier;

use super::budget::{CACHE_BUDGET_BYTES, key_bytes, row_bytes, series_bytes};

/// Bytes of live minutes a series keeps while its earlier history is
/// being read (about four and a half hours of minutes).
pub(crate) const MAX_PENDING_BYTES: usize = 16 * 1024;

/// A finalized minute of an Overview series, on the wall clock.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TierMinute {
    pub(crate) series: SeriesInEveryBoot,
    /// This boot's key of the series (its raw history is read by it).
    pub(crate) key: MetricKey,
    pub(crate) bucket: OverviewBucket,
}

/// A series the cache let go, for the store and the archive to delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Eviction {
    pub(crate) series: SeriesInEveryBoot,
    /// End of the latest minute the cache was given of it (fed, waiting
    /// or restored); `i64::MIN` when none.
    pub(crate) through_utc_ns: i64,
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
            fed_through_utc_ns: i64::MIN,
            pruned_before_utc_ns: i64::MIN,
            key: None,
        }
    }

    /// The end of the latest minute it was given, fed, waiting or
    /// restored (`i64::MIN` when none).
    fn given_through_utc_ns(&self, shift_ns: i64) -> i64 {
        let waiting = self.pending.back().map_or(i64::MIN, |minute| {
            Tier::OneMinute
                .align(minute.start_utc_ns, shift_ns)
                .saturating_add(Tier::OneMinute.step_i64())
        });
        let held = self.minutes_through_utc_ns().unwrap_or(i64::MIN);
        self.fed_through_utc_ns.max(waiting).max(held)
    }

    /// What it costs in memory under `series`: the structs, the buckets
    /// its sizes and waiting minutes have room for, and the strings.
    fn bytes(&self, series: &SeriesInEveryBoot) -> usize {
        let buckets = self.tiers.allocated_buckets() + self.pending.capacity();
        size_of::<(SeriesInEveryBoot, TierEntry)>()
            + series_bytes(series)
            + self.key.as_ref().map_or(0, key_bytes)
            + buckets * size_of::<OverviewBucket>()
    }

    /// The end of the latest minute the minutes size holds, if any.
    pub(crate) fn minutes_through_utc_ns(&self) -> Option<i64> {
        self.tiers.buckets(Tier::OneMinute).last().map(|minute| {
            minute
                .start_utc_ns
                .saturating_add(Tier::OneMinute.step_i64())
        })
    }
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
    /// What they cost in memory ([`row_bytes`]).
    unsent_bytes: usize,
    keeps_rows: bool,
    /// Series let go since the last [`TierBook::take_evictions`] (only
    /// while a store is kept).
    evicted: Vec<Eviction>,
    /// Bytes the entries may take ([`CACHE_BUDGET_BYTES`]).
    budget_bytes: usize,
}

impl TierBook {
    pub(crate) fn new() -> Self {
        Self::with_budget(CACHE_BUDGET_BYTES)
    }

    pub(crate) fn with_budget(budget_bytes: usize) -> Self {
        Self {
            shift_ns: 0,
            entries: BTreeMap::new(),
            waiting: false,
            new_backfilled: false,
            unsent: Vec::new(),
            unsent_bytes: 0,
            keeps_rows: false,
            evicted: Vec::new(),
            budget_bytes,
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
        self.unsent_bytes = 0;
        std::mem::take(&mut self.unsent)
    }

    pub(crate) fn has_unsent(&self) -> bool {
        !self.unsent.is_empty()
    }

    /// What the collected rows cost in memory.
    #[cfg(test)]
    pub(crate) fn unsent_bytes(&self) -> usize {
        self.unsent_bytes
    }

    /// Puts back rows the store could not take yet, before the ones
    /// collected since.
    pub(crate) fn return_unsent(&mut self, mut rows: Vec<TierRow>) {
        self.unsent_bytes += rows.iter().map(row_bytes).sum::<usize>();
        rows.append(&mut self.unsent);
        self.unsent = rows;
    }

    /// Drops the oldest collected rows until they cost at most
    /// `max_bytes` (the store fell behind); returns whether any went.
    pub(crate) fn shed_unsent(&mut self, max_bytes: usize) -> bool {
        let mut cut = 0;
        while self.unsent_bytes > max_bytes && cut < self.unsent.len() {
            self.unsent_bytes -= row_bytes(&self.unsent[cut]);
            cut += 1;
        }
        self.unsent.drain(..cut);
        cut > 0
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
        let minute_bytes = size_of::<OverviewBucket>();
        while (entry.pending.len() + 1) * minute_bytes > MAX_PENDING_BYTES {
            entry.pending.pop_front();
        }
        entry.pending.push_back(bucket);
    }

    /// The series' entry, made (waiting while the store loads) when new.
    fn entry(&mut self, series: &SeriesInEveryBoot) -> &mut TierEntry {
        let (shift, ready, backfilled) = (self.shift_ns, !self.waiting, self.new_backfilled);
        self.entries
            .entry(series.clone())
            .or_insert_with(|| TierEntry::new(shift, ready, backfilled && ready))
    }

    /// What every entry costs in memory.
    pub(crate) fn bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|(series, entry)| entry.bytes(series))
            .sum()
    }

    /// While the entries take more than the budget, the series given a
    /// minute longest ago give way; the last one always stays.
    pub(crate) fn keep_within_budget(&mut self) {
        let mut bytes = self.bytes();
        while bytes > self.budget_bytes && self.entries.len() > 1 {
            let Some(oldest) = self.given_longest_ago() else {
                return;
            };
            bytes = bytes.saturating_sub(self.entries.get(&oldest).map_or(0, |e| e.bytes(&oldest)));
            self.evict(&oldest);
        }
    }

    /// Lets the `count` series given a minute longest ago go (the store's
    /// budget asks for room).
    pub(crate) fn give_way(&mut self, count: usize) {
        for _ in 0..count {
            let Some(oldest) = self.given_longest_ago() else {
                return;
            };
            self.evict(&oldest);
        }
    }

    /// The series whose latest minute (fed, waiting or restored) is the
    /// oldest: a series no longer seen, before any live one.
    fn given_longest_ago(&self) -> Option<SeriesInEveryBoot> {
        self.entries
            .iter()
            .min_by_key(|(_, entry)| entry.given_through_utc_ns(self.shift_ns))
            .map(|(series, _)| series.clone())
    }

    /// Lets a series go: its entry, its rows not sent yet and, while a
    /// store is kept, a note for the store and the archive.
    fn evict(&mut self, series: &SeriesInEveryBoot) {
        let Some(entry) = self.entries.remove(series) else {
            return;
        };
        if !self.keeps_rows {
            return;
        }
        self.unsent.retain(|row| row.series != *series);
        self.unsent_bytes = self.unsent.iter().map(row_bytes).sum();
        self.evicted.push(Eviction {
            series: series.clone(),
            through_utc_ns: entry.given_through_utc_ns(self.shift_ns),
        });
    }

    /// Takes the series let go since the last call.
    pub(crate) fn take_evictions(&mut self) -> Vec<Eviction> {
        std::mem::take(&mut self.evicted)
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
            .max(start.saturating_add(Tier::OneMinute.step_i64()));
        self.collect(series, closed);
    }

    fn collect(&mut self, series: &SeriesInEveryBoot, closed: Vec<(Tier, OverviewBucket)>) {
        if !self.keeps_rows {
            return;
        }
        for (tier, bucket) in closed {
            let row = TierRow {
                series: series.clone(),
                tier,
                bucket,
            };
            self.unsent_bytes += row_bytes(&row);
            self.unsent.push(row);
        }
    }

    /// Puts back what the store kept of a series (startup).
    pub(crate) fn restore(
        &mut self,
        series: &SeriesInEveryBoot,
        buckets: &[(Tier, OverviewBucket)],
    ) {
        let entry = self.entry(series);
        for (tier, bucket) in buckets {
            entry.tiers.restore_closed(*tier, *bucket);
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
        let bars = match self.entries.get(series) {
            Some(entry) => {
                let waiting = entry.pending.iter().chain(self.after_fed(entry, extra));
                bars_in(&entry.tiers, tier, window, waiting)
            }
            None => bars_in(&TieredSeries::new(self.shift_ns), tier, window, extra),
        };
        OverviewBars {
            whole: whole_of(&bars, window),
            bars,
            parts: Vec::new(),
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
        let empty = TieredSeries::new(self.shift_ns);
        spans_ns
            .iter()
            .map(|span| {
                let window = (
                    end.saturating_sub(i64::try_from(*span).unwrap_or(i64::MAX)),
                    end,
                );
                match entry {
                    Some(entry) => merged_between(
                        &entry.tiers,
                        window,
                        entry.pending.iter().chain(self.after_fed(entry, extra)),
                    ),
                    None => merged_between(&empty, window, extra),
                }
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

/// `bars` merged into one bucket starting at the window's start: the
/// window's mean over the time it holds, its peak and that time (D-99).
fn whole_of(bars: &[OverviewBucket], (start, _): (i64, i64)) -> OverviewBucket {
    let mut whole = OverviewBucket::new(start);
    for bar in bars {
        whole.merge(bar);
    }
    whole
}
