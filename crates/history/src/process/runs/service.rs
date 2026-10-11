//! The process history service (D-76): the book, and the jobs that leave
//! for the probe and the stores. One probe is in flight at a time; the
//! journal leaves every [`APPEND_EVERY_NS`] and the short-term file is
//! replaced every [`SAVE_EVERY_NS`]. A store job refused for a full queue
//! keeps its journal for the next turn, within [`MAX_BACKLOG_BUCKETS`].
//!
//! An answer can be lost (the composition root drops one the actor's full
//! queue does not take in time), so every job sent has a deadline: a probe
//! report overdue lets the next probe leave, a load answer overdue is asked
//! for again and at last given up, and a write answer overdue stops being
//! waited for. Each is logged.

use std::collections::VecDeque;
use std::sync::Arc;

use dgxtop_core::action::Signal;
use dgxtop_core::canonical::ProcessTable;
use dgxtop_core::errors::InfraError;
use dgxtop_core::identity::{BootId, ProcessKey};
use dgxtop_core::ports::{
    Dispatch, LogLevel, Logger, ProbeDeriver, ProcessHistoryCompletion, ProcessHistoryJob,
    ProcessProbeJob,
};
use dgxtop_core::process_history::{ProcessHistoryJournal, StoreUsage};
use dgxtop_core::process_probe::ProbeReport;

use super::{ProcessHistoryBook, SampleInputs, retention};
use crate::process::Now;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
pub(crate) const APPEND_EVERY_NS: u64 = 10 * NANOS_PER_SECOND;
pub(crate) const SAVE_EVERY_NS: u64 = 30 * NANOS_PER_SECOND;
/// Buckets a backlog of refused journals keeps; older ones give way.
const MAX_BACKLOG_BUCKETS: usize = 8192;
/// A probe report not back after this long is taken as lost.
const PROBE_DEADLINE_NS: u64 = 5 * NANOS_PER_SECOND;
/// A load answer not back after this long is asked for again.
const LOAD_DEADLINE_NS: u64 = 30 * NANOS_PER_SECOND;
/// Loads asked for before the history goes on without what was kept.
const LOAD_ATTEMPTS: u32 = 3;
/// A write answer not back after this long is no longer waited for.
const WRITE_DEADLINE_NS: u64 = 60 * NANOS_PER_SECOND;
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
    Pending { sent_boot_ns: u64, attempts: u32 },
    Done,
}

/// A write sent: when it left, and whether it deletes runs.
#[derive(Debug, Clone, Copy)]
struct Sent {
    boot_ns: u64,
    deletes: bool,
}

/// The writes sent and not answered yet, by kind (one store worker answers
/// them in order).
#[derive(Debug, Default)]
struct PendingWrites {
    appends: VecDeque<Sent>,
    saves: VecDeque<Sent>,
}

impl PendingWrites {
    fn len(&self) -> usize {
        self.appends.len() + self.saves.len()
    }

    /// Whether an append unanswered deletes runs: until it is answered, the
    /// store's usage does not show it.
    fn deleting(&self) -> bool {
        self.appends.iter().any(|sent| sent.deletes)
    }

    /// Drops the writes sent before `cutoff_ns`; how many it dropped.
    fn drop_sent_before(&mut self, cutoff_ns: u64) -> usize {
        let mut dropped = 0;
        for queue in [&mut self.appends, &mut self.saves] {
            while queue.front().is_some_and(|sent| sent.boot_ns < cutoff_ns) {
                queue.pop_front();
                dropped += 1;
            }
        }
        dropped
    }
}

pub(crate) struct ProcessHistoryService {
    book: ProcessHistoryBook,
    probe: Option<Box<dyn Dispatch<ProcessProbeJob>>>,
    probe_deriver: Arc<dyn ProbeDeriver>,
    store: Option<Box<dyn Dispatch<ProcessHistoryJob>>>,
    logger: Arc<dyn Logger>,
    /// When the probe in flight left.
    probing_since_boot_ns: Option<u64>,
    /// Writes sent and not answered yet; the shutdown waits for them.
    writing: PendingWrites,
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
            probing_since_boot_ns: None,
            writing: PendingWrites::default(),
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
    pub(crate) fn start(&mut self, now: Now) {
        self.ask_load(now, 1);
    }

