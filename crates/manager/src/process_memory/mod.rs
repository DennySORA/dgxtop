//! What each GPU process holds over time (D-64), fed from the committed
//! process tables.
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
//! Bounded: [`tracking::MAX_TRACKED_PROCESSES`] processes, 24 hours each,
//! recorded while dgxtop runs (process memory never becomes a store
//! series, METRICS.md). An exited process keeps its record until it is a
//! day old or the oldest exited record gives way to a new process; a
//! process that loses its place takes its record with it, and its memory
//! then counts among the other processes.

mod buckets;
mod tracking;

use std::collections::BTreeMap;

use dgxtop_core::canonical::{ProcessRow, ProcessTable};
use dgxtop_core::clock::{NANOS_PER_SECOND, ms_to_ns};
use dgxtop_core::identity::{EntityId, ProcessKey};

#[cfg(test)]
pub(crate) use self::buckets::Held;
use self::buckets::TimeBuckets;
#[cfg(test)]
pub(crate) use self::tracking::preferred_color;
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
    /// Bytes it holds now on the GPUs `include` accepts.
    pub(crate) fn bytes_on(&self, include: impl Fn(&EntityId) -> bool) -> u64 {
        self.current
            .iter()
            .filter(|(gpu, _)| include(gpu))
            .fold(0, |sum, (_, bytes)| sum.saturating_add(*bytes))
    }

    pub(crate) fn is_listed(&self) -> bool {
        !self.current.is_empty()
    }
}

#[derive(Debug, Clone)]
struct ProcessRecord {
    pid: i32,
    name: String,
    current: Vec<(EntityId, u64)>,
    gpus: BTreeMap<EntityId, TimeBuckets>,
}

/// The newest table's values, recorded once the next table says how long
/// they held.
#[derive(Debug, Clone)]
struct Pending {
    at_ns: u64,
    held: Vec<(ProcessKey, EntityId, u64)>,
}

/// The process memory record (module docs).
#[derive(Debug, Clone)]
pub struct ProcessMemoryBook {
    tracking: Tracking,
    processes: BTreeMap<ProcessKey, ProcessRecord>,
    /// When process lists were recorded.
    listing: TimeBuckets,
    pending: Option<Pending>,
    /// Cycle and read time of the newest table recorded.
    last_table: Option<(u64, u64)>,
    hold_ns: u64,
}

impl ProcessMemoryBook {
    /// An empty book for tables read every `interval_ms`.
    pub fn new(interval_ms: u64) -> Self {
        Self {
            tracking: Tracking::default(),
            processes: BTreeMap::new(),
            listing: TimeBuckets::default(),
            pending: None,
            last_table: None,
            hold_ns: hold_ns(interval_ms),
        }
    }

    /// A new sampling interval changes how long a table's values hold.
    pub fn set_interval(&mut self, interval_ms: u64) {
        self.hold_ns = hold_ns(interval_ms);
    }

    /// Records a committed process table once (a commit of another domain
    /// hands the same table again).
    pub fn record(&mut self, table: &ProcessTable) {
        let now = table.read_finished_boot_ns;
        let stamp = (table.cycle_id, now);
        if now == 0 || self.last_table == Some(stamp) {
            return;
        }
        self.last_table = Some(stamp);
        self.close_pending(now);
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
        self.update_records(listed, now);
    }

    /// Records the pending table's values over the time until `now`.
    fn close_pending(&mut self, now: u64) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let end = now.min(pending.at_ns.saturating_add(self.hold_ns));
        if end <= pending.at_ns {
            return;
        }
        self.listing.add(pending.at_ns, end, 1);
        for (key, gpu, bytes) in pending.held {
            if let Some(record) = self.processes.get_mut(&key) {
                record
                    .gpus
                    .entry(gpu)
                    .or_default()
                    .add(pending.at_ns, end, bytes);
            }
        }
    }

    /// The tracked processes' values in `listed`, read at `at_ns`, pending
    /// until the next table.
    fn update_records(&mut self, listed: Vec<Listed<'_>>, at_ns: u64) {
        for record in self.processes.values_mut() {
            record.current.clear();
        }
        let mut held = Vec::new();
        for Listed { row, gpus } in listed {
            if self.tracking.color_of(&row.key).is_none() {
                continue;
            }
            held.extend(
                gpus.iter()
                    .map(|(gpu, bytes)| (row.key.clone(), gpu.clone(), *bytes)),
            );
            let record = self
                .processes
                .entry(row.key.clone())
                .or_insert_with(|| ProcessRecord {
                    pid: row.key.pid.get(),
                    name: row.name.clone(),
                    current: Vec::new(),
                    gpus: BTreeMap::new(),
                });
            record.name.clone_from(&row.name);
            record.current = gpus;
        }
        self.pending = Some(Pending { at_ns, held });
    }

    /// The color of `key` while it is tracked.
    pub(crate) fn slot_of(&self, key: &ProcessKey) -> Option<u8> {
        self.tracking.color_of(key)
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
                    current: &record.current,
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

    /// The GPUs `key` held memory on in its record.
    #[cfg(test)]
    pub(crate) fn gpus_of(&self, key: &ProcessKey) -> Vec<&EntityId> {
        self.processes
            .get(key)
            .map(|record| record.gpus.keys().collect())
            .unwrap_or_default()
    }

    /// What `key` held on `gpu` over `[start, end)` (boot clock): its mean
    /// over the time it was listed, that time and its peak.
    #[cfg(test)]
    pub(crate) fn held_on(
        &self,
        key: &ProcessKey,
        gpu: &EntityId,
        (start, end): (u64, u64),
    ) -> Held {
        self.processes
            .get(key)
            .and_then(|record| record.gpus.get(gpu))
            .map(|series| series.held(start, end))
            .unwrap_or_default()
    }

    /// `key`'s part of a chart of the GPUs `include` accepts, in each of
    /// `cells` even cells of `[start, end)` (boot clock): what it held
    /// there over the time process lists were recorded there, so the
    /// parts of all processes add up to what they held together; `None`
    /// where no list was recorded.
    pub(crate) fn part_means(
        &self,
        key: &ProcessKey,
        include: impl Fn(&EntityId) -> bool,
        (start, end): (u64, u64),
        cells: usize,
    ) -> Vec<Option<f64>> {
        let Some(record) = self.processes.get(key) else {
            return vec![None; cells];
        };
        let series: Vec<&TimeBuckets> = record
            .gpus
            .iter()
            .filter(|(gpu, _)| include(gpu))
            .map(|(_, series)| series)
            .collect();
        cell_edges(start, end, cells)
            .map(|(from, to)| {
                let listed = self.listing.held(from, to).covered_ns;
                #[allow(clippy::cast_precision_loss)]
                let listed_ns = listed as f64;
                (listed > 0).then(|| {
                    let integral: f64 = series
                        .iter()
                        .map(|series| series.held(from, to).integral)
                        .sum();
                    integral / listed_ns
                })
            })
            .collect()
    }
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

/// The edges of `cells` even cells of `[start, end)`.
fn cell_edges(start: u64, end: u64, cells: usize) -> impl Iterator<Item = (u64, u64)> {
    let span = u128::from(end.saturating_sub(start));
    let count = u128::try_from(cells.max(1)).unwrap_or(u128::MAX);
    let edge = move |cell: usize| {
        let offset = span * u128::try_from(cell).unwrap_or(u128::MAX) / count;
        start.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX))
    };
    (0..cells).map(move |cell| (edge(cell), edge(cell + 1)))
}

#[cfg(test)]
mod tests;
