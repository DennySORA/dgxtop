//! The Overview History in the history (D-82): every Overview series in
//! every bar size, on the same compression as the Process History, with
//! no age limit from the archive.
//!
//! * Fed by the minutes the history finalizes (the same ones the archive
//!   queue journals), each on the wall clock ([`rows`]).
//! * Its own in-memory cache ([`book`]) answers every chart and statistics
//!   read ([`reader`]); the store is read once, at startup.
//! * Kept on disk through the store port (`HistoryTierJob`, [`link`]),
//!   when the archive is on: at startup it loads what the store kept, lists
//!   the series earlier runs left in the archive and compresses each one's
//!   minutes the sizes do not hold yet (a bounded page a read, one series
//!   at a time); from then on it hands the store every closed bucket and
//!   asks the archive, through its journals, to delete the minutes the
//!   minutes size let go — the coarser sizes hold them.
//!
//! The archive is pruned only while nothing it would lose is unwritten:
//! the startup is over, every closed bucket was written and no write
//! failed this run. A series the cache lets go is not shown and not read
//! back: the store deletes it with the next journal, the archive its
//! minutes with the next archive journal (whatever the store's state),
//! and its archived minutes are not compressed if the startup is still
//! reading them. The history alone decides which series go (D-95): the
//! cache and the store each have a byte budget ([`budget`]), and the store
//! never deletes a series on its own.

mod book;
mod budget;
mod issues;
mod link;
mod reader;
pub(crate) mod rows;

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::{HostId, MetricKey};
use dgxtop_core::overview_history::{
    ArchivePrune, ArchivedMinutes, ArchivedMinutesRead, HistoryTierCompletion, HistoryTierJob,
    OverviewBars, OverviewBucket,
};
use dgxtop_core::ports::{Dispatch, Logger};
use dgxtop_core::tiers::Tier;

use self::book::{TierBook, TierMinute};
#[cfg(test)]
use self::link::{ARCHIVE_PAGE_MINUTES, PRUNE_EVERY_NS};
use self::link::{PageRead, StoreLink, appended, archived_read, listed, loaded, settle};

/// Archived minutes compressed under one hold of the lock (a day): feeds
/// from the state's commit path and readers wait for a chunk at most,
/// never for a whole page.
const ARCHIVE_CHUNK_MINUTES: usize = 1_440;

/// The history's Overview series in every size; cheap to clone, shared by
/// the recorder (feeds), the store actor (completions) and the readers.
#[derive(Clone)]
pub struct OverviewTiers {
    inner: Arc<Mutex<Inner>>,
}

impl fmt::Debug for OverviewTiers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock();
        formatter
            .debug_struct("OverviewTiers")
            .field("series", &inner.book.len())
            .field("store", &inner.link.is_some())
            .finish_non_exhaustive()
    }
}

struct Inner {
    book: TierBook,
    link: Option<StoreLink>,
    /// The most archived minutes compressed under one hold of the lock.
    #[cfg(test)]
    most_archived_under_one_lock: usize,
}

impl Inner {
    fn new(book: TierBook) -> Self {
        Self {
            book,
            link: None,
            #[cfg(test)]
            most_archived_under_one_lock: 0,
        }
    }
}

impl Default for OverviewTiers {
    fn default() -> Self {
        Self::new()
    }
}

