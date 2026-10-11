//! The Overview's sizes in the history (D-82): bars on the local clock,
//! partial bars as the mean of what they hold, gaps as gaps, window
//! statistics merged from aligned buckets, and the store's startup: load,
//! archive compression, waiting live minutes and archive pruning, with the
//! statistics still right after the archive's minutes are gone.

use std::collections::BTreeMap;

use dgxtop_core::errors::InfraError;
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::overview_history::{
    ArchivePrune, ArchivedMinutes, HistoryTierCompletion, HistoryTierJob, OverviewBucket,
    StoredTierSeries, TierJournal, TierStoreUsage,
};
use dgxtop_core::testing::{CapturingDispatch, host_id, metric_key};
use dgxtop_core::tiers::{TIER_BUCKETS, Tier};

use super::book::TierMinute;
use super::budget::STORE_BUDGET_BYTES;
use super::*;

const SECOND: i64 = 1_000_000_000;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;
/// Midnight UTC.
const MIDNIGHT: i64 = 1_790_208_000 * SECOND;

fn power() -> SeriesInEveryBoot {
    SeriesInEveryBoot::from(&metric_key("gpu:0", "gpu.power.draw", "gpu"))
}

#[allow(clippy::cast_precision_loss)]
fn watts(minute: i64) -> f64 {
    100.0 + (minute % 53) as f64
}

/// A boot minute starting 17 s past the wall clock's minute, fully held.
#[allow(clippy::cast_precision_loss)]
fn minute_at(start: i64, value: f64) -> OverviewBucket {
    let covered = MINUTE.unsigned_abs();
    OverviewBucket::gauge(
        start + 17 * SECOND,
        covered,
        value * covered as f64,
        (value, value),
        12,
    )
}

fn minute(index: i64) -> OverviewBucket {
    minute_at(MIDNIGHT + index * MINUTE, watts(index))
}

fn live(index: i64) -> TierMinute {
    TierMinute {
        series: power(),
        key: metric_key("gpu:0", "gpu.power.draw", "gpu"),
        bucket: minute(index),
    }
}

/// Every minute from `from` to `to` (minute indexes) as one bucket.
fn brute_force(from: i64, to: i64) -> OverviewBucket {
    let mut all = OverviewBucket::new(0);
    for index in from..to {
        all.merge(&minute(index));
    }
    all
}

fn assert_same(merged: &OverviewBucket, expected: &OverviewBucket) {
    assert_eq!(merged.covered_ns, expected.covered_ns, "covered");
    assert_eq!(merged.peak(), expected.peak(), "peak");
    let (mean, wanted) = (merged.mean().unwrap(), expected.mean().unwrap());
    assert!((mean - wanted).abs() < 1e-9, "{mean} vs {wanted}");
    let (energy, wanted) = (merged.integral().unwrap(), expected.integral().unwrap());
    assert!((energy - wanted).abs() < 1e-3, "{energy} vs {wanted}");
}

/// What the store and the archive keep across runs, and the store actor
/// that runs the jobs against them.
#[derive(Default)]
struct Disk {
    tiers: BTreeMap<(Tier, i64), OverviewBucket>,
    archive: Vec<OverviewBucket>,
    fail_appends: bool,
}