    /// Sends the load, its `attempts`-th; one refused waits for its
    /// deadline like one lost.
    fn ask_load(&mut self, now: Now, attempts: u32) {
        let Some(store) = &self.store else {
            return;
        };
        self.load = Load::Pending {
            sent_boot_ns: now.boot_ns,
            attempts,
        };
        let request = retention::startup_request();
        if let Err(error) = store.try_dispatch(ProcessHistoryJob::Load(request)) {
            self.warn(&format!("PROCESS_HISTORY_LOAD_NOT_SENT: {error}"));
        }
    }

    /// Samples the live runs from `table` and probes them.
    pub(crate) fn on_table(&mut self, table: &ProcessTable, inputs: &SampleInputs<'_>) {
        self.book.observe(table, inputs);
        self.dispatch_probe(inputs.now);
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
    pub(crate) fn on_sampled_table(&mut self, now: Now) {
        self.dispatch_probe(now);
    }

    pub(crate) fn on_probe(&mut self, report: &ProbeReport, now: Now) {
        self.probing_since_boot_ns = None;
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
        let read = ProcessHistoryJob::ReadRun {
            key: key.clone(),
            buckets: retention::held_buckets(),
        };
        if let Err(error) = store.try_dispatch(read) {
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
            ProcessHistoryCompletion::Appended(result) => {
                self.writing.appends.pop_front();
                match result {
                    Ok(usage) => self.on_usage(usage),
                    Err(error) => self.warn(&format!("PROCESS_HISTORY_WRITE_FAILED: {error}")),
                }
            }
            ProcessHistoryCompletion::Saved(result) => {
                self.writing.saves.pop_front();
                self.warn_failed_write(result);
            }
            ProcessHistoryCompletion::RunRead { key, result } => match result {
                Ok(rows) => self.book.on_buckets(&key, rows),
                Err(error) => {
                    self.book.on_read_failed(&key);
                    self.warn(&format!(
                        "PROCESS_HISTORY_READ_FAILED: {}: {error}",
                        key.summary()
                    ));
                }
            },
        }
    }

    /// Sends the journal and the short-term file when due. Nothing leaves
    /// before the stores answered the load, so a restart never writes over
    /// what it has not read.
    pub(crate) fn tick(&mut self, now: Now) {
        if self.store.is_none() {
            return;
        }
        self.expire_writes(now);
        self.expire_load(now);
        if self.load != Load::Done {
            return;
        }
        if now.boot_ns >= self.next_append_ns {
            self.next_append_ns = now.boot_ns.saturating_add(APPEND_EVERY_NS);
            self.append(now);
        }
        if now.boot_ns >= self.next_save_ns {
            self.next_save_ns = now.boot_ns.saturating_add(SAVE_EVERY_NS);
            self.save(now);
        }
    }

    /// Writes not answered yet (the shutdown waits for them, so their
    /// answers find the actor listening).
    pub(crate) fn writes_pending(&self) -> usize {
        self.writing.len()
    }

    /// At shutdown: whatever is left, then the short-term file.
    pub(crate) fn flush(&mut self, now: Now) {
        if self.store.is_none() || self.load != Load::Done {
            return;
        }
        self.book.touch_live();
        self.append(now);
        self.save(now);
    }

    fn dispatch_probe(&mut self, now: Now) {
        let Some(probe) = &self.probe else {
            return;
        };
        if let Some(since) = self.probing_since_boot_ns {
            if now.boot_ns.saturating_sub(since) < PROBE_DEADLINE_NS {
                return;
            }
            self.warn("PROCESS_HISTORY_PROBE_LOST: no report; probing again");
        }
        let targets = self.book.probe_targets();
        if targets.is_empty() {
            return;
        }
        self.probing_since_boot_ns = probe
            .try_dispatch(ProcessProbeJob { targets })
            .is_ok()
            .then_some(now.boot_ns);
    }

    /// A load overdue is asked for again; after the last attempt the
    /// history goes on without what was kept, as after a failed load.
    fn expire_load(&mut self, now: Now) {
        let Load::Pending {
            sent_boot_ns,
            attempts,
        } = self.load
        else {
            return;
        };
        if now.boot_ns.saturating_sub(sent_boot_ns) < LOAD_DEADLINE_NS {
            return;
        }
        if attempts < LOAD_ATTEMPTS {
            self.ask_load(now, attempts + 1);
            return;
        }
        self.load = Load::Done;
        self.warn(&format!(
            "PROCESS_HISTORY_LOAD_LOST: no answer to {attempts} loads; going on without them"
        ));
    }

    fn expire_writes(&mut self, now: Now) {
        let lost = self
            .writing
            .drop_sent_before(now.boot_ns.saturating_sub(WRITE_DEADLINE_NS));
        if lost > 0 {
            self.warn(&format!(
                "PROCESS_HISTORY_WRITE_LOST: {lost} writes unanswered after {}s",
                WRITE_DEADLINE_NS / NANOS_PER_SECOND
            ));
        }
    }

    /// What the store holds after an append: past the budget the ended
    /// runs last seen longest ago give way, and runs the book no longer
    /// lists are cut (D-94). Not while deletions are on their way: the
    /// usage would not show them yet.
    fn on_usage(&mut self, usage: StoreUsage) {
        if self.writing.deleting()
            || self.book.has_unsent_deletions()
            || has_deletions(&self.backlog)
        {
            return;
        }
        self.book.keep_stored_runs(usage);
        let gave_way = self.book.give_way(usage, retention::STORED_BUDGET_BYTES);
        if gave_way > 0 {
            self.logger.log(
                LogLevel::Info,
                LOG_TARGET,
                &format!(
                    "PROCESS_HISTORY_RUNS_GIVE_WAY: {gave_way} ended runs; the store uses {} \
                     of {} bytes",
                    usage.used_bytes,
                    retention::STORED_BUDGET_BYTES
                ),
            );
        }
    }

    fn warn_failed_write(&self, result: Result<(), InfraError>) {
        if let Err(error) = result {
            self.warn(&format!("PROCESS_HISTORY_WRITE_FAILED: {error}"));
        }
    }

    fn append(&mut self, now: Now) {
        let mut journal = std::mem::take(&mut self.backlog);
        let fresh = self.book.take_journal();
        journal.runs.extend(fresh.runs);
        journal.buckets.extend(fresh.buckets);
        journal.keep_latest.extend(fresh.keep_latest);
        journal.forget.extend(fresh.forget);
        journal.keep_runs = fresh.keep_runs.or(journal.keep_runs);
        if journal.is_empty() {
            return;
        }
        let Some(store) = &self.store else {
            return;
        };
        let sent = Sent {
            boot_ns: now.boot_ns,
            deletes: has_deletions(&journal),
        };
        match store.try_dispatch(ProcessHistoryJob::Append(journal.clone())) {
            Ok(()) => self.writing.appends.push_back(sent),
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
        let sent = Sent {
            boot_ns: now.boot_ns,
            deletes: false,
        };
        match store.try_dispatch(ProcessHistoryJob::SaveRecent(recent)) {
            Ok(()) => self.writing.saves.push_back(sent),
            Err(error) => self.warn(&format!("PROCESS_HISTORY_SAVE_DEFERRED: {error}")),
        }
    }

    fn warn(&self, message: &str) {
        self.logger.log(LogLevel::Warn, LOG_TARGET, message);
    }
}

/// Whether `journal` deletes runs.
fn has_deletions(journal: &ProcessHistoryJournal) -> bool {
    !journal.forget.is_empty() || journal.keep_runs.is_some()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use dgxtop_core::process_history::{MAX_LISTED_RUNS, ProcessHistoryLoad};
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
        logger: Arc<MemoryLogger>,
    }

    fn rig() -> Rig {
        let probe = CapturingDispatch::new();
        let store = CapturingDispatch::new();
        let logger = Arc::new(MemoryLogger::default());
        let service = ProcessHistoryService::new(
            ServicePorts {
                probe: Some(Box::new(probe.clone())),
                probe_deriver: Arc::new(IdleProbeDeriver),
                store: Some(Box::new(store.clone())),
            },
            (boot_id("boot-test"), 0),
            Arc::clone(&logger) as Arc<dyn Logger>,
            0,
        );
        Rig {
            service,
            probe,
            store,
            logger,
        }
    }

    /// The codes logged so far.
    fn logged(rig: &Rig) -> Vec<String> {
        rig.logger
            .records()
            .into_iter()
            .map(|(_, target, message)| {
                assert_eq!(target, LOG_TARGET);
                message.split(':').next().unwrap_or_default().to_owned()
            })
            .collect()
    }

    /// The stores answered the load with nothing kept.
    fn load_answered(rig: &mut Rig, second: u64) {
        rig.service.start(now(0));
        rig.store.take();
        rig.service.on_completion(
            ProcessHistoryCompletion::Loaded(Ok(ProcessHistoryLoad::default())),
            now(second),
        );
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
        rig.service.start(now(0));
        assert!(matches!(rig.store.take()[..], [ProcessHistoryJob::Load(_)]));
        observe(&mut rig, 1);
        rig.service.tick(now(20));
        assert!(rig.store.is_empty(), "the load is pending");
        rig.service.on_completion(
            ProcessHistoryCompletion::Loaded(Ok(ProcessHistoryLoad::default())),
            now(20),
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
        rig.service.on_completion(
            ProcessHistoryCompletion::Appended(Ok(StoreUsage::default())),
            now(43),
        );
        rig.service
            .on_completion(ProcessHistoryCompletion::Saved(Ok(())), now(43));
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
        rig.service.on_sampled_table(now(1));
        assert!(rig.probe.take().is_empty(), "the first probe is in flight");
        rig.service.on_probe(&ProbeReport::default(), now(1));
        rig.service.on_sampled_table(now(1));
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

    /// A probe report that never came back (dropped on a full queue) does
    /// not stop the probing: the next probe leaves once the report is
    /// overdue.
    #[test]
    fn a_lost_probe_report_is_followed_by_the_next_probe() {
        let mut rig = rig();
        observe(&mut rig, 1);
        assert_eq!(rig.probe.take().len(), 1);
        observe(&mut rig, 3);
        rig.service.on_sampled_table(now(5));
        assert!(rig.probe.is_empty(), "the report is not overdue yet");
        observe(&mut rig, 7);
        assert_eq!(rig.probe.take().len(), 1, "probed again");
        assert!(logged(&rig).contains(&"PROCESS_HISTORY_PROBE_LOST".to_owned()));
    }

    /// A load answer that never came back is asked for again, and after
    /// the last attempt counts as failed, so the writes are not held for
    /// the rest of the run.
    #[test]
    fn a_lost_load_answer_is_asked_again_then_given_up() {
        let mut rig = rig();
        rig.service.start(now(0));
        assert!(matches!(rig.store.take()[..], [ProcessHistoryJob::Load(_)]));
        observe(&mut rig, 1);
        rig.service.tick(now(20));
        assert!(rig.store.is_empty(), "not overdue yet");
        rig.service.tick(now(31));
        assert!(matches!(rig.store.take()[..], [ProcessHistoryJob::Load(_)]));
        rig.service.tick(now(62));
        assert!(matches!(rig.store.take()[..], [ProcessHistoryJob::Load(_)]));
        rig.service.tick(now(93));
        let jobs = rig.store.take();
        assert!(
            jobs.iter()
                .any(|job| matches!(job, ProcessHistoryJob::Append(_))),
            "the writes go: {jobs:?}"
        );
        assert!(logged(&rig).contains(&"PROCESS_HISTORY_LOAD_LOST".to_owned()));
    }

    /// A write answer that never came back stops counting as pending once
    /// overdue, so the shutdown does not wait for it.
    #[test]
    fn a_lost_write_answer_stops_being_pending() {
        let mut rig = rig();
        load_answered(&mut rig, 1);
        observe(&mut rig, 1);
        rig.service.tick(now(31));
        assert_eq!(rig.store.take().len(), 2, "the journal and the file");
        assert_eq!(rig.service.writes_pending(), 2);
        rig.service.tick(now(61));
        assert_eq!(rig.store.take().len(), 1, "the file again");
        assert_eq!(rig.service.writes_pending(), 3, "none overdue yet");
        rig.service.tick(now(92));
        assert_eq!(rig.store.take().len(), 1, "the file again");
        assert_eq!(
            rig.service.writes_pending(),
            2,
            "the first two are overdue; the two sent since are pending"
        );
        assert!(logged(&rig).contains(&"PROCESS_HISTORY_WRITE_LOST".to_owned()));
    }

    /// An ended run the stores kept, last seen `last_seen_s` seconds after
    /// the tests' boot.
    fn ended_run(pid: i64, last_seen_s: u64) -> dgxtop_core::process_history::RunInfo {
        let seen = i64::try_from(now(last_seen_s).utc_ns).unwrap();
        dgxtop_core::process_history::RunInfo {
            key: dgxtop_core::testing::test_process_key(pid),
            name: "acme_llm.eval".to_owned(),
            command: String::new(),
            cwd: String::new(),
            uid: None,
            gpus: Vec::new(),
            details: dgxtop_core::process_history::RunDetails::default(),
            started_utc_ns: seen,
            last_seen_utc_ns: seen,
            end: Some(dgxtop_core::process_history::RunEnd {
                utc_ns: seen,
                cause: dgxtop_core::process_history::EndCause::Exit(0),
            }),
        }
    }

    /// A store just over the history's budget, one run's worth.
    fn over_budget() -> ProcessHistoryCompletion {
        ProcessHistoryCompletion::Appended(Ok(StoreUsage {
            used_bytes: retention::STORED_BUDGET_BYTES + 1,
            runs: 4,
        }))
    }

    /// The journals sent since the last call: what each forgets.
    fn forgotten(rig: &Rig) -> Vec<Vec<i32>> {
        rig.store
            .take()
            .into_iter()
            .filter_map(|job| match job {
                ProcessHistoryJob::Append(journal) => {
                    Some(journal.forget.iter().map(|key| key.pid.get()).collect())
                }
                _ => None,
            })
            .collect()
    }

    /// Past its byte budget the history lets the ended run last seen
    /// longest ago go, from the list and the store (D-94); while that
    /// deletion is unanswered, another usage over the budget (an append
    /// sent before it) lets nothing more go; its answer decides again.
    #[test]
    fn over_the_budget_ended_runs_give_way_once_per_answered_deletion() {
        let mut rig = rig();
        rig.service.start(now(0));
        rig.store.take();
        rig.service.on_completion(
            ProcessHistoryCompletion::Loaded(Ok(ProcessHistoryLoad {
                runs: vec![ended_run(1, 1), ended_run(2, 2), ended_run(3, 3)],
                ..ProcessHistoryLoad::default()
            })),
            now(4),
        );
        observe(&mut rig, 5);
        rig.service.flush(now(5));
        rig.service.flush(now(6));
        assert_eq!(forgotten(&rig), vec![Vec::<i32>::new(), Vec::new()]);
        rig.service.on_completion(over_budget(), now(7));
        assert!(rig.service.book().run(&key_of(1)).is_none(), "gave way");
        rig.service.flush(now(8));
        assert_eq!(forgotten(&rig), vec![vec![1]]);
        rig.service.on_completion(over_budget(), now(9));
        assert!(
            rig.service.book().run(&key_of(2)).is_some(),
            "the deletion is not answered yet"
        );
        rig.service.on_completion(over_budget(), now(10));
        rig.service.flush(now(11));
        assert_eq!(forgotten(&rig), vec![vec![2]]);
        assert!(rig.service.book().run(&key_of(3)).is_some());
        assert!(
            rig.service.book().run(&key_of(9)).is_some(),
            "the live run stays"
        );
        let gave_way = logged(&rig)
            .into_iter()
            .filter(|code| code == "PROCESS_HISTORY_RUNS_GIVE_WAY")
            .count();
        assert_eq!(gave_way, 2);
    }

    /// Within the budget nothing gives way; a store holding more runs than
    /// the history lists is cut to them.
    #[test]
    fn within_the_budget_only_unlisted_runs_are_cut() {
        let mut rig = rig();
        load_answered(&mut rig, 1);
        observe(&mut rig, 1);
        rig.service.flush(now(2));
        rig.store.take();
        rig.service.on_completion(
            ProcessHistoryCompletion::Appended(Ok(StoreUsage {
                used_bytes: 1,
                runs: 600,
            })),
            now(3),
        );
        rig.service.flush(now(4));
        let jobs = rig.store.take();
        let Some(ProcessHistoryJob::Append(journal)) = jobs.first() else {
            panic!("{jobs:?}");
        };
        assert_eq!(journal.keep_runs, Some(MAX_LISTED_RUNS));
        assert!(journal.forget.is_empty());
    }

    fn key_of(pid: i64) -> ProcessKey {
        dgxtop_core::testing::test_process_key(pid)
    }
}
