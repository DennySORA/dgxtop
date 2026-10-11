//! The process history service (D-76): the book, and the jobs that leave
//! for the probe and the stores. One probe is in flight at a time; the
//! journal leaves every [`APPEND_EVERY_NS`] and the short-term file is
//! replaced every [`SAVE_EVERY_NS`]. A store job refused for a full queue
//! keeps its journal for the next turn, within [`MAX_BACKLOG_BUCKETS`].

use std::sync::Arc;

use dgxtop_core::action::Signal;
use dgxtop_core::canonical::ProcessTable;
use dgxtop_core::identity::{BootId, ProcessKey};
use dgxtop_core::ports::{
    Dispatch, LogLevel, Logger, ProbeDeriver, ProcessHistoryCompletion, ProcessHistoryJob,
    ProcessProbeJob,
};
use dgxtop_core::process_history::ProcessHistoryJournal;
use dgxtop_core::process_probe::ProbeReport;

use super::{ProcessHistoryBook, SampleInputs};
use crate::process::Now;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
pub(crate) const APPEND_EVERY_NS: u64 = 10 * NANOS_PER_SECOND;
pub(crate) const SAVE_EVERY_NS: u64 = 30 * NANOS_PER_SECOND;
/// Buckets a backlog of refused journals keeps; older ones give way.
const MAX_BACKLOG_BUCKETS: usize = 8192;
const LOG_TARGET: &str = "history.process";

/// The ports of the process history; the probe and the stores may be
/// absent (no probe in a replay, no stores with the archive off).
pub(crate) struct ServicePorts {
    pub(crate) probe: Option<Box<dyn Dispatch<ProcessProbeJob>>>,
    /// Makes the values of the probe's readings (D-79).
    pub(crate) probe_deriver: Arc<dyn ProbeDeriver>,
    pub(crate) store: Option<Box<dyn Dispatch<ProcessHistoryJob>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Load {
    Pending,
    Done,
}

pub(crate) struct ProcessHistoryService {
    book: ProcessHistoryBook,
    probe: Option<Box<dyn Dispatch<ProcessProbeJob>>>,
    probe_deriver: Arc<dyn ProbeDeriver>,
    store: Option<Box<dyn Dispatch<ProcessHistoryJob>>>,
    logger: Arc<dyn Logger>,
    probing: bool,
    /// Writes sent and not answered yet; the shutdown waits for them.
    writing: usize,
    load: Load,
    backlog: ProcessHistoryJournal,
    next_append_ns: u64,
    next_save_ns: u64,
}

impl ProcessHistoryService {
    pub(crate) fn new(
        ports: ServicePorts,
        (boot_id, utc_offset_s): (BootId, i32),
        logger: Arc<dyn Logger>,
        now_ns: u64,
    ) -> Self {
        Self {
            book: ProcessHistoryBook::new(boot_id).with_utc_offset(utc_offset_s),
            probe: ports.probe,
            probe_deriver: ports.probe_deriver,
            store: ports.store,
            logger,
            probing: false,
            writing: 0,
            load: Load::Done,
            backlog: ProcessHistoryJournal::default(),
            next_append_ns: now_ns.saturating_add(APPEND_EVERY_NS),
            next_save_ns: now_ns.saturating_add(SAVE_EVERY_NS),
        }
    }

    pub(crate) fn book(&self) -> &ProcessHistoryBook {
        &self.book
    }

    /// Whether runs outlive dgxtop.
    pub(crate) fn keeps_runs_on_disk(&self) -> bool {
        self.store.is_some()
    }

    /// Asks the stores for what they kept.
    pub(crate) fn start(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        match store.try_dispatch(ProcessHistoryJob::Load) {
            Ok(()) => self.load = Load::Pending,
            Err(error) => self.warn(&format!("PROCESS_HISTORY_LOAD_NOT_SENT: {error}")),
        }
    }

    /// Samples the live runs from `table` and probes them.
    pub(crate) fn on_table(&mut self, table: &ProcessTable, inputs: &SampleInputs<'_>) {
        self.book.observe(table, inputs);
        self.dispatch_probe();
    }

    /// Whether `table` was sampled already: every committed batch hands
    /// over the snapshot's table, so only the first commit of a cycle
    /// brings a new one and the caller can skip building its inputs.
    #[cfg(test)]
    pub(crate) fn has_sampled(&self, table: &ProcessTable) -> bool {
        self.book.has_sampled(table)
    }

    /// A commit without a new table still lets a finished probe be followed
    /// by the next one, exactly as [`Self::on_table`] would.
    pub(crate) fn on_sampled_table(&mut self) {
        self.dispatch_probe();
    }

    pub(crate) fn on_probe(&mut self, report: &ProbeReport, now: Now) {
        self.probing = false;
        self.book.on_probe(report, now, &*self.probe_deriver);
    }