impl Disk {
    fn run(&mut self, job: HistoryTierJob) -> HistoryTierCompletion {
        match job {
            HistoryTierJob::Load => {
                let buckets: Vec<(Tier, OverviewBucket)> = self
                    .tiers
                    .iter()
                    .map(|((tier, _), bucket)| (*tier, *bucket))
                    .collect();
                let stored = (!buckets.is_empty())
                    .then(|| StoredTierSeries {
                        series: power(),
                        buckets,
                    })
                    .into_iter()
                    .collect();
                HistoryTierCompletion::Loaded(Ok(stored))
            }
            HistoryTierJob::Append(journal) => {
                if self.fail_appends {
                    return HistoryTierCompletion::Appended(Err(InfraError::DiskFull {
                        operation: "test",
                    }));
                }
                for row in journal.rows {
                    self.tiers
                        .insert((row.tier, row.bucket.start_utc_ns), row.bucket);
                }
                for tier in Tier::ALL {
                    let starts: Vec<i64> = self
                        .tiers
                        .keys()
                        .filter(|(kept, _)| *kept == tier)
                        .map(|(_, start)| *start)
                        .collect();
                    let excess = starts.len().saturating_sub(journal.keep_latest as usize);
                    for start in &starts[..excess] {
                        self.tiers.remove(&(tier, *start));
                    }
                }
                HistoryTierCompletion::Appended(Ok(TierStoreUsage::default()))
            }
            HistoryTierJob::ListArchived(list) => {
                let listed = (!self.archive.is_empty()
                    && list.metric_ids.contains(&"gpu.power.draw".to_owned()))
                .then(power)
                .into_iter()
                .collect();
                HistoryTierCompletion::Listed(Ok(listed))
            }
            HistoryTierJob::ReadArchived(read) => {
                let mut minutes: Vec<OverviewBucket> = self
                    .archive
                    .iter()
                    .filter(|minute| {
                        minute.start_utc_ns >= read.from_utc_ns
                            && minute.start_utc_ns < read.segments_before_utc_ns
                    })
                    .copied()
                    .collect();
                let page = read.max_minutes as usize;
                let more = minutes.len() > page;
                minutes.truncate(page);
                HistoryTierCompletion::ArchivedRead {
                    read,
                    result: Ok(ArchivedMinutes { minutes, more }),
                }
            }
        }
    }

    fn prune(&mut self, prunes: &[ArchivePrune]) {
        for prune in prunes {
            self.archive
                .retain(|minute| minute.start_utc_ns >= prune.before_utc_ns);
        }
    }
}

/// One run of dgxtop's history against a [`Disk`].
struct Run {
    tiers: OverviewTiers,
    port: CapturingDispatch<HistoryTierJob>,
}

impl Run {
    fn start(disk: &mut Disk, run_start: i64) -> Self {
        let run = Self::attached(OverviewTiers::new(), run_start);
        run.settle(disk);
        run
    }

    /// `tiers` with a store attached, nothing answered yet.
    fn attached(tiers: OverviewTiers, run_start: i64) -> Self {
        let port = CapturingDispatch::new();
        tiers.attach_store(Box::new(port.clone()), host_id("test-host"), run_start);
        Self { tiers, port }
    }

    /// Runs every job the history sent, and what they lead to.
    fn settle(&self, disk: &mut Disk) {
        loop {
            let jobs = self.port.take();
            if jobs.is_empty() {
                return;
            }
            for job in jobs {
                let completion = disk.run(job);
                self.tiers.on_store(completion);
            }
        }
    }

    /// Live minutes `from..to`: archived too, as the archive queue does.
    fn feed(&self, disk: &mut Disk, (from, to): (i64, i64)) {
        for index in from..to {
            disk.archive.push(minute(index));
            self.tiers.feed(vec![live(index)]);
        }
        self.settle(disk);
    }

    fn windows(&self, end: i64) -> Vec<OverviewBucket> {
        let spans: Vec<u64> = [1, 6, 12, 24]
            .map(|hours| (hours * HOUR).unsigned_abs())
            .to_vec();
        self.tiers.windows(&power(), &spans, end, &[])
    }
}