impl OverviewTiers {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(TierBook::new()))),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Buckets follow the local clock, `utc_offset_s` east of UTC (a day's
    /// bucket is a local day, D-77). Set at composition, before any data.
    pub fn set_utc_offset(&self, utc_offset_s: i32) {
        let shift_ns = i64::from(utc_offset_s).saturating_mul(1_000_000_000);
        self.lock().book.set_shift(shift_ns);
    }

    /// Keeps the sizes in a store from now on: loads what it kept, then
    /// compresses the archive's minutes from before `run_start_utc_ns` of
    /// `host_id`'s series. Live minutes wait until each series' earlier
    /// history is in. The store's issues are logged to `logger`.
    pub fn attach_store(
        &self,
        port: Box<dyn Dispatch<HistoryTierJob>>,
        host_id: HostId,
        run_start_utc_ns: i64,
        logger: Arc<dyn Logger>,
    ) {
        let mut inner = self.lock();
        let Inner {
            book, link: slot, ..
        } = &mut *inner;
        book.keep_on_disk();
        *slot = Some(StoreLink::start(
            book,
            port,
            (host_id, run_start_utc_ns),
            logger,
        ));
    }

    /// What a store job did.
    pub fn on_store(&self, completion: HistoryTierCompletion) {
        match completion {
            HistoryTierCompletion::Loaded(result) => {
                self.with_link(|book, link| loaded(book, link, result));
            }
            HistoryTierCompletion::Listed(result) => {
                self.with_link(|book, link| listed(book, link, result));
            }
            HistoryTierCompletion::ArchivedRead { read, result } => {
                let result = result.map(|page| self.compress_archived(&read, &page));
                self.with_link(|book, link| archived_read(book, link, &read, result));
            }
            HistoryTierCompletion::Appended(result) => {
                self.with_link(|book, link| appended(book, link, result));
            }
        }
    }

    /// Runs `step` under the lock while a store is kept, then lets what
    /// the budget asks go and hands the store what is due.
    fn with_link(&self, step: impl FnOnce(&mut TierBook, &mut StoreLink)) {
        let mut inner = self.lock();
        let Inner { book, link, .. } = &mut *inner;
        let Some(link) = link.as_mut() else {
            return;
        };
        step(book, link);
        settle(book, link);
    }

    /// The archived minutes of the series being read into its sizes, a
    /// chunk under each hold of the lock; stops when the series was let go
    /// meanwhile.
    fn compress_archived(&self, read: &ArchivedMinutesRead, page: &ArchivedMinutes) -> PageRead {
        for chunk in page.minutes.chunks(ARCHIVE_CHUNK_MINUTES) {
            let mut wanted = false;
            self.with_link(|book, link| wanted = link.add_archived(book, &read.series, chunk));
            if !wanted {
                break;
            }
            #[cfg(test)]
            {
                let mut inner = self.lock();
                inner.most_archived_under_one_lock =
                    inner.most_archived_under_one_lock.max(chunk.len());
            }
        }
        PageRead {
            next_from_utc_ns: page
                .minutes
                .last()
                .filter(|_| page.more)
                .map(|last| last.start_utc_ns.saturating_add(1)),
        }
    }

    /// Finalized minutes, into the sizes; their closed buckets go to the
    /// store.
    pub(crate) fn feed(&self, minutes: Vec<TierMinute>) {
        let mut inner = self.lock();
        let Inner { book, link, .. } = &mut *inner;
        for minute in minutes {
            book.feed(minute);
        }
        match link.as_mut() {
            Some(link) => settle(book, link),
            None => {
                book.keep_within_budget();
                drop(book.take_unsent());
            }
        }
    }

    /// The deletions the archive may make now: every series the cache let
    /// go, and the minutes the sizes hold (at most every ten minutes, only
    /// while everything they would lose is written).
    pub(crate) fn take_prunes(&self, now_boot_ns: u64) -> Vec<ArchivePrune> {
        let mut inner = self.lock();
        let Inner { book, link, .. } = &mut *inner;
        link.as_mut()
            .map_or_else(Vec::new, |link| link.take_prunes(book, now_boot_ns))
    }

    /// The bars of `tier` starting in `window`, with `extra` minutes (not
    /// finalized yet) merged in.
    pub(crate) fn bars(
        &self,
        series: &SeriesInEveryBoot,
        tier: Tier,
        window: (i64, i64),
        extra: &[OverviewBucket],
    ) -> OverviewBars {
        let inner = self.lock();
        let mut bars = inner.book.bars(series, tier, window, extra);
        bars.kept_on_disk = inner.link.is_some();
        bars.store_issue = inner
            .link
            .as_ref()
            .and_then(|link| link.latest_issue().map(str::to_owned));
        bars
    }

    /// Each span ending at `end` as one bucket.
    pub(crate) fn windows(
        &self,
        series: &SeriesInEveryBoot,
        spans_ns: &[u64],
        end: i64,
        extra: &[OverviewBucket],
    ) -> Vec<OverviewBucket> {
        self.lock().book.windows(series, spans_ns, end, extra)
    }

    /// This boot's key of a series.
    pub(crate) fn key_of(&self, series: &SeriesInEveryBoot) -> Option<MetricKey> {
        self.lock().book.key_of(series)
    }

    /// Without a store, with a cache of `budget_bytes`.
    #[cfg(test)]
    fn with_cache_budget(budget_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(TierBook::with_budget(budget_bytes)))),
        }
    }

    #[cfg(test)]
    fn most_archived_under_one_lock(&self) -> usize {
        self.lock().most_archived_under_one_lock
    }

    #[cfg(test)]
    fn with_book<R>(&self, read: impl FnOnce(&TierBook) -> R) -> R {
        read(&self.lock().book)
    }
}

#[cfg(test)]
mod tests;
