//! Causally ordered record of what the stack did: control-thread hand-overs
//! to the workers, audit commits, adapter calls and every pidfd system call.
//! The spies append; tests count and order the steps.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use dgxtop_core::action::{ActionState, PersistenceState, RecoveredIntent, SignalOutcome};
use dgxtop_core::errors::ErrorCode;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The control thread handed `ActionOp::Prepare` to the action worker.
    PrepareDispatched { request_id: Uuid },
    /// The control thread handed `ActionOp::Send` to the action worker.
    SendDispatched { request_id: Uuid, intent_id: Uuid },
    /// A dispatch to a worker was refused (queue full or stopped).
    DispatchRefused,
    /// The audit worker started an intent transaction.
    IntentRequested { request_id: Uuid, intent_id: Uuid },
    /// The intent's COMMIT (`synchronous=FULL`) returned: it is durable.
    IntentCommitted { request_id: Uuid, intent_id: Uuid },
    /// The audit store refused or failed the intent.
    IntentRefused { request_id: Uuid, code: ErrorCode },
    /// An outcome row of a committed intent was written.
    OutcomeRecorded {
        request_id: Uuid,
        state: ActionState,
        detail: String,
    },
    /// The audit store answered the startup recovery (`None`: it failed).
    RecoveryAnswered {
        recovered: Option<Vec<RecoveredIntent>>,
    },
    /// The audit store reported its status.
    StatusReported { state: PersistenceState },
    /// `pidfd_open(pid, 0)` returned (`fd` is `None` on failure).
    PidfdOpened { pid: i32, fd: Option<RawFd> },
    /// The adapter finished a prepare (`error` is `None` on success).
    PrepareFinished { pid: i32, error: Option<ErrorCode> },
    /// The adapter finished a send.
    SendFinished { outcome: SignalOutcome },
    /// `pidfd_send_signal(fd, signal, NULL, 0)` was issued to the kernel.
    SignalSyscall {
        fd: RawFd,
        target_pid: Option<i64>,
        signal: i32,
    },
}

/// Shared, append-only step log.
#[derive(Debug, Clone, Default)]
pub struct Journal {
    steps: Arc<Mutex<Vec<Step>>>,
}

impl Journal {
    pub fn record(&self, step: Step) {
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(step);
    }

    pub fn steps(&self) -> Steps {
        Steps(
            self.steps
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
    }
}

/// One `pidfd_send_signal` call as the kernel saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalCall {
    pub fd: RawFd,
    pub target_pid: Option<i64>,
    pub signal: i32,
}

/// A snapshot of the journal with intent-revealing queries.
#[derive(Debug, Clone)]
pub struct Steps(pub Vec<Step>);

impl Steps {
    pub fn prepares_dispatched(&self) -> usize {
        self.count(|step| matches!(step, Step::PrepareDispatched { .. }))
    }

    pub fn sends_dispatched(&self) -> usize {
        self.count(|step| matches!(step, Step::SendDispatched { .. }))
    }

    pub fn intents_requested(&self) -> Vec<Uuid> {
        self.0
            .iter()
            .filter_map(|step| match step {
                Step::IntentRequested { request_id, .. } => Some(*request_id),
                _ => None,
            })
            .collect()
    }

    /// `(request_id, intent_id)` of every durable intent.
    pub fn intents_committed(&self) -> Vec<(Uuid, Uuid)> {
        self.0
            .iter()
            .filter_map(|step| match step {
                Step::IntentCommitted {
                    request_id,
                    intent_id,
                } => Some((*request_id, *intent_id)),
                _ => None,
            })
            .collect()
    }

    /// States of the outcome rows written for one intent, in write order.
    pub fn outcomes_for(&self, request_id: Uuid) -> Vec<ActionState> {
        self.0
            .iter()
            .filter_map(|step| match step {
                Step::OutcomeRecorded {
                    request_id: recorded,
                    state,
                    ..
                } if *recorded == request_id => Some(*state),
                _ => None,
            })
            .collect()
    }

    /// Detail of the outcome row written for one intent in `state`.
    pub fn outcome_detail(&self, request_id: Uuid, state: ActionState) -> Option<String> {
        self.0.iter().find_map(|step| match step {
            Step::OutcomeRecorded {
                request_id: recorded,
                state: written,
                detail,
            } if *recorded == request_id && *written == state => Some(detail.clone()),
            _ => None,
        })
    }

    /// Intents the audit store's successful recovery marked unknown
    /// (`None` while no recovery succeeded).
    pub fn recovered_intents(&self) -> Option<Vec<RecoveredIntent>> {
        self.0.iter().find_map(|step| match step {
            Step::RecoveryAnswered { recovered } => recovered.clone(),
            _ => None,
        })
    }

    /// States the audit store reported, oldest first.
    pub fn statuses_reported(&self) -> Vec<PersistenceState> {
        self.0
            .iter()
            .filter_map(|step| match step {
                Step::StatusReported { state } => Some(*state),
                _ => None,
            })
            .collect()
    }

    pub fn signal_calls(&self) -> Vec<SignalCall> {
        self.0
            .iter()
            .filter_map(|step| match step {
                Step::SignalSyscall {
                    fd,
                    target_pid,
                    signal,
                } => Some(SignalCall {
                    fd: *fd,
                    target_pid: *target_pid,
                    signal: *signal,
                }),
                _ => None,
            })
            .collect()
    }

    /// Descriptors returned by `pidfd_open` for `pid` (failed opens excluded).
    pub fn pidfds_opened_for(&self, pid: i32) -> Vec<RawFd> {
        self.0
            .iter()
            .filter_map(|step| match step {
                Step::PidfdOpened {
                    pid: opened,
                    fd: Some(fd),
                } if *opened == pid => Some(*fd),
                _ => None,
            })
            .collect()
    }

    pub fn prepare_errors_for(&self, pid: i32) -> Vec<Option<ErrorCode>> {
        self.0
            .iter()
            .filter_map(|step| match step {
                Step::PrepareFinished {
                    pid: prepared,
                    error,
                } if *prepared == pid => Some(*error),
                _ => None,
            })
            .collect()
    }

    pub fn send_outcomes(&self) -> Vec<SignalOutcome> {
        self.0
            .iter()
            .filter_map(|step| match step {
                Step::SendFinished { outcome } => Some(outcome.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn contains(&self, step: &Step) -> bool {
        self.0.contains(step)
    }

    pub fn index_of(&self, predicate: impl Fn(&Step) -> bool) -> Option<usize> {
        self.0.iter().position(predicate)
    }

    fn count(&self, predicate: impl Fn(&Step) -> bool) -> usize {
        self.0.iter().filter(|step| predicate(step)).count()
    }
}

/// Jobs handed to one worker versus jobs it finished (completion delivered),
/// so the suite can tell when a worker has nothing left in flight.
#[derive(Debug, Default)]
pub struct WorkCounter {
    handed_over: AtomicU64,
    finished: AtomicU64,
}

impl WorkCounter {
    pub fn handed_over(&self) {
        self.handed_over.fetch_add(1, Ordering::SeqCst);
    }

    /// A hand-over the worker queue refused.
    pub fn withdrawn(&self) {
        self.handed_over.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn finished(&self) {
        self.finished.fetch_add(1, Ordering::SeqCst);
    }

    pub fn is_idle(&self) -> bool {
        self.finished.load(Ordering::SeqCst) == self.handed_over.load(Ordering::SeqCst)
    }
}