/// Bars follow the local clock and a partial bar is the mean of the time
/// it holds: a run that began at 03:15 has a 03:00 hour bar of 45 minutes.
/// A time without minutes is a gap, not a zero.
#[test]
fn bars_are_local_clock_buckets_holding_only_what_was_recorded() {
    let tiers = OverviewTiers::new();
    tiers.set_utc_offset(8 * 3_600);
    // 03:15 local is 19:15 UTC the day before.
    let local_0315 = MIDNIGHT - 5 * HOUR + 15 * MINUTE;
    let first = (local_0315 - MIDNIGHT) / MINUTE;
    let minutes: Vec<TierMinute> = (first..first + 105)
        .filter(|index| !(first + 60..first + 70).contains(index))
        .map(live)
        .collect();
    tiers.feed(minutes);
    let local_0300 = local_0315 - 15 * MINUTE;
    let window = (local_0300 - 2 * HOUR, local_0300 + 2 * HOUR);
    let bars = tiers.bars(&power(), Tier::OneHour, window, &[]);
    let shape: Vec<(i64, u64)> = bars
        .bars
        .iter()
        .map(|bar| {
            (
                (bar.start_utc_ns - local_0300) / MINUTE,
                bar.covered_ns / MINUTE.unsigned_abs(),
            )
        })
        .collect();
    assert_eq!(shape, vec![(0, 45), (60, 50)], "45 and 60 - 10 gap minutes");
    assert_same(&bars.bars[0], &brute_force(first, first + 45));
    assert_eq!(
        bars.settled_through_utc_ns,
        MIDNIGHT + (first + 105) * MINUTE,
        "every bar ending by the latest minute is final"
    );
    let gaps = tiers.bars(&power(), Tier::OneMinute, window, &[]);
    assert_eq!(gaps.bars.len(), 95, "the missing ten minutes are gaps");
    tiers.feed(vec![live(first + 105)]);
    let later = tiers.bars(&power(), Tier::OneHour, window, &[]);
    assert_eq!(later.epoch, bars.epoch, "a live minute keeps the past");
    let unknown = SeriesInEveryBoot::from(&metric_key("gpu:9", "gpu.power.draw", "gpu"));
    assert!(
        tiers
            .bars(&unknown, Tier::OneHour, window, &[])
            .bars
            .is_empty()
    );
}

/// The bar being filled holds the minutes not finalized yet, and a
/// minute finalized meanwhile is not counted twice.
#[test]
fn the_latest_bar_holds_the_minutes_not_finalized_yet() {
    let tiers = OverviewTiers::new();
    tiers.feed((0..30).map(live).collect());
    let unfinalized = [minute(29), minute(30), minute(31)];
    let window = (MIDNIGHT - 2 * HOUR, MIDNIGHT + HOUR);
    let bars = tiers.bars(&power(), Tier::OneHour, window, &unfinalized);
    assert_eq!(bars.bars.len(), 1);
    assert_same(&bars.bars[0], &brute_force(0, 32));
    let windows = tiers.windows(
        &power(),
        &[HOUR.unsigned_abs()],
        MIDNIGHT + 32 * MINUTE,
        &unfinalized,
    );
    assert_same(&windows[0], &brute_force(0, 32));
}

/// The last 1 h to 24 h are merges of aligned buckets, exact against every
/// minute, and stay exact after a restart from the store once the archive
/// let the old minutes go.
#[test]
fn statistics_stay_right_after_the_archive_minutes_are_deleted() {
    let mut disk = Disk::default();
    let first = Run::start(&mut disk, MIDNIGHT);
    // Every window starts on a five-minute boundary the sizes still hold.
    let minutes = 2 * 24 * 60 + 35;
    first.feed(&mut disk, (0, minutes));
    let end = MIDNIGHT + minutes * MINUTE;
    let before = first.windows(end);
    for (window, hours) in before.iter().zip([1, 6, 12, 24]) {
        assert_same(window, &brute_force(minutes - hours * 60, minutes));
    }
    let prunes = first.tiers.take_prunes(1);
    assert_eq!(prunes.len(), 1, "the backfilled series may be pruned");
    assert!(
        prunes[0].before_utc_ns >= end - (i64::try_from(TIER_BUCKETS).unwrap() + 2) * MINUTE,
        "the minutes the minutes size let go"
    );
    disk.prune(&prunes);
    assert!(disk.archive.len() <= TIER_BUCKETS + 2);
    assert!(
        first.tiers.take_prunes(2).is_empty(),
        "at most every ten minutes"
    );
    // The next run: only the store and the pruned archive are left.
    let second = Run::start(&mut disk, end + HOUR);
    let after = second.windows(end);
    for (index, (before, after)) in before.iter().zip(&after).enumerate() {
        assert_same(after, before);
        assert!(index < 4);
    }
    let day = second
        .tiers
        .bars(&power(), Tier::OneDay, (MIDNIGHT - 24 * HOUR, end), &[]);
    let total: u64 = day.bars.iter().map(|bar| bar.covered_ns).sum();
    assert_eq!(
        total,
        (minutes * MINUTE).unsigned_abs(),
        "every minute, once"
    );
}

