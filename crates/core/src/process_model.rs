//! What the history keeps of the GPU processes, as the Manager reads it
//! (D-79): the history follows the runs and what each GPU process holds
//! over time on its own actor and publishes a bounded read model (like the
//! state's snapshot), which the Manager's presentation only shapes into
//! views. The Manager hands it the committed process tables with the
//! snapshot and catalog they came with ([`ProcessTableSample`]) and the
//! runs a session shows ([`RunRequest`]).

use std::sync::Arc;

use crate::canonical::{CatalogSnapshot, ImmutableSnapshot, ProcessTable};
use crate::identity::{EntityId, ProcessKey};
use crate::process_history::{RunBucket, RunEnd, RunInfo};
use crate::tiers::BarSize;

/// A committed process table, new since the last one handed over, as the
/// Manager hands it on unchanged (D-99): the snapshot it is new in and the
/// catalog of that moment, from which the history takes what a sample of
/// the runs needs beside the table (the GPUs' labels, the host's clock
/// ticks and network rates).
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessTableSample {
    /// The committed snapshot whose `process_table` is new.
    pub snapshot: Arc<ImmutableSnapshot>,
    pub catalog: Arc<CatalogSnapshot>,
    /// How often the table is read: a table's memory values hold for a
    /// few such periods at most (D-64).
    pub process_period_ms: u64,
}

impl ProcessTableSample {
    /// The new table.
    pub fn table(&self) -> &Arc<ProcessTable> {
        &self.snapshot.process_table
    }
}

/// A run a session shows, at a bar size, over as many bars as its plots
/// have columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub key: ProcessKey,
    pub bar: BarSize,
    pub columns: usize,
}

/// One listed run with its whole-life figures.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub key: ProcessKey,
    pub name: String,
    pub started_utc_ns: i64,
    pub runtime_ns: u64,
    pub live: bool,
    pub end: Option<RunEnd>,
    pub gpus: Vec<String>,
    /// Everything it used, as one bucket: every sample counted once.
    pub whole: RunBucket,
}

/// A run's buckets of one size laid out as chart bars (D-77).
#[derive(Debug, Clone, PartialEq)]
pub struct RunBars {
    /// The size drawn: the one asked for, or the one `Auto` chose.
    pub bar: BarSize,
    pub step_ns: i64,
    /// `[start, end)` of the window, UTC.
    pub window: (i64, i64),
    /// The bars holding a sample, oldest first.
    pub bars: Vec<RunBucket>,
}

/// What a session needs to show one run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunDetail {
    /// What it answers.
    pub request: RunRequest,
    pub info: RunInfo,
    pub live: bool,
    /// `None` while no bucket is held.
    pub bars: Option<RunBars>,
    pub whole: RunBucket,
    /// Its buckets are being read from the long-term store.
    pub loading: bool,
}

/// A GPU process that holds a color (D-64, D-65).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedProcess {
    /// Its color (`0..PROCESS_COLOR_SLOTS`); an exited process's may be a
    /// listed one's too.
    pub slot: u8,
    pub key: ProcessKey,
    pub pid: i32,
    pub name: String,
    /// Bytes on each GPU in the newest table; empty when it is not listed.
    pub current: Vec<(EntityId, u64)>,
}

impl TrackedProcess {
    /// Bytes it holds now on the GPUs `include` accepts.
    pub fn bytes_on(&self, include: impl Fn(&EntityId) -> bool) -> u64 {
        self.current
            .iter()
            .filter(|(gpu, _)| include(gpu))
            .fold(0, |sum, (_, bytes)| sum.saturating_add(*bytes))
    }

    pub fn is_listed(&self) -> bool {
        !self.current.is_empty()
    }
}

/// How far the writes to the process history's stores have come: the
/// shutdown waits for the flush it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteProgress {
    /// Flushes handled so far.
    pub flushes: u64,
    /// Writes sent and not answered yet.
    pub pending: usize,
}

impl WriteProgress {
    /// Whether `flushes` flushes were handled and their writes answered.
    pub fn settled_after(&self, flushes: u64) -> bool {
        self.flushes >= flushes && self.pending == 0
    }
}

/// The read model the history publishes (module docs).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProcessHistoryModel {
    /// Moves with every publish.
    pub revision: u64,
    /// The runs, the running ones first, each part the latest start first
    /// (D-115), every one listed (at most
    /// `process_history::MAX_LISTED_RUNS`); a session's projection takes
    /// the ones its filter keeps, at most `view::MAX_RUN_ROWS` (D-113).
    pub runs: Vec<RunRow>,
    /// Runs listed beyond those.
    pub more_runs: usize,
    /// Runs outlive dgxtop (the stores are on).
    pub kept_on_disk: bool,
    /// The runs sessions asked for, the latest asked last.
    pub details: Vec<RunDetail>,
    /// The processes holding a color, in color order.
    pub tracked: Vec<TrackedProcess>,
    pub writes: WriteProgress,
}

