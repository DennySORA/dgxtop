//! What each GPU process holds over time (D-64), fed from the committed
//! process tables the Manager hands the process actor (D-79).
//!
//! The processes that hold the most GPU memory get a color each, the one
//! their display name hashes to when they can, so a job keeps its color
//! ([`tracking`], D-65); the views draw their parts of the memory bars and
//! charts in it, and the process table names it. For every tracked process the
//! book records what it held on each GPU ([`buckets`]): a table's values
//! hold until the next table, at most [`HOLD_PERIODS`] sampling periods, so
//! a stalled read never stretches a value. The time process lists were
//! recorded is kept beside them: it is the time base of a process's part
//! of a chart, in which a time it was not listed counts as 0.
//!
//! Each series is a tiered series of the shared compression on the wall
//! clock, aligned to the local clock like every chart (D-97): a part's
//! cells are the chart's bars, merged from the same bucket sizes, so they
//! line up with the bars they stack on. A table's read time is placed on
//! the wall clock by the offset the actor read with it.
//!
//! Bounded: [`tracking::MAX_TRACKED_PROCESSES`] processes, each series at
//! most the compression's buckets of every size, recorded while dgxtop
//! runs (process memory never becomes a store series). An
//! exited process keeps its record until it has not been listed for a day
//! or the oldest exited record gives way to a new process; a process that
//! loses its place takes its record with it, and its memory then counts
//! among the other processes.

mod buckets;
mod tracking;

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_compression::cells_between;
use dgxtop_core::canonical::{ProcessRow, ProcessTable};
use dgxtop_core::clock::{NANOS_PER_SECOND, ms_to_ns};
use dgxtop_core::identity::{EntityId, ProcessKey};
use dgxtop_core::process_model::MemoryPart;

use self::buckets::{Held, HeldSeries, add_held};
use self::tracking::{Listed as TrackedListed, Tracking};

/// A table's values hold for at most this many sampling periods.
const HOLD_PERIODS: u64 = 3;
/// A process not listed for this long has no time left in any history
/// window; it gives its place and record up.
const RETENTION_NS: u64 = 86_400 * NANOS_PER_SECOND;

/// A tracked process as the views show it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TrackedProcess<'a> {
    /// Its color (`0..PROCESS_COLOR_SLOTS`); an exited process's may be a
    /// listed one's too.
    pub(crate) slot: u8,
    pub(crate) key: &'a ProcessKey,
    pub(crate) pid: i32,
    pub(crate) name: &'a str,
    /// Bytes on each GPU in the newest table; empty when it is not listed.
    pub(crate) current: &'a [(EntityId, u64)],
}

impl TrackedProcess<'_> {
    pub(crate) fn is_listed(&self) -> bool {
        !self.current.is_empty()
    }
}

#[derive(Debug, Clone)]
struct ProcessRecord {
    pid: i32,
    name: String,
    gpus: BTreeMap<EntityId, HeldSeries>,
}

/// The newest table's values of the tracked processes: what each holds
/// now ([`TrackedProcess::current`]), recorded once the next table says
/// how long they held.
#[derive(Debug, Clone)]
struct NewestTable {
    read_boot_ns: u64,
    /// The read time on the wall clock.
    read_utc_ns: i64,
    held: BTreeMap<ProcessKey, Vec<(EntityId, u64)>>,
}

/// The process memory record (module docs).
#[derive(Debug, Clone)]
pub(crate) struct ProcessMemoryBook {
    tracking: Tracking,
    processes: BTreeMap<ProcessKey, ProcessRecord>,
    /// When process lists were recorded.
    listing: HeldSeries,
    newest: Option<NewestTable>,
    hold_ns: u64,
    /// Local time's offset from UTC: the buckets follow the local clock.
    shift_ns: i64,
    /// Moves with every table recorded.
    revision: u64,
}

impl ProcessMemoryBook {
    /// An empty book for tables read every `interval_ms`, its buckets on
    /// the local clock `utc_offset_s` east of UTC.
    pub(crate) fn new(interval_ms: u64, utc_offset_s: i32) -> Self {
        let shift_ns = i64::from(utc_offset_s).saturating_mul(1_000_000_000);
        Self {
            tracking: Tracking::default(),
            processes: BTreeMap::new(),
            listing: HeldSeries::new(shift_ns),
            newest: None,
            hold_ns: hold_ns(interval_ms),
            shift_ns,
            revision: 0,
        }
    }

