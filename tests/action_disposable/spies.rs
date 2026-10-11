//! Transparent spies around the real implementations. Each one delegates to
//! the production code unchanged and only journals what crossed it, plus the
//! few explicit fault switches the scenarios need (a held audit receipt, a
//! crash right after COMMIT, a stalled send, a full disk, a kernel without
//! pidfd). Nothing here can send a signal on its own.

use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dgxtop_adapters::process_action::{PidfdActionAdapter, PidfdReadiness, PidfdSyscalls};
use dgxtop_core::action::{
    ActionAttempt, ActionError, ActionHandle, ActionIntent, ActionOp, ActionOutcomeRecord,
    AuditExpiryReport, AuditFailure, CapabilityContext, DurableReceipt, ExitObservation,
    PersistenceKind, PersistenceState, PersistenceStatus, PreparedProcess, RecoveredIntent, Signal,
    SignalOutcome,
};
use dgxtop_core::errors::{ErrorCode, InfraError};
use dgxtop_core::identity::ProcessKey;
use dgxtop_core::ports::{AuditRepository, Dispatch, DispatchError, ProcessActionPort};
use dgxtop_runtime::queues::PoolHandle;
use dgxtop_store::sqlite::{FreeSpaceProbe, StatvfsFreeSpace};
use nix::errno::Errno;

use crate::journal::{Journal, Step, WorkCounter};

// ----- kernel boundary ------------------------------------------------------

/// Journals every pidfd system call, including the descriptor's target as
/// the kernel reports it in `/proc/self/fdinfo`, then performs it.
pub struct SyscallSpy<K> {
    kernel: K,
    journal: Journal,
}

impl<K> SyscallSpy<K> {
    pub fn new(kernel: K, journal: Journal) -> Self {
        Self { kernel, journal }
    }
}

impl<K: PidfdSyscalls> PidfdSyscalls for SyscallSpy<K> {
    fn open(&self, pid: i32) -> Result<OwnedFd, Errno> {
        let opened = self.kernel.open(pid);
        self.journal.record(Step::PidfdOpened {
            pid,
            fd: opened.as_ref().ok().map(AsRawFd::as_raw_fd),
        });
        opened
    }

    fn send_signal(&self, pidfd: BorrowedFd<'_>, signal: i32) -> Result<(), Errno> {
        self.journal.record(Step::SignalSyscall {
            fd: pidfd.as_raw_fd(),
            target_pid: pidfd_target(pidfd.as_raw_fd()),
            signal,
        });
        self.kernel.send_signal(pidfd, signal)
    }

    fn readiness(&self, pidfd: BorrowedFd<'_>) -> Result<PidfdReadiness, Errno> {
        self.kernel.readiness(pidfd)
    }
}

/// A kernel without `pidfd_open` (older than 5.3, or filtered by seccomp).
pub struct KernelWithoutPidfd;

impl PidfdSyscalls for KernelWithoutPidfd {
    fn open(&self, _pid: i32) -> Result<OwnedFd, Errno> {
        Err(Errno::ENOSYS)
    }

    fn send_signal(&self, _pidfd: BorrowedFd<'_>, _signal: i32) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    fn readiness(&self, _pidfd: BorrowedFd<'_>) -> Result<PidfdReadiness, Errno> {
        Err(Errno::ENOSYS)
    }
}

/// `Pid:` of `/proc/self/fdinfo/<fd>`: the process a pidfd refers to.
fn pidfd_target(fd: RawFd) -> Option<i64> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).ok()?;
    info.lines()
        .find_map(|line| line.strip_prefix("Pid:"))?
        .trim()
        .parse()
        .ok()
}

// ----- action adapter ---------------------------------------------------------

/// The real pidfd adapter, journaled.
pub struct ActionPortSpy {
    adapter: PidfdActionAdapter,
    journal: Journal,
}

impl ActionPortSpy {
    pub fn new(adapter: PidfdActionAdapter, journal: Journal) -> Self {
        Self { adapter, journal }
    }
}