    /// dgxtop sent `signal` to `key`.
    pub(crate) fn note_signal(&mut self, key: &ProcessKey, signal: Signal, now: Now) {
        self.book.note_signal(key, signal, now);
    }

    /// Reads the buckets of `key` from the long-term store when they are
    /// not in memory (a run opened in the Process History tab).
    pub(crate) fn open(&mut self, key: &ProcessKey) {
        let Some(store) = &self.store else {
            return;
        };
        if !self.book.wants_buckets(key) {
            return;
        }
        if let Err(error) = store.try_dispatch(ProcessHistoryJob::ReadRun(key.clone())) {
            self.warn(&format!("PROCESS_HISTORY_READ_NOT_SENT: {error}"));
        }
    }

    /// Takes what a store job did; the book's revision says whether the
    /// runs changed.
    pub(crate) fn on_completion(&mut self, completion: ProcessHistoryCompletion, now: Now) {
        match completion {
            ProcessHistoryCompletion::Loaded(result) => {
                self.load = Load::Done;
                match result {
                    Ok(load) => self.book.restore(load, now),
                    Err(error) => self.warn(&format!("PROCESS_HISTORY_LOAD_FAILED: {error}")),
                }
            }
            ProcessHistoryCompletion::Appended(result)
            | ProcessHistoryCompletion::Saved(result) => {
                self.writing = self.writing.saturating_sub(1);
                if let Err(error) = result {
                    self.warn(&format!("PROCESS_HISTORY_WRITE_FAILED: {error}"));
                }
            }
            ProcessHistoryCompletion::RunRead { key, result } => match result {
                Ok(rows) => self.book.on_buckets(&key, rows),
                Err(error) => self.warn(&format!(
                    "PROCESS_HISTORY_READ_FAILED: {}: {error}",
                    key.summary()
                )),
            },
        }
    }

    /// Sends the journal and the short-term file when due. Nothing leaves
    /// before the stores answered the load, so a restart never writes over
    /// what it has not read.
    pub(crate) fn tick(&mut self, now: Now) {
        if self.store.is_none() || self.load != Load::Done {
            return;
        }
        if now.boot_ns >= self.next_append_ns {
            self.next_append_ns = now.boot_ns.saturating_add(APPEND_EVERY_NS);
            self.append();
        }
        if now.boot_ns >= self.next_save_ns {
            self.next_save_ns = now.boot_ns.saturating_add(SAVE_EVERY_NS);
            self.save(now);
        }
    }

    /// Writes not answered yet (the shutdown waits for them, so their
    /// answers find the actor listening).
    pub(crate) fn writes_pending(&self) -> usize {
        self.writing
    }

    /// At shutdown: whatever is left, then the short-term file.
    pub(crate) fn flush(&mut self, now: Now) {
        if self.store.is_none() || self.load != Load::Done {
            return;
        }
        self.book.touch_live();
        self.append();
        self.save(now);
    }

    fn dispatch_probe(&mut self) {
        let Some(probe) = &self.probe else {
            return;
        };
        if self.probing {
            return;
        }
        let targets = self.book.probe_targets();
        if targets.is_empty() {
            return;
        }
        if probe.try_dispatch(ProcessProbeJob { targets }).is_ok() {
            self.probing = true;
        }
    }

    fn append(&mut self) {
        let mut journal = std::mem::take(&mut self.backlog);
        let fresh = self.book.take_journal();
        journal.runs.extend(fresh.runs);
        journal.buckets.extend(fresh.buckets);
        if journal.is_empty() {
            return;
        }
        let Some(store) = &self.store else {
            return;
        };
        match store.try_dispatch(ProcessHistoryJob::Append(journal.clone())) {
            Ok(()) => self.writing += 1,
            Err(error) => {
                self.warn(&format!("PROCESS_HISTORY_APPEND_DEFERRED: {error}"));
                let excess = journal.buckets.len().saturating_sub(MAX_BACKLOG_BUCKETS);
                journal.buckets.drain(..excess);
                self.backlog = journal;
            }
        }
    }

    fn save(&mut self, now: Now) {
        let Some(store) = &self.store else {
            return;
        };
        let recent = self.book.recent(now);
        match store.try_dispatch(ProcessHistoryJob::SaveRecent(recent)) {
            Ok(()) => self.writing += 1,
            Err(error) => self.warn(&format!("PROCESS_HISTORY_SAVE_DEFERRED: {error}")),
        }
    }

