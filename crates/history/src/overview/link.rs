//! The Overview History's link to its store (D-82): the startup (load what
//! the store kept, list the archive's series, compress each one's archived
//! minutes a page at a time), then a journal of closed buckets and the
//! series the cache let go after every change, and the archive deletions
//! that are safe (D-95, D-96).

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::errors::InfraError;
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::HostId;
use dgxtop_core::overview_history::{
    ArchivePrune, ArchivedMinutesRead, ArchivedSeriesList, HistoryTierJob, OVERVIEW_METRICS,
    OverviewBucket, StoredTierSeries, TierJournal, TierRow, TierStoreUsage, is_overview_series,
};
use dgxtop_core::ports::{Dispatch, DispatchError, Logger};
use dgxtop_core::tiers::TIER_BUCKETS;

use super::book::{Eviction, TierBook};
use super::budget;
use super::issues::{self, StoreIssues};

/// Archived minutes one read returns at most (two weeks of one series).
pub(super) const ARCHIVE_PAGE_MINUTES: u32 = 20_000;
/// The archive is pruned at most this often (boot clock).
pub(super) const PRUNE_EVERY_NS: u64 = 600 * NANOS_PER_SECOND;

/// What is left after an archived page went into the sizes.
pub(super) struct PageRead {
    /// Where the next page starts, when more follow.
    pub(super) next_from_utc_ns: Option<i64>,
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
pub(super) struct StoreLink {
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
    issues: StoreIssues,
    /// The journals sent and not answered yet, oldest first (the store's
    /// one actor answers in order), each with whether it deletes series.
    appends_inflight: VecDeque<bool>,
    last_prune_boot_ns: Option<u64>,
    /// Series the cache let go that the store is still to delete.
    forget: Vec<SeriesInEveryBoot>,
    /// Their archived minutes, for the archive to delete.
    eviction_prunes: Vec<ArchivePrune>,
    /// Series let go during the startup: their archived minutes are not
    /// compressed.
    let_go: BTreeSet<SeriesInEveryBoot>,
}

impl StoreLink {
    /// A link that starts by loading what the store kept; the store's issues
    /// are logged to `logger`.
    pub(super) fn start(
        book: &mut TierBook,
        port: Box<dyn Dispatch<HistoryTierJob>>,
        (host_id, run_start_utc_ns): (HostId, i64),
        logger: Arc<dyn Logger>,
    ) -> Self {
        let mut link = Self {
            port,
            host_id,
            run_start_utc_ns,
            phase: Phase::Loading,
            held: None,
            listed: false,
            healthy: true,
            issues: StoreIssues::new(logger),
            appends_inflight: VecDeque::new(),
            last_prune_boot_ns: None,
            forget: Vec::new(),
            eviction_prunes: Vec::new(),
            let_go: BTreeSet::new(),
        };
        link.send(book, HistoryTierJob::Load);
        link
    }

    /// The store's latest issue, for the charts to say.
    pub(super) fn latest_issue(&self) -> Option<&str> {
        self.issues.latest()
    }

    /// The deletions the archive may make now: every series the cache let
    /// go, and the minutes the sizes hold (at most every
    /// [`PRUNE_EVERY_NS`], only while everything they would lose is
    /// written).
    pub(super) fn take_prunes(
        &mut self,
        book: &mut TierBook,
        now_boot_ns: u64,
    ) -> Vec<ArchivePrune> {
        let mut prunes = std::mem::take(&mut self.eviction_prunes);
        let settled = self.healthy
            && matches!(self.phase, Phase::Done)
            && self.held.is_none()
            && self.appends_inflight.is_empty()
            && !book.has_unsent();
        let due = self
            .last_prune_boot_ns
            .is_none_or(|last| now_boot_ns.saturating_sub(last) >= PRUNE_EVERY_NS);
        if settled && due {
            self.last_prune_boot_ns = Some(now_boot_ns);
            prunes.extend(book.prunes());
        }
        prunes
    }

    /// A store call failed: nothing it would have held may be lost from
    /// the archive this run.
    fn fail(&mut self, code: &'static str, error: &InfraError) {
        self.healthy = false;
        self.issues.infra(code, "history tiers", error);
    }