    /// A new sampling interval changes how long a table's values hold.
    pub(crate) fn set_interval(&mut self, interval_ms: u64) {
        self.hold_ns = hold_ns(interval_ms);
    }

    /// Moves with every table recorded.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// Records a committed process table (the actor hands each one once),
    /// read when the wall clock was `boot_to_utc_ns` ahead of the boot
    /// clock; one without a read time is none.
    pub(crate) fn record(&mut self, table: &ProcessTable, boot_to_utc_ns: i128) {
        let now = table.read_finished_boot_ns;
        if now == 0 {
            return;
        }
        self.revision += 1;
        self.close_newest(now);
        let listed: Vec<Listed<'_>> = table.rows.iter().filter_map(Listed::of).collect();
        let sizes: Vec<TrackedListed<'_>> = listed
            .iter()
            .map(|process| TrackedListed {
                key: &process.row.key,
                name: &process.row.name,
                bytes: total(&process.gpus),
            })
            .collect();
        let mut lost = self.tracking.update(&sizes, now);
        lost.extend(self.tracking.expire(now.saturating_sub(RETENTION_NS)));
        for key in lost {
            self.processes.remove(&key);
        }
        let read_utc_ns = i128::from(now).saturating_add(boot_to_utc_ns);
        self.update_records(listed, (now, utc_i64(read_utc_ns)));
    }

    /// Records the newest table's values over the time until `now` (boot
    /// clock), placed on the wall clock from that table's read time.
    fn close_newest(&mut self, now: u64) {
        let Some(newest) = self.newest.take() else {
            return;
        };
        let end = now.min(newest.read_boot_ns.saturating_add(self.hold_ns));
        let held_ns = i64::try_from(end.saturating_sub(newest.read_boot_ns)).unwrap_or(i64::MAX);
        if held_ns == 0 {
            return;
        }
        let span = (
            newest.read_utc_ns,
            newest.read_utc_ns.saturating_add(held_ns),
        );
        add_held(&mut self.listing, span, 1);
        for (key, gpus) in newest.held {
            let Some(record) = self.processes.get_mut(&key) else {
                continue;
            };
            for (gpu, bytes) in gpus {
                let series = record
                    .gpus
                    .entry(gpu)
                    .or_insert_with(|| HeldSeries::new(self.shift_ns));
                add_held(series, span, bytes);
            }
        }
    }

    /// The tracked processes' values in `listed`, read at `read` (boot and
    /// wall clock), held until the next table.
    fn update_records(&mut self, listed: Vec<Listed<'_>>, (read_boot_ns, read_utc_ns): (u64, i64)) {
        let mut held = BTreeMap::new();
        for Listed { row, gpus } in listed {
            if self.tracking.color_of(&row.key).is_none() {
                continue;
            }
            let record = self
                .processes
                .entry(row.key.clone())
                .or_insert_with(|| ProcessRecord {
                    pid: row.key.pid.get(),
                    name: row.name.clone(),
                    gpus: BTreeMap::new(),
                });
            record.name.clone_from(&row.name);
            held.insert(row.key.clone(), gpus);
        }
        self.newest = Some(NewestTable {
            read_boot_ns,
            read_utc_ns,
            held,
        });
    }

    /// What `key` holds on each GPU in the newest table; empty when it is
    /// not listed there.
    fn current_of(&self, key: &ProcessKey) -> &[(EntityId, u64)] {
        self.newest
            .as_ref()
            .and_then(|newest| newest.held.get(key))
            .map_or(&[], Vec::as_slice)
    }