/// A first run with the new store compresses every archived minute into
/// every size; live minutes that came meanwhile wait and follow them.
#[test]
fn the_archive_is_compressed_at_startup_and_live_minutes_wait() {
    let earlier = 3 * 24 * 60;
    let mut disk = Disk {
        archive: (0..earlier).map(minute).collect(),
        ..Disk::default()
    };
    let run = Run {
        tiers: OverviewTiers::new(),
        port: CapturingDispatch::new(),
    };
    run.tiers.attach_store(
        Box::new(run.port.clone()),
        host_id("test-host"),
        MIDNIGHT + earlier * MINUTE,
    );
    // Live minutes before the store answered: they wait.
    run.tiers.feed((earlier..earlier + 3).map(live).collect());
    run.tiers.with_book(|book| {
        assert!(book.tiers_of(&power()).unwrap().is_empty(), "waiting");
    });
    let waiting = run.tiers.bars(
        &power(),
        Tier::OneHour,
        (MIDNIGHT, MIDNIGHT + 100 * HOUR),
        &[],
    );
    assert_eq!(
        waiting.settled_through_utc_ns,
        i64::MIN,
        "nothing settled yet"
    );
    assert_eq!(waiting.bars.len(), 1, "waiting minutes are read too");
    run.settle(&mut disk);
    let days = run.tiers.bars(
        &power(),
        Tier::OneDay,
        (MIDNIGHT - 24 * HOUR, MIDNIGHT + 5 * 24 * HOUR),
        &[],
    );
    let counted: Vec<u64> = days
        .bars
        .iter()
        .map(|bar| bar.covered_ns / MINUTE.unsigned_abs())
        .collect();
    assert_eq!(counted, vec![1440, 1440, 1440, 3]);
    assert_ne!(
        days.epoch, waiting.epoch,
        "the past changed under any cached bar"
    );
    assert!(disk.tiers.keys().any(|(tier, _)| *tier == Tier::OneDay));
    assert!(
        disk.tiers
            .keys()
            .filter(|(tier, _)| *tier == Tier::OneMinute)
            .count()
            <= TIER_BUCKETS,
        "the store keeps the latest of each size"
    );
}

/// No deletion while anything it would lose may be unwritten: during the
/// startup, with a write in flight, or after a write failed.
#[test]
fn the_archive_is_pruned_only_once_everything_is_written() {
    let mut disk = Disk::default();
    let run = Run {
        tiers: OverviewTiers::new(),
        port: CapturingDispatch::new(),
    };
    run.tiers
        .attach_store(Box::new(run.port.clone()), host_id("test-host"), MIDNIGHT);
    assert!(
        run.tiers.take_prunes(0).is_empty(),
        "the startup is not over"
    );
    run.settle(&mut disk);
    run.tiers.feed((0..401).map(live).collect());
    assert!(run.tiers.take_prunes(0).is_empty(), "a write is in flight");
    run.settle(&mut disk);
    assert_eq!(run.tiers.take_prunes(0).len(), 1);
    disk.fail_appends = true;
    run.feed(&mut disk, (401, 420));
    assert!(
        run.tiers.take_prunes(PRUNE_EVERY_NS * 2).is_empty(),
        "a failed write keeps the archive's minutes"
    );
    let bars = run.tiers.bars(
        &power(),
        Tier::OneHour,
        (MIDNIGHT, MIDNIGHT + 10 * HOUR),
        &[],
    );
    assert!(bars.store_issue.unwrap().contains("disk full"));
}