impl ProcessActionPort for ActionPortSpy {
    fn prepare(
        &mut self,
        key: &ProcessKey,
        signal: Signal,
        context: &CapabilityContext,
    ) -> Result<PreparedProcess, ActionError> {
        let prepared = self.adapter.prepare(key, signal, context);
        self.journal.record(Step::PrepareFinished {
            pid: key.pid.get(),
            error: prepared.as_ref().err().map(ActionError::code),
        });
        prepared
    }

    fn send(
        &mut self,
        handle: ActionHandle,
        signal: Signal,
        receipt: &DurableReceipt,
    ) -> SignalOutcome {
        let outcome = self.adapter.send(handle, signal, receipt);
        self.journal.record(Step::SendFinished {
            outcome: outcome.clone(),
        });
        outcome
    }

    fn observe_exit(&mut self, handle: ActionHandle) -> ExitObservation {
        self.adapter.observe_exit(handle)
    }

    fn release(&mut self, handle: ActionHandle) {
        self.adapter.release(handle);
    }
}

// ----- audit store --------------------------------------------------------------

/// Fault switches of the audit worker.
#[derive(Debug, Default)]
pub struct AuditFaults {
    crash_after_next_intent: AtomicBool,
    crashed: AtomicBool,
}

impl AuditFaults {
    /// The process "dies" right after the next intent's COMMIT: instead of
    /// the receipt the Manager gets a commit-uncertain failure, and no later
    /// audit write reaches the database.
    pub fn crash_after_next_intent_commit(&self) {
        self.crash_after_next_intent.store(true, Ordering::SeqCst);
    }

    fn crash_now_if_requested(&self) -> bool {
        let requested = self.crash_after_next_intent.swap(false, Ordering::SeqCst);
        if requested {
            self.crashed.store(true, Ordering::SeqCst);
        }
        requested
    }

    fn has_crashed(&self) -> bool {
        self.crashed.load(Ordering::SeqCst)
    }
}

fn dead_process(commit_uncertain: bool) -> AuditFailure {
    AuditFailure {
        code: ErrorCode::PersistFailed,
        message: "simulated crash after the intent COMMIT".to_owned(),
        commit_uncertain,
    }
}

/// The real audit repository (SQLite, or the fail-closed stand-in), journaled.
pub struct AuditSpy {
    repository: Box<dyn AuditRepository>,
    journal: Journal,
    faults: Arc<AuditFaults>,
}

impl AuditSpy {
    pub fn new(
        repository: Box<dyn AuditRepository>,
        journal: Journal,
        faults: Arc<AuditFaults>,
    ) -> Self {
        Self {
            repository,
            journal,
            faults,
        }
    }

    fn record_intent_result(
        &self,
        intent: &ActionIntent,
        result: &Result<DurableReceipt, AuditFailure>,
    ) {
        let step = match result {
            Ok(_) => Step::IntentCommitted {
                request_id: intent.request_id,
                intent_id: intent.intent_id,
            },
            Err(failure) => Step::IntentRefused {
                request_id: intent.request_id,
                code: failure.code,
            },
        };
        self.journal.record(step);
    }
}

impl AuditRepository for AuditSpy {
    fn append_intent(&mut self, intent: &ActionIntent) -> Result<DurableReceipt, AuditFailure> {
        if self.faults.has_crashed() {
            return Err(dead_process(false));
        }
        self.journal.record(Step::IntentRequested {
            request_id: intent.request_id,
            intent_id: intent.intent_id,
        });
        let result = self.repository.append_intent(intent);
        self.record_intent_result(intent, &result);
        // Durable on disk, but the receipt never reaches the Manager.
        if result.is_ok() && self.faults.crash_now_if_requested() {
            return Err(dead_process(true));
        }
        result
    }