    /// What the cache let go: deleted from the store and the archive, and
    /// left out of what the startup still reads.
    fn forget(&mut self, evictions: Vec<Eviction>) {
        for eviction in evictions {
            // This run's minutes of it all end by `through_utc_ns`, earlier
            // runs' before this run started; a comeback starts after both.
            let before_utc_ns = eviction.through_utc_ns.max(self.run_start_utc_ns);
            self.eviction_prunes.push(ArchivePrune {
                series: eviction.series.clone(),
                before_utc_ns,
            });
            self.forget.push(eviction.series.clone());
            if matches!(self.phase, Phase::Done) {
                continue;
            }
            if let Phase::Backfilling { queue, .. } = &mut self.phase {
                queue.retain(|series| *series != eviction.series);
            }
            self.let_go.insert(eviction.series);
        }
    }

    /// Past the store's budget the series given a minute longest ago give
    /// way, never while a deletion is still unanswered (the usage would
    /// not show it yet).
    fn give_way_for_store(&mut self, book: &mut TierBook, usage: TierStoreUsage) {
        let deleting = !self.forget.is_empty() || self.appends_inflight.contains(&true);
        let count = budget::series_giving_way(usage, budget::STORE_BUDGET_BYTES);
        if !deleting && count > 0 {
            book.give_way(count);
            self.issues
                .gave_way(count, usage.used_bytes, budget::STORE_BUDGET_BYTES);
        }
    }

    /// Drops the oldest closed buckets past [`budget::UNSENT_BUDGET_BYTES`]
    /// while the store cannot take them; the archive is then no longer
    /// pruned this run (what it would lose was not written).
    fn keep_unsent_within_budget(&mut self, book: &mut TierBook) {
        if book.shed_unsent(budget::UNSENT_BUDGET_BYTES) {
            self.healthy = false;
            self.issues.rows_dropped();
        }
    }

    /// One chunk of an archived page of the series being read into its
    /// sizes (the first one of its first page goes on from what was
    /// stored); `false` when it is no longer read.
    pub(super) fn add_archived(
        &mut self,
        book: &mut TierBook,
        series: &SeriesInEveryBoot,
        minutes: &[OverviewBucket],
    ) -> bool {
        let Phase::Backfilling {
            reading: Some((read, first_page)),
            ..
        } = &mut self.phase
        else {
            return false;
        };
        if read != series || self.let_go.contains(series) {
            return false;
        }
        book.add_archived(series, minutes, *first_page);
        *first_page = false;
        true
    }

    /// Takes the series being read, with whether its first page is still
    /// to come, when the answer is about it.
    fn take_reading(&mut self, series: &SeriesInEveryBoot) -> Option<(SeriesInEveryBoot, bool)> {
        let Phase::Backfilling { reading, .. } = &mut self.phase else {
            return None;
        };
        reading.take_if(|(read, _)| read == series)
    }

    /// Sends a startup job, holding it while the queue is full.
    fn send(&mut self, book: &mut TierBook, job: HistoryTierJob) {
        match self.port.try_dispatch(job.clone()) {
            Ok(()) => {}
            Err(DispatchError::Busy) => self.held = Some(job),
            Err(error) => self.stop(book, &error),
        }
    }

    /// The store actor is gone: no answer will come, so the startup is
    /// over and every series still waiting for it is fed live (none
    /// backfilled, so the archive is never pruned for them).
    fn stop(&mut self, book: &mut TierBook, error: &DispatchError) {
        self.healthy = false;
        self.issues.stopped(error);
        self.held = None;
        if !matches!(self.phase, Phase::Done) {
            self.phase = Phase::Done;
            self.let_go.clear();
            book.release(false);
        }
    }
}

/// What the store kept is restored; then the archive lists its series.
pub(super) fn loaded(
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
        Err(error) => link.fail(issues::LOAD_FAILED, &error),
    }
    link.phase = Phase::Listing;
    link.send(
        book,
        HistoryTierJob::ListArchived(ArchivedSeriesList {
            host_id: link.host_id.clone(),
            metric_ids: OVERVIEW_METRICS.iter().map(|id| (*id).to_owned()).collect(),
            before_utc_ns: link.run_start_utc_ns,
        }),
    );
}