/// A store that cannot be read still lets the archive be compressed; a
/// busy store's jobs are sent again.
#[test]
fn a_failed_load_or_a_busy_store_does_not_lose_the_archive() {
    let mut disk = Disk {
        archive: (0..90).map(minute).collect(),
        ..Disk::default()
    };
    let run = Run {
        tiers: OverviewTiers::new(),
        port: CapturingDispatch::new(),
    };
    run.port.set_busy(true);
    run.tiers.attach_store(
        Box::new(run.port.clone()),
        host_id("test-host"),
        MIDNIGHT + 90 * MINUTE,
    );
    assert!(run.port.is_empty());
    run.port.set_busy(false);
    run.tiers.feed(Vec::new());
    let jobs = run.port.take();
    assert_eq!(jobs, vec![HistoryTierJob::Load], "held, then sent");
    run.tiers
        .on_store(HistoryTierCompletion::Loaded(Err(InfraError::Unavailable(
            "locked".to_owned(),
        ))));
    run.settle(&mut disk);
    let hours = run.tiers.bars(
        &power(),
        Tier::OneHour,
        (MIDNIGHT, MIDNIGHT + 2 * HOUR),
        &[],
    );
    let held: u64 = hours.bars.iter().map(|bar| bar.covered_ns).sum();
    assert_eq!(held, (90 * MINUTE).unsigned_abs());
    assert!(
        run.tiers.take_prunes(0).is_empty(),
        "the store was not healthy"
    );
}

/// A cache budget that holds exactly what `build` leaves in a cache.
fn budget_of(build: impl FnOnce(&OverviewTiers)) -> usize {
    let scratch = OverviewTiers::new();
    build(&scratch);
    scratch.with_book(TierBook::bytes)
}

/// The cache is bounded by bytes (D-95): past its budget the series given
/// a minute longest ago goes, and what is left fits.
#[test]
fn the_cache_is_bounded() {
    let feed = |tiers: &OverviewTiers, gpus: std::ops::RangeInclusive<usize>| {
        for gpu in gpus {
            tiers.feed(vec![gpu_minute(gpu, i64::try_from(gpu).unwrap())]);
        }
    };
    let budget = budget_of(|scratch| feed(scratch, 1..=3));
    let tiers = OverviewTiers::with_cache_budget(budget);
    feed(&tiers, 0..=3);
    tiers.with_book(|book| {
        assert_eq!(book.len(), 3);
        assert!(
            book.entry_of(&gpu_series(0)).is_none(),
            "the one fed longest ago went"
        );
        assert!(book.bytes() <= budget);
    });
}

/// The default budget holds every Overview series of a large host (eight
/// GPUs, a dozen disks and interfaces) at a day of minutes each.
#[test]
fn the_default_budget_holds_a_large_host() {
    let tiers = OverviewTiers::new();
    let gpus = 8 * 5 + 2 + 12 * 2 + 12 * 2;
    for gpu in 0..gpus {
        let day: Vec<TierMinute> = (0..24 * 60).map(|index| gpu_minute(gpu, index)).collect();
        tiers.feed(day);
    }
    tiers.with_book(|book| assert_eq!(book.len(), gpus, "{} bytes", book.bytes()));
}

fn gpu_series(gpu: usize) -> SeriesInEveryBoot {
    SeriesInEveryBoot::from(&metric_key(&format!("gpu:{gpu}"), "gpu.power.draw", "gpu"))
}

