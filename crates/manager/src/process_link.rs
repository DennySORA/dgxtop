//! The Manager's side of the history's process actor (D-79): it hands the
//! actor what it needs and reads what it publishes, and holds none of its
//! data. A committed table goes once (every commit of a cycle carries the
//! snapshot's table); a commit without a new one lets a finished probe be
//! followed by the next; a run a session shows is asked for until the
//! model answers it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use dgxtop_core::action::Signal;
use dgxtop_core::canonical::ProcessTable;
use dgxtop_core::identity::{EntityId, ProcessKey, SessionId};
use dgxtop_core::ports::{
    Dispatch, ProcessHistoryInput, ProcessHistoryReader, ProcessMemoryReader,
};
use dgxtop_core::process_model::{MemoryPart, ProcessHistoryModel, ProcessTableSample, RunRequest};

/// A request the model does not answer yet is asked again after this long
/// (the actor's queue may have been full).
const ASK_AGAIN_NS: u64 = 1_000_000_000;

/// The ports of the process actor (the composition root wires them).
pub struct ProcessHistoryHandles {
    pub inputs: Box<dyn Dispatch<ProcessHistoryInput>>,
    pub runs: Arc<dyn ProcessHistoryReader>,
    pub memory: Arc<dyn ProcessMemoryReader>,
}

/// The Manager's link to the process actor (module docs).
pub(crate) struct ProcessHistoryLink {
    inputs: Option<Box<dyn Dispatch<ProcessHistoryInput>>>,
    runs: Arc<dyn ProcessHistoryReader>,
    memory: Arc<dyn ProcessMemoryReader>,
    /// The table handed over last (cycle, read time).
    last_table: Option<(u64, u64)>,
    /// What each session asked for last, and when.
    asked: BTreeMap<SessionId, (RunRequest, u64)>,
    flushes: u64,
    /// A flush asked for and refused for a full queue, sent again while the
    /// shutdown waits.
    flush_owed: bool,
    seen_revision: u64,
}

impl ProcessHistoryLink {
    /// `None`: no process history (tests and tools without one).
    pub(crate) fn new(handles: Option<ProcessHistoryHandles>) -> Self {
        let (inputs, runs, memory) = match handles {
            Some(handles) => (Some(handles.inputs), handles.runs, handles.memory),
            None => {
                let none = Arc::new(NoProcessHistory::default());
                (
                    None,
                    Arc::clone(&none) as Arc<dyn ProcessHistoryReader>,
                    none as Arc<dyn ProcessMemoryReader>,
                )
            }
        };
        Self {
            inputs,
            runs,
            memory,
            last_table: None,
            asked: BTreeMap::new(),
            flushes: 0,
            flush_owed: false,
            seen_revision: 0,
        }
    }

    /// The model published last.
    pub(crate) fn model(&self) -> Arc<ProcessHistoryModel> {
        self.runs.latest()
    }

    pub(crate) fn memory(&self) -> &dyn ProcessMemoryReader {
        self.memory.as_ref()
    }

    /// Whether `table` was handed over already, so the caller can skip
    /// building its sample.
    pub(crate) fn has_table(&self, table: &ProcessTable) -> bool {
        self.last_table == Some(table_identity(table))
    }

    /// Hands over a new table; one refused for a full queue goes with the
    /// next commit.
    pub(crate) fn send_table(&mut self, sample: ProcessTableSample) {
        let identity = table_identity(&sample.table);
        if self.send(ProcessHistoryInput::Table(Box::new(sample))) {
            self.last_table = Some(identity);
        }
    }

    /// A commit without a new table.
    pub(crate) fn tick(&self) {
        // A tick refused for a full queue changes nothing: the queue holds
        // inputs that tick too.
        if let Some(inputs) = &self.inputs {
            let _ = inputs.try_dispatch(ProcessHistoryInput::Tick);
        }
    }

    /// dgxtop sent `signal` to `key`.
    pub(crate) fn note_signal(&self, key: &ProcessKey, signal: Signal) {
        self.send(ProcessHistoryInput::Signal {
            key: key.clone(),
            signal,
        });
    }

    /// `session` shows `request` (`None`: no run): asked for while the
    /// model does not answer it, at most once a second.
    pub(crate) fn show(&mut self, session: &SessionId, request: Option<RunRequest>, now_ns: u64) {
        let Some(request) = request else {
            self.asked.remove(session);
            return;
        };
        if self.inputs.is_none() || self.runs.latest().answers(&request) {
            return;
        }
        let due = self.asked.get(session).is_none_or(|(asked, at)| {
            *asked != request || now_ns.saturating_sub(*at) >= ASK_AGAIN_NS
        });
        if due {
            self.send(ProcessHistoryInput::Show(request.clone()));
            self.asked.insert(session.clone(), (request, now_ns));
        }
    }

    /// A session closed.
    pub(crate) fn forget(&mut self, session: &SessionId) {
        self.asked.remove(session);
    }

    /// At shutdown: whatever is left goes to the stores.
    pub(crate) fn flush(&mut self) {
        if self.inputs.is_none() {
            return;
        }
        if self.send(ProcessHistoryInput::Flush) {
            self.flushes += 1;
            self.flush_owed = false;
        } else {
            self.flush_owed = true;
        }
    }

    /// Whether the flush asked for was handled and its writes answered;
    /// a flush the full queue refused is sent again first (the shutdown
    /// waits for it, within its bound).
    pub(crate) fn settled(&mut self) -> bool {
        if self.flush_owed {
            self.flush();
        }
        !self.flush_owed && self.runs.latest().writes.settled_after(self.flushes)
    }

    /// Whether a new model was published since the last call.
    pub(crate) fn take_changed(&mut self) -> bool {
        let revision = self.runs.latest().revision;
        let changed = revision != self.seen_revision;
        self.seen_revision = revision;
        changed
    }

    fn send(&self, input: ProcessHistoryInput) -> bool {
        self.inputs
            .as_ref()
            .is_some_and(|inputs| inputs.try_dispatch(input).is_ok())
    }
}

/// What tells one table from the next: its cycle and read time.
fn table_identity(table: &ProcessTable) -> (u64, u64) {
    (table.cycle_id, table.read_finished_boot_ns)
}

/// The process history of a setup without one: no runs, no colors.
#[derive(Debug, Default)]
struct NoProcessHistory {
    model: Arc<ProcessHistoryModel>,
}

impl ProcessHistoryReader for NoProcessHistory {
    fn latest(&self) -> Arc<ProcessHistoryModel> {
        Arc::clone(&self.model)
    }
}

impl ProcessMemoryReader for NoProcessHistory {
    fn parts(
        &self,
        _gpus: &BTreeSet<EntityId>,
        _window: (u64, u64),
        _cells: usize,
    ) -> Vec<MemoryPart> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests;