    fn append_outcome(&mut self, outcome: &ActionOutcomeRecord) -> Result<(), AuditFailure> {
        if self.faults.has_crashed() {
            return Err(dead_process(false));
        }
        let written = self.repository.append_outcome(outcome);
        if written.is_ok() {
            self.journal.record(Step::OutcomeRecorded {
                request_id: outcome.request_id,
                state: outcome.state,
            });
        }
        written
    }

    fn append_attempt(&mut self, attempt: &ActionAttempt) -> Result<(), AuditFailure> {
        if self.faults.has_crashed() {
            return Err(dead_process(false));
        }
        self.repository.append_attempt(attempt)
    }

    fn recover_unknown(
        &mut self,
        now_boot_ns: u64,
        now_utc: &str,
    ) -> Result<Vec<RecoveredIntent>, AuditFailure> {
        if self.faults.has_crashed() {
            return Err(dead_process(false));
        }
        self.repository.recover_unknown(now_boot_ns, now_utc)
    }

    fn expire(&mut self, before_utc: &str) -> Result<AuditExpiryReport, AuditFailure> {
        if self.faults.has_crashed() {
            return Err(dead_process(false));
        }
        self.repository.expire(before_utc)
    }

    fn status(&mut self) -> PersistenceStatus {
        if self.faults.has_crashed() {
            return PersistenceStatus {
                state: PersistenceState::Failed,
                detail: Some("simulated crash".to_owned()),
                ..PersistenceStatus::healthy(PersistenceKind::Audit)
            };
        }
        self.repository.status()
    }
}

/// Real `statvfs` free space that a test can flip to "disk full", which makes
/// the real audit repository refuse new records (STORAGE.md §6 low-water mark).
#[derive(Debug, Default)]
pub struct DiskSpace {
    full: AtomicBool,
}

impl DiskSpace {
    pub fn fill_up(&self) {
        self.full.store(true, Ordering::SeqCst);
    }
}

impl FreeSpaceProbe for DiskSpace {
    fn available_bytes(&self, path: &Path) -> Result<u64, InfraError> {
        if self.full.load(Ordering::SeqCst) {
            return Ok(0);
        }
        StatvfsFreeSpace.available_bytes(path)
    }
}

// ----- control thread -> worker hand-over ------------------------------------------

/// A Manager `Dispatch` port into a real worker queue, journaled on the
/// control thread at hand-over time (so a send that was never even requested
/// is provable) and counted for [`WorkCounter`] quiescence.
pub struct RecordingDispatch<J> {
    queue: PoolHandle<J>,
    journal: Journal,
    work: Arc<WorkCounter>,
}

impl<J> RecordingDispatch<J> {
    pub fn new(queue: PoolHandle<J>, journal: Journal, work: Arc<WorkCounter>) -> Self {
        Self {
            queue,
            journal,
            work,
        }
    }
}

impl<J: Send + DescribeJob> Dispatch<J> for RecordingDispatch<J> {
    fn try_dispatch(&self, job: J) -> Result<(), DispatchError> {
        // Journaled before the hand-over so the worker's own steps follow it.
        if let Some(step) = job.describe() {
            self.journal.record(step);
        }
        self.work.handed_over();
        let handed = self.queue.try_dispatch(job);
        if handed.is_err() {
            self.work.withdrawn();
            self.journal.record(Step::DispatchRefused);
        }
        handed
    }
}

/// The journal step of a job, if it is one the suite tracks.
pub trait DescribeJob {
    fn describe(&self) -> Option<Step>;
}

impl DescribeJob for ActionOp {
    fn describe(&self) -> Option<Step> {
        match self {
            Self::Prepare { request_id, .. } => Some(Step::PrepareDispatched {
                request_id: *request_id,
            }),
            Self::Send {
                request_id,
                intent_id,
                ..
            } => Some(Step::SendDispatched {
                request_id: *request_id,
                intent_id: *intent_id,
            }),
            Self::ObserveExit { .. } | Self::Release { .. } => None,
        }
    }
}

impl DescribeJob for dgxtop_core::action::AuditOp {
    fn describe(&self) -> Option<Step> {
        None
    }
}