fn gpu_minute(gpu: usize, index: i64) -> TierMinute {
    let key = metric_key(&format!("gpu:{gpu}"), "gpu.power.draw", "gpu");
    TierMinute {
        series: SeriesInEveryBoot::from(&key),
        key,
        bucket: minute(index),
    }
}

/// The journals the history sent to its store.
fn appends(jobs: &[HistoryTierJob]) -> Vec<&TierJournal> {
    jobs.iter()
        .filter_map(|job| match job {
            HistoryTierJob::Append(journal) => Some(journal),
            _ => None,
        })
        .collect()
}

/// A series the cache lets go is gone (owner): the store deletes it with
/// the next journal (its buckets not sent yet go with it), the archive its
/// minutes whatever the store's state, and when it comes back it starts
/// fresh, after what was deleted.
#[test]
fn an_evicted_series_is_deleted_everywhere_and_comes_back_fresh() {
    let mut disk = Disk::default();
    let feed_before_eviction = |tiers: &OverviewTiers| {
        tiers.feed((0..3).map(|index| gpu_minute(0, index)).collect());
        for gpu in 1..4 {
            tiers.feed(vec![gpu_minute(gpu, 10)]);
        }
    };
    let budget = budget_of(feed_before_eviction);
    let run = Run::attached(OverviewTiers::with_cache_budget(budget), MIDNIGHT);
    run.settle(&mut disk);
    run.port.set_busy(true);
    feed_before_eviction(&run.tiers);
    assert!(run.port.is_empty(), "the store is busy");
    run.port.set_busy(false);
    run.tiers.feed(vec![gpu_minute(4, 10)]);
    let jobs = run.port.take();
    let sent = appends(&jobs);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].forget, vec![gpu_series(0)]);
    assert!(
        sent[0].rows.iter().all(|row| row.series != gpu_series(0)),
        "its buckets not sent yet went with it"
    );
    let prunes = run.tiers.take_prunes(0);
    assert_eq!(
        prunes,
        vec![ArchivePrune {
            series: gpu_series(0),
            before_utc_ns: MIDNIGHT + 3 * MINUTE,
        }],
        "deleted from the archive although a write is in flight"
    );
    assert!(run.tiers.take_prunes(0).is_empty(), "asked once");
    run.tiers.on_store(HistoryTierCompletion::Appended(Ok(
        TierStoreUsage::default(),
    )));
    assert!(
        appends(&run.port.take()).is_empty(),
        "nothing left to forget"
    );

    run.tiers.feed(vec![gpu_minute(0, 20)]);
    let bars = run.tiers.bars(
        &gpu_series(0),
        Tier::OneDay,
        (MIDNIGHT - 24 * HOUR, MIDNIGHT + 24 * HOUR),
        &[],
    );
    let held: u64 = bars.bars.iter().map(|bar| bar.covered_ns).sum();
    assert_eq!(held, MINUTE.unsigned_abs(), "only what came after");
    assert!(minute(20).start_utc_ns >= prunes[0].before_utc_ns);
    let comeback = run.port.take();
    assert!(
        appends(&comeback)
            .iter()
            .all(|journal| !journal.forget.contains(&gpu_series(0))),
        "the comeback is not deleted"
    );
}

