//! The history's process actor (D-76, D-79) and the workers that answer
//! it: the probe of the followed GPU processes and the process history's
//! stores. The actor is spawned after them, since it holds their handles,
//! so their answers reach it through a [`ProcessInbox`] set once it runs.

use std::sync::{Arc, OnceLock};

use dgxtop_adapters::common::RootedFiles;
use dgxtop_adapters::procfs::ProcfsProcessProbe;
use dgxtop_collectors::process::ProcessProbeDeriver;
use dgxtop_core::config::Config;
use dgxtop_core::identity::BootId;
use dgxtop_core::ports::{
    Dispatch, LogLevel, Logger, ProcessHistoryCompletion, ProcessHistoryInput, ProcessHistoryJob,
    ProcessHistoryReader, ProcessHistoryStore, ProcessMemoryReader, ProcessProbe, ProcessProbeJob,
};
use dgxtop_core::quality::Namespace;
use dgxtop_history::{ProcessHistory, ProcessHistoryPorts, ProcessHistorySettings};
use dgxtop_manager::ProcessHistoryHandles;
use dgxtop_runtime::queues::PoolHandle;
use dgxtop_store::paths::StatePaths;
use dgxtop_store::process_history::ProcessHistoryFiles;

use super::StartError;
use super::assemble::Wiring;
use super::workers::{COMPLETION_SEND_TIMEOUT, Workers, log_recreated};
use crate::worker_queues::{PROCESS_HISTORY_INPUTS, PROCESS_HISTORY_QUEUE, PROCESS_PROBE_QUEUE};

/// Where the process probe and the process history's stores send what
/// they did: the history's process actor, set once it is spawned.
pub(super) type ProcessInbox = Arc<OnceLock<PoolHandle<ProcessHistoryInput>>>;

/// Where a dropped process answer is logged: beside the process actor's own
/// lines, whose deadlines then recover from it.
const PROCESS_HISTORY_LOG_TARGET: &str = "history.process";

/// What the process history hands the Manager and the history's readers.
pub(super) struct ProcessHistoryParts {
    pub(super) handles: ProcessHistoryHandles,
    pub(super) memory: Arc<dyn ProcessMemoryReader>,
}

impl Wiring {
    /// The process history with its probe and stores, answers wired to the
    /// actor (D-76, D-79).
    pub(super) fn spawn_process_history_parts(
        &self,
        effective: &Config,
        paths: &StatePaths,
        (boot_id, utc_offset_s): (BootId, i32),
        workers: &mut Workers,
    ) -> Result<ProcessHistoryParts, StartError> {
        let inbox: ProcessInbox = Arc::new(OnceLock::new());
        let probe = self.spawn_process_probe(&inbox, workers)?;
        let store = self.spawn_process_history_store(effective, paths, &inbox, workers)?;
        self.spawn_process_history((probe, store), (boot_id, utc_offset_s), &inbox, workers)
    }

    /// The process history's probe (D-76): live processes only, so none in
    /// a replay. Its reports go to the history's process actor.
    fn spawn_process_probe(
        &self,
        inbox: &ProcessInbox,
        workers: &mut Workers,
    ) -> Result<Option<PoolHandle<ProcessProbeJob>>, StartError> {
        if self.namespace != Namespace::Live {
            return Ok(None);
        }
        let probe = ProcfsProcessProbe::new(Arc::new(RootedFiles::host()), Arc::clone(&self.clock));
        let inbox = Arc::clone(inbox);
        let logger = Arc::clone(&self.logger);
        let handle = workers.actor(
            "process-probe",
            PROCESS_PROBE_QUEUE,
            probe,
            move |probe: &mut ProcfsProcessProbe, job: ProcessProbeJob| {
                let report = probe.probe(&job.targets);
                deliver_process_input(&inbox, ProcessHistoryInput::Probe(report), &logger);
            },
        )?;
        Ok(Some(handle))
    }

    /// The history's process actor (D-79): the GPU process runs and
    /// memory, fed by the Manager, answered by the probe and the stores,
    /// read by the Manager through what it publishes; its memory record
    /// goes to the history's readers.
    fn spawn_process_history(
        &self,
        (probe, store): (
            Option<PoolHandle<ProcessProbeJob>>,
            Option<PoolHandle<ProcessHistoryJob>>,
        ),
        (boot_id, utc_offset_s): (BootId, i32),
        inbox: &ProcessInbox,
        workers: &mut Workers,
    ) -> Result<ProcessHistoryParts, StartError> {
        let (actor, published) = ProcessHistory::new(
            ProcessHistorySettings {
                boot_id,
                utc_offset_s,
            },
            ProcessHistoryPorts {
                probe: probe.map(|port| Box::new(port) as Box<dyn Dispatch<ProcessProbeJob>>),
                probe_deriver: Arc::new(ProcessProbeDeriver),
                store: store.map(|port| Box::new(port) as Box<dyn Dispatch<ProcessHistoryJob>>),
                clock: Arc::clone(&self.clock),
                logger: Arc::clone(&self.logger),
            },
        );
        let handle = workers.actor(
            "process-history",
            PROCESS_HISTORY_INPUTS,
            actor,
            |actor: &mut ProcessHistory, input: ProcessHistoryInput| actor.handle(input),
        )?;
        // The probe and the stores answer only jobs the actor sends, so the
        // inbox is set before any answer can come.
        let _ = inbox.set(handle.clone());
        let published = Arc::new(published);
        Ok(ProcessHistoryParts {
            handles: ProcessHistoryHandles {
                inputs: Box::new(handle),
                runs: Arc::clone(&published) as Arc<dyn ProcessHistoryReader>,
            },
            memory: published,
        })
    }

