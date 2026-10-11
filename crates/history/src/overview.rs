//! The Overview History in the history (D-82): every Overview series in
//! every bar size, on the same compression as the Process History, with
//! no age limit from the archive.
//!
//! * Fed by the minutes the history finalizes (the same ones the archive
//!   queue journals), each on the wall clock ([`rows`]).
//! * Its own in-memory cache ([`book`]) answers every chart and statistics
//!   read ([`reader`]); the store is read once, at startup.
//! * Kept on disk through the store port (`HistoryTierJob`), when the
//!   archive is on: at startup it loads what the store kept, lists the
//!   series earlier runs left in the archive and compresses each one's
//!   minutes the sizes do not hold yet (a bounded page a read, one series
//!   at a time); from then on it hands the store every closed bucket and
//!   asks the archive, through its journals, to delete the minutes the
//!   minutes size let go — the coarser sizes hold them.
//!
//! The archive is pruned only while nothing it would lose is unwritten:
//! the startup is over, every closed bucket was written and no write
//! failed this run.

mod book;
mod reader;
pub(crate) mod rows;

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::errors::InfraError;
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::{HostId, MetricKey};
use dgxtop_core::overview_history::{
    ArchivePrune, ArchivedMinutes, ArchivedMinutesRead, ArchivedSeriesList, HistoryTierCompletion,
    HistoryTierJob, OVERVIEW_METRICS, OverviewBars, OverviewBucket, StoredTierSeries, TierJournal,
    TierRow,
};
use dgxtop_core::ports::{Dispatch, DispatchError};
use dgxtop_core::process_history::{TIER_BUCKETS, Tier};

use self::book::{TierBook, TierMinute};

/// Closed buckets kept for a store that fell behind; past it the oldest
/// go (and the archive is no longer pruned this run).
const MAX_UNSENT_ROWS: usize = 200_000;
/// Archived minutes one read returns at most (two weeks of one series).
const ARCHIVE_PAGE_MINUTES: u32 = 20_000;
/// The archive is pruned at most this often (boot clock).
const PRUNE_EVERY_NS: u64 = 600 * NANOS_PER_SECOND;

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
}

/// Where the startup of a kept history stands.
#[derive(Debug)]
enum Phase {
    Loading,
    Listing,
    /// The series whose archived minutes are still to read, and the one
    /// being read (with whether its first page is still to come).
    Backfilling {
        queue: VecDeque<SeriesInEveryBoot>,
        reading: Option<(SeriesInEveryBoot, bool)>,
    },
    Done,
}

/// The store the sizes are kept in.
struct StoreLink {
    port: Box<dyn Dispatch<HistoryTierJob>>,
    host_id: HostId,
    run_start_utc_ns: i64,
    phase: Phase,
    /// A startup job the store's queue was too full for.
    held: Option<HistoryTierJob>,
    /// The archive listed its series (the startup could know them all).
    listed: bool,
    /// Every closed bucket so far reached the store.
    healthy: bool,
    issue: Option<String>,
    appends_inflight: u32,
    last_prune_boot_ns: Option<u64>,
}

impl StoreLink {
    fn fail(&mut self, error: &InfraError) {
        self.healthy = false;
        self.issue = Some(format!("history tiers: {error}"));
    }