/// A series let go while the startup runs is not read back: one the load
/// pushed out is not compressed from the archive, and one let go while its
/// archived minutes are being read keeps none of them.
#[test]
fn a_series_let_go_during_the_startup_is_not_read_back() {
    let run_start = MIDNIGHT + 4 * HOUR;
    let stored = |gpus: std::ops::RangeInclusive<usize>| {
        gpus.map(|gpu| StoredTierSeries {
            series: gpu_series(gpu),
            buckets: vec![(Tier::OneMinute, minute(i64::try_from(gpu).unwrap()))],
        })
        .collect()
    };
    let budget = budget_of(|scratch| {
        scratch.attach_store(
            Box::new(CapturingDispatch::new()),
            host_id("test-host"),
            run_start,
        );
        // What is left once the load pushed one out and a new series took
        // the place of another.
        scratch.on_store(HistoryTierCompletion::Loaded(Ok(stored(2..=4))));
        scratch.feed(vec![gpu_minute(500, 200)]);
    });
    let run = Run::attached(OverviewTiers::with_cache_budget(budget), run_start);
    assert_eq!(run.port.take(), vec![HistoryTierJob::Load]);
    run.tiers
        .on_store(HistoryTierCompletion::Loaded(Ok(stored(0..=4))));
    let jobs = run.port.take();
    assert!(matches!(jobs[0], HistoryTierJob::ListArchived(_)));
    assert_eq!(appends(&jobs)[0].forget, vec![gpu_series(0)]);
    run.tiers.on_store(HistoryTierCompletion::Listed(Ok(vec![
        gpu_series(0),
        gpu_series(1),
        gpu_series(2),
    ])));
    let read = match run.port.take().as_slice() {
        [HistoryTierJob::ReadArchived(read)] => read.clone(),
        other => panic!("one read expected, got {other:?}"),
    };
    assert_eq!(read.series, gpu_series(1), "the one let go is skipped");
    // A new series takes the place of the one being read.
    run.tiers.feed(vec![gpu_minute(500, 200)]);
    run.tiers.on_store(HistoryTierCompletion::ArchivedRead {
        read,
        result: Ok(ArchivedMinutes {
            minutes: vec![minute(-30)],
            more: false,
        }),
    });
    let next: Vec<SeriesInEveryBoot> = run
        .port
        .take()
        .into_iter()
        .filter_map(|job| match job {
            HistoryTierJob::ReadArchived(read) => Some(read.series),
            _ => None,
        })
        .collect();
    assert_eq!(next, vec![gpu_series(2)]);
    run.tiers.with_book(|book| {
        assert!(book.entry_of(&gpu_series(0)).is_none());
        assert!(
            book.entry_of(&gpu_series(1)).is_none(),
            "its page was dropped"
        );
    });
    let before: Vec<(SeriesInEveryBoot, i64)> = run
        .tiers
        .take_prunes(0)
        .into_iter()
        .map(|prune| (prune.series, prune.before_utc_ns))
        .collect();
    assert_eq!(
        before,
        vec![(gpu_series(0), run_start), (gpu_series(1), run_start)],
        "every minute of earlier runs"
    );
}

/// A series let go and come back is a new past: every answer about it
/// carries an epoch it never had before, so a reader's cached bars of the
/// old one are read again (without a store, and after the startup).
#[test]
fn a_series_that_comes_back_has_a_new_epoch() {
    let tiers = OverviewTiers::with_cache_budget(budget_of(|scratch| {
        scratch.feed(vec![gpu_minute(0, 0)]);
    }));
    let window = (MIDNIGHT - 24 * HOUR, MIDNIGHT + 24 * HOUR);
    let epoch =
        |tiers: &OverviewTiers| tiers.bars(&gpu_series(0), Tier::OneHour, window, &[]).epoch;
    tiers.feed(vec![gpu_minute(0, 0)]);
    let before = epoch(&tiers);
    tiers.feed(vec![gpu_minute(1, 10)]);
    tiers.with_book(|book| assert!(book.entry_of(&gpu_series(0)).is_none()));
    let gone = epoch(&tiers);
    assert_ne!(gone, before, "the series let go is not the one cached");
    tiers.feed(vec![gpu_minute(0, 20)]);
    let back = epoch(&tiers);
    assert_ne!(back, before, "the comeback is a new past");
    assert_ne!(back, gone);
}