    /// The process history's stores (D-76), on with
    /// `store.process_history_enabled`, unset following the history archive
    /// (D-80): without them the runs live only while dgxtop runs. What a
    /// job did goes to the history's process actor.
    fn spawn_process_history_store(
        &self,
        effective: &Config,
        paths: &StatePaths,
        inbox: &ProcessInbox,
        workers: &mut Workers,
    ) -> Result<Option<PoolHandle<ProcessHistoryJob>>, StartError> {
        if !(effective.store.process_history_on_disk() && self.sqlite_ok) {
            return Ok(None);
        }
        let store =
            ProcessHistoryFiles::open(&paths.process_history_database, &paths.process_recent_file);
        log_recreated(
            self.logger.as_ref(),
            &paths.process_history_database,
            store.recreated(),
        );
        let inbox = Arc::clone(inbox);
        let logger = Arc::clone(&self.logger);
        let handle = workers.actor(
            "process-history-store",
            PROCESS_HISTORY_QUEUE,
            store,
            move |store: &mut ProcessHistoryFiles, job: ProcessHistoryJob| {
                let completion = run_process_store_job(store, job);
                deliver_process_input(&inbox, ProcessHistoryInput::Store(completion), &logger);
            },
        )?;
        Ok(Some(handle))
    }
}

/// What the process history's stores did with `job`.
fn run_process_store_job(
    store: &mut ProcessHistoryFiles,
    job: ProcessHistoryJob,
) -> ProcessHistoryCompletion {
    match job {
        ProcessHistoryJob::Load(request) => ProcessHistoryCompletion::Loaded(store.load(&request)),
        ProcessHistoryJob::Append(journal) => {
            ProcessHistoryCompletion::Appended(store.append(&journal))
        }
        ProcessHistoryJob::SaveRecent(recent) => {
            ProcessHistoryCompletion::Saved(store.save_recent(&recent))
        }
        ProcessHistoryJob::ReadRun { key, buckets } => {
            let result = store.read_run(&key, &buckets);
            ProcessHistoryCompletion::RunRead { key, result }
        }
    }
}

/// Hands a probe report or a store answer to the process actor, waiting a
/// bounded time while its queue is full: the actor never waits on these
/// workers, so this cannot deadlock. An answer still not taken is dropped
/// and logged; the actor's deadlines stop it waiting for it.
fn deliver_process_input(
    inbox: &ProcessInbox,
    input: ProcessHistoryInput,
    logger: &Arc<dyn Logger>,
) {
    let answer = answer_name(&input);
    let delivered = inbox.get().is_some_and(|actor| {
        actor
            .dispatch_within(input, COMPLETION_SEND_TIMEOUT)
            .is_ok()
    });
    if !delivered {
        logger.log(
            LogLevel::Error,
            PROCESS_HISTORY_LOG_TARGET,
            &format!(
                "PROCESS_HISTORY_ANSWER_DROPPED: the process actor is full or stopped; \
                 its {answer} was dropped"
            ),
        );
    }
}

/// What answer `input` is, for the log.
fn answer_name(input: &ProcessHistoryInput) -> &'static str {
    match input {
        ProcessHistoryInput::Probe(_) => "probe report",
        ProcessHistoryInput::Store(ProcessHistoryCompletion::Loaded(_)) => "load answer",
        ProcessHistoryInput::Store(ProcessHistoryCompletion::Appended(_)) => "append answer",
        ProcessHistoryInput::Store(ProcessHistoryCompletion::Saved(_)) => "save answer",
        ProcessHistoryInput::Store(ProcessHistoryCompletion::RunRead { .. }) => "run read answer",
        _ => "input",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer the process actor cannot take is logged as dropped under
    /// the process history's target, naming the answer.
    #[test]
    fn a_dropped_process_answer_is_logged_by_the_process_history() {
        let memory = Arc::new(dgxtop_core::testing::MemoryLogger::default());
        let logger = Arc::clone(&memory) as Arc<dyn Logger>;
        let inbox: ProcessInbox = Arc::new(OnceLock::new());
        deliver_process_input(
            &inbox,
            ProcessHistoryInput::Store(ProcessHistoryCompletion::Appended(Ok(Default::default()))),
            &logger,
        );
        let records = memory.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, LogLevel::Error);
        assert_eq!(records[0].1, "history.process");
        assert!(
            records[0].2.starts_with("PROCESS_HISTORY_ANSWER_DROPPED: "),
            "{records:?}"
        );
        assert!(records[0].2.contains("append answer"), "{records:?}");
    }
}