/// The archive's Overview series are read one after the other.
pub(super) fn listed(
    book: &mut TierBook,
    link: &mut StoreLink,
    result: Result<Vec<SeriesInEveryBoot>, InfraError>,
) {
    let queue = match result {
        Ok(series) => {
            link.listed = true;
            series
                .into_iter()
                .filter(|series| {
                    is_overview_series(series.metric_id.as_str(), series.entity_id.as_str())
                        && !link.let_go.contains(series)
                })
                .collect()
        }
        Err(error) => {
            link.issues
                .infra(issues::ARCHIVE_LIST_FAILED, "history archive", &error);
            VecDeque::new()
        }
    };
    link.phase = Phase::Backfilling {
        queue,
        reading: None,
    };
    next_backfill(book, link);
}

/// A page of the series being read went into its sizes: the next page, or
/// the next series.
pub(super) fn archived_read(
    book: &mut TierBook,
    link: &mut StoreLink,
    read: &ArchivedMinutesRead,
    result: Result<PageRead, InfraError>,
) {
    let Some((series, first_page)) = link.take_reading(&read.series) else {
        return;
    };
    if link.let_go.contains(&series) {
        next_backfill(book, link);
        return;
    }
    match result {
        Ok(page) => {
            // A page without minutes still goes on from what was stored.
            book.add_archived(&series, &[], first_page);
            if let Some(from_utc_ns) = page.next_from_utc_ns {
                read_next_page(book, link, read, series, from_utc_ns);
                return;
            }
            book.finish(&series, true, true);
        }
        Err(error) => {
            link.issues
                .infra(issues::ARCHIVE_READ_FAILED, "history archive", &error);
            book.finish(&series, false, !first_page);
        }
    }
    next_backfill(book, link);
}

/// Asks for the page of `series` after the one just read.
fn read_next_page(
    book: &mut TierBook,
    link: &mut StoreLink,
    read: &ArchivedMinutesRead,
    series: SeriesInEveryBoot,
    from_utc_ns: i64,
) {
    if let Phase::Backfilling { reading, .. } = &mut link.phase {
        *reading = Some((series, false));
    }
    let next = ArchivedMinutesRead {
        from_utc_ns,
        ..read.clone()
    };
    link.send(book, HistoryTierJob::ReadArchived(next));
}

/// Reads the next series' archived minutes; with none left the startup is
/// over.
fn next_backfill(book: &mut TierBook, link: &mut StoreLink) {
    let Phase::Backfilling { queue, reading } = &mut link.phase else {
        return;
    };
    let Some(series) = queue.pop_front() else {
        link.phase = Phase::Done;
        link.let_go.clear();
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
    link.send(book, HistoryTierJob::ReadArchived(read));
}

/// A journal was answered: past the store's budget series give way.
pub(super) fn appended(
    book: &mut TierBook,
    link: &mut StoreLink,
    result: Result<TierStoreUsage, InfraError>,
) {
    link.appends_inflight.pop_front();
    match result {
        Ok(usage) => link.give_way_for_store(book, usage),
        Err(error) => link.fail(issues::WRITE_FAILED, &error),
    }
}

/// After any change: what the cache's budget asks goes, and the store gets
/// what is due.
pub(super) fn settle(book: &mut TierBook, link: &mut StoreLink) {
    book.keep_within_budget();
    link.forget(book.take_evictions());
    flush(book, link);
}

/// Sends a held startup job, then the closed buckets collected.
fn flush(book: &mut TierBook, link: &mut StoreLink) {
    if let Some(job) = link.held.take() {
        link.send(book, job);
        if link.held.is_some() {
            link.keep_unsent_within_budget(book);
            return;
        }
    }
    let rows: Arc<Vec<TierRow>> = Arc::new(book.take_unsent());
    if rows.is_empty() && link.forget.is_empty() {
        return;
    }
    let journal = TierJournal {
        forget: link.forget.clone(),
        rows: Arc::clone(&rows),
        keep_latest: u32::try_from(TIER_BUCKETS).unwrap_or(u32::MAX),
    };
    match link.port.try_dispatch(HistoryTierJob::Append(journal)) {
        Ok(()) => {
            link.appends_inflight.push_back(!link.forget.is_empty());
            link.forget.clear();
        }
        Err(DispatchError::Busy) => {
            // The refused journal is dropped already: the rows come back
            // without a copy.
            book.return_unsent(Arc::unwrap_or_clone(rows));
            link.keep_unsent_within_budget(book);
        }
        Err(error) => link.stop(book, &error),
    }
}