    /// Sends a startup job, holding it while the queue is full.
    fn send(&mut self, job: HistoryTierJob) {
        match self.port.try_dispatch(job.clone()) {
            Ok(()) => {}
            Err(DispatchError::Busy) => self.held = Some(job),
            Err(error) => {
                self.healthy = false;
                self.issue = Some(format!("history tiers store stopped: {error}"));
            }
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
            inner: Arc::new(Mutex::new(Inner {
                book: TierBook::new(),
                link: None,
            })),
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
    /// history is in.
    pub fn attach_store(
        &self,
        port: Box<dyn Dispatch<HistoryTierJob>>,
        host_id: HostId,
        run_start_utc_ns: i64,
    ) {
        let mut inner = self.lock();
        inner.book.keep_on_disk();
        let mut link = StoreLink {
            port,
            host_id,
            run_start_utc_ns,
            phase: Phase::Loading,
            held: None,
            listed: false,
            healthy: true,
            issue: None,
            appends_inflight: 0,
            last_prune_boot_ns: None,
        };
        link.send(HistoryTierJob::Load);
        inner.link = Some(link);
    }

    /// What a store job did.
    pub fn on_store(&self, completion: HistoryTierCompletion) {
        let mut inner = self.lock();
        let Inner { book, link } = &mut *inner;
        let Some(link) = link.as_mut() else {
            return;
        };
        match completion {
            HistoryTierCompletion::Loaded(result) => loaded(book, link, result),
            HistoryTierCompletion::Listed(result) => listed(book, link, result),
            HistoryTierCompletion::ArchivedRead { read, result } => {
                archived_read(book, link, &read, result);
            }
            HistoryTierCompletion::Appended(result) => {
                link.appends_inflight = link.appends_inflight.saturating_sub(1);
                if let Err(error) = result {
                    link.fail(&error);
                }
            }
        }
        flush(book, link);
    }

    /// Finalized minutes, into the sizes; their closed buckets go to the
    /// store.
    pub(crate) fn feed(&self, minutes: Vec<TierMinute>) {
        let mut inner = self.lock();
        let Inner { book, link } = &mut *inner;
        for minute in minutes {
            book.feed(minute);
        }
        match link.as_mut() {
            Some(link) => flush(book, link),
            None => drop(book.take_unsent()),
        }
    }

    /// The deletions the archive may make now (at most every ten minutes,
    /// only while everything they would lose is written).
    pub(crate) fn take_prunes(&self, now_boot_ns: u64) -> Vec<ArchivePrune> {
        let mut inner = self.lock();
        let Inner { book, link } = &mut *inner;
        let Some(link) = link.as_mut() else {
            return Vec::new();
        };
        let settled = link.healthy
            && matches!(link.phase, Phase::Done)
            && link.held.is_none()
            && link.appends_inflight == 0
            && book.unsent_len() == 0;
        let due = link
            .last_prune_boot_ns
            .is_none_or(|last| now_boot_ns.saturating_sub(last) >= PRUNE_EVERY_NS);
        if !(settled && due) {
            return Vec::new();
        }
        link.last_prune_boot_ns = Some(now_boot_ns);
        book.prunes()
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
        bars.store_issue = inner.link.as_ref().and_then(|link| link.issue.clone());
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

    #[cfg(test)]
    fn with_book<R>(&self, read: impl FnOnce(&TierBook) -> R) -> R {
        read(&self.lock().book)
    }
}

fn loaded(
    book: &mut TierBook,
    link: &mut StoreLink,
    result: Result<Vec<StoredTierSeries>, InfraError>,
) {
    match result {
        Ok(stored) => {
            for series in stored {
                book.restore(&series.series, &series.buckets);
            }
        }
        Err(error) => link.fail(&error),
    }
    link.phase = Phase::Listing;
    link.send(HistoryTierJob::ListArchived(ArchivedSeriesList {
        host_id: link.host_id.clone(),
        metric_ids: OVERVIEW_METRICS.iter().map(|id| (*id).to_owned()).collect(),
        before_utc_ns: link.run_start_utc_ns,
    }));
}

fn listed(
    book: &mut TierBook,
    link: &mut StoreLink,
    result: Result<Vec<SeriesInEveryBoot>, InfraError>,
) {
    let queue = match result {
        Ok(series) => {
            link.listed = true;
            series.into_iter().collect()
        }
        Err(error) => {
            link.issue = Some(format!("history archive: {error}"));
            VecDeque::new()
        }
    };
    link.phase = Phase::Backfilling {
        queue,
        reading: None,
    };
    next_backfill(book, link);
}

fn archived_read(
    book: &mut TierBook,
    link: &mut StoreLink,
    read: &ArchivedMinutesRead,
    result: Result<ArchivedMinutes, InfraError>,
) {
    let Phase::Backfilling { reading, .. } = &mut link.phase else {
        return;
    };
    let Some((series, first_page)) = reading.take().filter(|(series, _)| *series == read.series)
    else {
        return;
    };
    match result {
        Ok(page) => {
            book.add_archived(&series, &page.minutes, first_page);
            match page.minutes.last().filter(|_| page.more) {
                Some(last) => {
                    let next = ArchivedMinutesRead {
                        from_utc_ns: last.start_utc_ns.saturating_add(1),
                        ..read.clone()
                    };
                    if let Phase::Backfilling { reading, .. } = &mut link.phase {
                        *reading = Some((series, false));
                    }
                    link.send(HistoryTierJob::ReadArchived(next));
                    return;
                }
                None => book.finish(&series, true, true),
            }
        }
        Err(error) => {
            link.issue = Some(format!("history archive: {error}"));
            book.finish(&series, false, !first_page);
        }
    }
    next_backfill(book, link);
}

/// Reads the next series' archived minutes; with none left the startup is
/// over.
fn next_backfill(book: &mut TierBook, link: &mut StoreLink) {
    let Phase::Backfilling { queue, reading } = &mut link.phase else {
        return;
    };
    let Some(series) = queue.pop_front() else {
        link.phase = Phase::Done;
        book.release(link.listed);
        return;
    };
    let read = ArchivedMinutesRead {
        from_utc_ns: book.archive_from_utc_ns(&series),
        series: series.clone(),
        segments_before_utc_ns: link.run_start_utc_ns,
        max_minutes: ARCHIVE_PAGE_MINUTES,
    };
    *reading = Some((series, true));
    link.send(HistoryTierJob::ReadArchived(read));
}

/// Sends a held startup job, then the closed buckets collected.
fn flush(book: &mut TierBook, link: &mut StoreLink) {
    if let Some(job) = link.held.take() {
        link.send(job);
        if link.held.is_some() {
            return;
        }
    }
    let rows: Vec<TierRow> = book.take_unsent();
    if rows.is_empty() {
        return;
    }
    let journal = TierJournal {
        rows: rows.clone(),
        keep_latest: u32::try_from(TIER_BUCKETS).unwrap_or(u32::MAX),
    };
    match link.port.try_dispatch(HistoryTierJob::Append(journal)) {
        Ok(()) => link.appends_inflight = link.appends_inflight.saturating_add(1),
        Err(DispatchError::Busy) => {
            book.return_unsent(rows);
            if book.shed_unsent(MAX_UNSENT_ROWS) {
                link.healthy = false;
                link.issue =
                    Some("history tiers store fell behind; oldest buckets dropped".to_owned());
            }
        }
        Err(error) => {
            link.healthy = false;
            link.issue = Some(format!("history tiers store stopped: {error}"));
        }
    }
}

#[cfg(test)]
mod tests;