impl ProcessHistoryModel {
    /// The color of `key` while it holds one.
    pub fn slot_of(&self, key: &ProcessKey) -> Option<u8> {
        self.tracked
            .iter()
            .find(|process| process.key == *key)
            .map(|process| process.slot)
    }

    /// The detail answering `request`; until it is answered, the latest
    /// one of the same run (another size or width: the request is on its
    /// way), so an opened run never flickers back to the list.
    pub fn detail_for(&self, request: &RunRequest) -> Option<&RunDetail> {
        self.details
            .iter()
            .find(|detail| detail.request == *request)
            .or_else(|| {
                self.details
                    .iter()
                    .rev()
                    .find(|detail| detail.request.key == request.key)
            })
    }

    /// Whether `request` is answered exactly.
    pub fn answers(&self, request: &RunRequest) -> bool {
        self.details.iter().any(|detail| detail.request == *request)
    }
}

/// A tracked process's part of a memory chart (D-64): its mean in each
/// cell of an even grid over the chart's window; `None` where no process
/// list was recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryPart {
    pub slot: u8,
    pub means: Vec<Option<f64>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{BootId, HostId, Pid, PidNamespace};

    fn key(pid: i64) -> ProcessKey {
        ProcessKey {
            host_id: HostId::new("host-test").unwrap(),
            boot_id: BootId::new("boot-test").unwrap(),
            pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
            pid: Pid::new(pid).unwrap(),
            start_time_ticks: 7,
        }
    }

    fn detail(pid: i64, bar: BarSize, columns: usize) -> RunDetail {
        let request = RunRequest {
            key: key(pid),
            bar,
            columns,
        };
        RunDetail {
            info: RunInfo {
                key: request.key.clone(),
                name: String::new(),
                command: String::new(),
                cwd: String::new(),
                uid: None,
                gpus: Vec::new(),
                details: crate::process_history::RunDetails::default(),
                started_utc_ns: 0,
                last_seen_utc_ns: 0,
                end: None,
            },
            request,
            live: true,
            bars: None,
            whole: RunBucket::new(0),
            loading: false,
        }
    }

    /// The exact answer first; else the latest answer of the same run.
    #[test]
    fn a_detail_answers_its_request_or_stands_in_for_its_run() {
        let model = ProcessHistoryModel {
            details: vec![
                detail(1, BarSize::Auto, 80),
                detail(1, BarSize::OneHour, 80),
                detail(2, BarSize::Auto, 80),
            ],
            ..ProcessHistoryModel::default()
        };
        let asked = |pid, bar| RunRequest {
            key: key(pid),
            bar,
            columns: 80,
        };
        let exact = model.detail_for(&asked(1, BarSize::Auto)).unwrap();
        assert_eq!(exact.request.bar, BarSize::Auto);
        assert!(model.answers(&asked(1, BarSize::Auto)));
        let stand_in = model.detail_for(&asked(1, BarSize::OneDay)).unwrap();
        assert_eq!(
            stand_in.request.bar,
            BarSize::OneHour,
            "the latest of run 1"
        );
        assert!(!model.answers(&asked(1, BarSize::OneDay)));
        assert!(model.detail_for(&asked(3, BarSize::Auto)).is_none());
    }

    #[test]
    fn colors_and_held_bytes_come_from_the_tracked_processes() {
        let gpu = EntityId::new("gpu:GPU-aaaa").unwrap();
        let other = EntityId::new("gpu:GPU-bbbb").unwrap();
        let tracked = TrackedProcess {
            slot: 2,
            key: key(9),
            pid: 9,
            name: "train".to_owned(),
            current: vec![(gpu.clone(), 3), (other, 4)],
        };
        assert_eq!(tracked.bytes_on(|id| *id == gpu), 3);
        assert_eq!(tracked.bytes_on(|_| true), 7);
        assert!(tracked.is_listed());
        let model = ProcessHistoryModel {
            tracked: vec![tracked],
            ..ProcessHistoryModel::default()
        };
        assert_eq!(model.slot_of(&key(9)), Some(2));
        assert_eq!(model.slot_of(&key(1)), None);
    }

    /// The shutdown waits until the flush it asked for was handled and
    /// every write it sent was answered.
    #[test]
    fn writes_settle_after_the_flush_and_its_answers() {
        let mut progress = WriteProgress::default();
        assert!(progress.settled_after(0));
        assert!(!progress.settled_after(1), "the flush is not handled yet");
        progress.flushes = 1;
        progress.pending = 2;
        assert!(!progress.settled_after(1), "its writes are unanswered");
        progress.pending = 0;
        assert!(progress.settled_after(1));
    }
}