    /// Every tracked process, in color order.
    pub(crate) fn tracked(&self) -> Vec<TrackedProcess<'_>> {
        let mut tracked: Vec<TrackedProcess<'_>> = self
            .tracking
            .holders()
            .filter_map(|(slot, key, _)| {
                let record = self.processes.get(key)?;
                Some(TrackedProcess {
                    slot,
                    key,
                    pid: record.pid,
                    name: &record.name,
                    current: self.current_of(key),
                })
            })
            .collect();
        tracked.sort_by(|left, right| {
            left.slot
                .cmp(&right.slot)
                .then(right.is_listed().cmp(&left.is_listed()))
                .then_with(|| left.key.cmp(right.key))
        });
        tracked
    }

    /// `key`'s part of a chart of the GPUs `include` accepts, in each cell
    /// of `window` (wall clock) whose time lists were recorded in is
    /// `listing`: what it held there over that time, so the parts of all
    /// processes add up to what they held together; `None` where no list
    /// was recorded.
    fn means_over(
        &self,
        key: &ProcessKey,
        include: impl Fn(&EntityId) -> bool,
        window: (i64, i64),
        listing: &[Held],
    ) -> Vec<Option<f64>> {
        let Some(record) = self.processes.get(key) else {
            return vec![None; listing.len()];
        };
        let mut integrals = vec![0.0; listing.len()];
        for series in record
            .gpus
            .iter()
            .filter(|(gpu, _)| include(gpu))
            .map(|(_, series)| series)
        {
            let cells = cells_between(series, window, listing.len());
            for (sum, cell) in integrals.iter_mut().zip(cells) {
                *sum += cell.integral;
            }
        }
        listing
            .iter()
            .zip(integrals)
            .map(|(listed, integral)| {
                #[allow(clippy::cast_precision_loss)]
                let listed_ns = listed.covered_ns as f64;
                (listed.covered_ns > 0).then(|| integral / listed_ns)
            })
            .collect()
    }

    /// Each tracked process's part of a chart of the GPUs in `gpus` over
    /// `window` (wall clock) in `cells` cells (`means_over`), in
    /// color order; a process that held nothing there is left out.
    pub(crate) fn parts(
        &self,
        gpus: &BTreeSet<EntityId>,
        window: (u64, u64),
        cells: usize,
    ) -> Vec<MemoryPart> {
        let window = utc_window(window);
        let listing = cells_between(&self.listing, window, cells);
        self.tracked()
            .into_iter()
            .filter_map(|process| {
                let means =
                    self.means_over(process.key, |gpu| gpus.contains(gpu), window, &listing);
                means
                    .iter()
                    .flatten()
                    .any(|mean| *mean > 0.0)
                    .then_some(MemoryPart {
                        slot: process.slot,
                        means,
                    })
            })
            .collect()
    }
}

/// A window of the views (unsigned wall clock) on the series' clock.
fn utc_window((start, end): (u64, u64)) -> (i64, i64) {
    let signed = |utc: u64| i64::try_from(utc).unwrap_or(i64::MAX);
    (signed(start), signed(end))
}

/// `utc_ns` as the series' signed wall-clock time, saturating.
fn utc_i64(utc_ns: i128) -> i64 {
    i64::try_from(utc_ns.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).unwrap_or(i64::MAX)
}

/// How long a table's values hold at `interval_ms`.
fn hold_ns(interval_ms: u64) -> u64 {
    ms_to_ns(interval_ms.max(1)).saturating_mul(HOLD_PERIODS)
}

/// A row that holds GPU memory, with its known bytes per GPU.
struct Listed<'a> {
    row: &'a ProcessRow,
    gpus: Vec<(EntityId, u64)>,
}

impl<'a> Listed<'a> {
    /// `row` when it holds known GPU memory above zero.
    fn of(row: &'a ProcessRow) -> Option<Self> {
        let gpus: Vec<(EntityId, u64)> = row
            .gpu
            .iter()
            .filter_map(|link| {
                let bytes = link.memory_bytes.filter(|bytes| *bytes > 0)?;
                Some((link.gpu_entity_id.clone(), bytes))
            })
            .collect();
        (!gpus.is_empty()).then_some(Self { row, gpus })
    }
}

fn total(gpus: &[(EntityId, u64)]) -> u64 {
    gpus.iter()
        .fold(0, |sum, (_, bytes)| sum.saturating_add(*bytes))
}

#[cfg(test)]
mod tests;