    fn warn(&self, message: &str) {
        self.logger.log(LogLevel::Warn, LOG_TARGET, message);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use dgxtop_core::process_history::ProcessHistoryLoad;
    use dgxtop_core::testing::{
        CapturingDispatch, IdleProbeDeriver, MemoryLogger, boot_id, gpu_process, process_table,
    };

    use super::*;

    const SECOND: u64 = NANOS_PER_SECOND;

    fn now(second: u64) -> Now {
        Now {
            boot_ns: second * SECOND,
            utc_ns: 1_789_999_800_000_000_000 + i128::from(second * SECOND),
        }
    }

    struct Rig {
        service: ProcessHistoryService,
        probe: CapturingDispatch<ProcessProbeJob>,
        store: CapturingDispatch<ProcessHistoryJob>,
    }

    fn rig() -> Rig {
        let probe = CapturingDispatch::new();
        let store = CapturingDispatch::new();
        let service = ProcessHistoryService::new(
            ServicePorts {
                probe: Some(Box::new(probe.clone())),
                probe_deriver: Arc::new(IdleProbeDeriver),
                store: Some(Box::new(store.clone())),
            },
            (boot_id("boot-test"), 0),
            Arc::new(MemoryLogger::default()),
            0,
        );
        Rig {
            service,
            probe,
            store,
        }
    }

    fn observe(rig: &mut Rig, second: u64) {
        let labels = std::collections::BTreeMap::new();
        let table = process_table(
            second,
            second * SECOND,
            vec![gpu_process(
                9,
                "acme_llm.train",
                &[("gpu:GPU-aaaa", Some(1 << 30))],
            )],
        );
        rig.service.on_table(
            &table,
            &SampleInputs {
                now: now(second),
                clk_tck: Some(100),
                gpu_labels: &labels,
                host_network: [None, None],
            },
        );
    }

    /// Nothing is written before the stores answered the load; then the
    /// journal and the short-term file leave on their cadences.
    #[test]
    fn writes_wait_for_the_load_then_follow_their_cadence() {
        let mut rig = rig();
        rig.service.start();
        assert!(matches!(rig.store.take()[..], [ProcessHistoryJob::Load]));
        observe(&mut rig, 1);
        rig.service.tick(now(40));
        assert!(rig.store.is_empty(), "the load is pending");
        rig.service.on_completion(
            ProcessHistoryCompletion::Loaded(Ok(ProcessHistoryLoad::default())),
            now(40),
        );
        rig.service.tick(now(41));
        let jobs = rig.store.take();
        assert!(matches!(jobs[0], ProcessHistoryJob::Append(_)), "{jobs:?}");
        assert!(
            matches!(jobs[1], ProcessHistoryJob::SaveRecent(_)),
            "{jobs:?}"
        );
        rig.service.tick(now(42));
        assert!(rig.store.is_empty(), "not due again");
        assert_eq!(rig.service.writes_pending(), 2, "two writes unanswered");
        for _ in 0..2 {
            rig.service
                .on_completion(ProcessHistoryCompletion::Saved(Ok(())), now(43));
        }
        assert_eq!(rig.service.writes_pending(), 0);
    }

    /// One probe at a time: the next waits for the report.
    #[test]
    fn one_probe_is_in_flight_at_a_time() {
        let mut rig = rig();
        observe(&mut rig, 1);
        observe(&mut rig, 2);
        assert_eq!(rig.probe.take().len(), 1);
        rig.service.on_probe(&ProbeReport::default(), now(2));
        observe(&mut rig, 3);
        assert_eq!(rig.probe.take().len(), 1);
    }

    /// A commit that brings the table already sampled skips the sample but
    /// still follows a finished probe with the next, as a new table would.
    #[test]
    fn a_sampled_table_still_lets_the_next_probe_leave() {
        let mut rig = rig();
        observe(&mut rig, 1);
        assert_eq!(rig.probe.take().len(), 1);
        let same = process_table(1, SECOND, Vec::new());
        assert!(rig.service.has_sampled(&same));
        rig.service.on_sampled_table();
        assert!(rig.probe.take().is_empty(), "the first probe is in flight");
        rig.service.on_probe(&ProbeReport::default(), now(1));
        rig.service.on_sampled_table();
        assert_eq!(rig.probe.take().len(), 1);
        assert!(
            !rig.service
                .has_sampled(&process_table(2, 2 * SECOND, Vec::new()))
        );
    }

    /// A full store queue keeps the journal for the next turn.
    #[test]
    fn a_refused_journal_is_kept_for_the_next_turn() {
        let mut rig = rig();
        observe(&mut rig, 1);
        rig.store.set_busy(true);
        rig.service.tick(now(11));
        rig.store.set_busy(false);
        observe(&mut rig, 12);
        rig.service.flush(now(12));
        let jobs = rig.store.take();
        let Some(ProcessHistoryJob::Append(journal)) = jobs.first() else {
            panic!("{jobs:?}");
        };
        assert_eq!(
            journal.runs.len(),
            2,
            "before the refusal, then at the flush"
        );
        assert_eq!(
            i128::from(journal.runs[1].last_seen_utc_ns),
            now(12).utc_ns,
            "the flush carries when a live run was last seen"
        );
    }
}