/// A store port that stops taking jobs once it is stopped.
#[derive(Clone, Default)]
struct StoppingPort {
    jobs: CapturingDispatch<HistoryTierJob>,
    closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl StoppingPort {
    fn stop(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Dispatch<HistoryTierJob> for StoppingPort {
    fn try_dispatch(&self, job: HistoryTierJob) -> Result<(), DispatchError> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(DispatchError::Closed);
        }
        self.jobs.try_dispatch(job)
    }
}

/// The startup ends when the store actor stops, at any step: the series
/// waiting for it are fed live from then on (nothing stays waiting), and
/// the archive is not pruned.
#[test]
fn the_startup_ends_when_the_store_stops() {
    let window = (MIDNIGHT - HOUR, MIDNIGHT + 2 * HOUR);
    for step in ["load", "list", "read"] {
        let tiers = OverviewTiers::new();
        let port = StoppingPort::default();
        if step == "load" {
            port.stop();
        }
        tiers.attach_store(Box::new(port.clone()), host_id("test-host"), MIDNIGHT);
        tiers.feed(vec![live(0)]);
        if step != "load" {
            assert_eq!(port.jobs.take(), vec![HistoryTierJob::Load], "{step}");
            if step == "read" {
                tiers.on_store(HistoryTierCompletion::Loaded(Ok(Vec::new())));
                port.jobs.take();
            }
            port.stop();
            let answer = if step == "list" {
                HistoryTierCompletion::Loaded(Ok(Vec::new()))
            } else {
                HistoryTierCompletion::Listed(Ok(vec![power()]))
            };
            tiers.on_store(answer);
        }
        tiers.feed(vec![live(1)]);
        tiers.with_book(|book| {
            let held = book.tiers_of(&power()).map_or(0, |tiers| tiers.len());
            assert!(held > 0, "{step}: the series is fed live");
        });
        let bars = tiers.bars(&power(), Tier::OneHour, window, &[]);
        assert_ne!(bars.settled_through_utc_ns, i64::MIN, "{step}: settled");
        assert!(bars.store_issue.is_some(), "{step}: the stop is shown");
        assert!(tiers.take_prunes(0).is_empty(), "{step}: nothing pruned");
    }
}

/// Past the store's budget (D-95) the series given a minute longest ago
/// give way through the cache's eviction: the next journal deletes it and
/// the archive its minutes. No more go while that deletion is unanswered
/// (its usage would not show it yet), nor once the store is within budget.
#[test]
fn the_store_budget_lets_the_series_seen_longest_ago_go() {
    let mut disk = Disk::default();
    let run = Run::start(&mut disk, MIDNIGHT);
    run.tiers
        .feed((0..3).map(|index| gpu_minute(0, index)).collect());
    run.tiers.feed(vec![gpu_minute(1, 10)]);
    run.tiers.feed(vec![gpu_minute(2, 11)]);
    run.tiers.feed(vec![gpu_minute(1, 11)]);
    assert_eq!(appends(&run.port.take()).len(), 2, "two journals in flight");
    let over = TierStoreUsage {
        used_bytes: STORE_BUDGET_BYTES + 1,
        series: 3,
    };
    run.tiers
        .on_store(HistoryTierCompletion::Appended(Ok(over)));
    let jobs = run.port.take();
    let sent = appends(&jobs);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].forget, vec![gpu_series(0)]);
    run.tiers
        .on_store(HistoryTierCompletion::Appended(Ok(over)));
    run.tiers
        .with_book(|book| assert_eq!(book.len(), 2, "the deletion is unanswered"));
    assert!(appends(&run.port.take()).is_empty());
    let within = TierStoreUsage {
        used_bytes: STORE_BUDGET_BYTES,
        series: 2,
    };
    run.tiers
        .on_store(HistoryTierCompletion::Appended(Ok(within)));
    run.tiers
        .with_book(|book| assert_eq!(book.len(), 2, "within budget"));
    let prunes = run.tiers.take_prunes(0);
    assert!(
        prunes.iter().any(|prune| prune.series == gpu_series(0)),
        "deleted from the archive too: {prunes:?}"
    );
}
