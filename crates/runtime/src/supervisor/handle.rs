//! Cloneable, non-blocking front of the supervisor.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use dgxtop_core::budget::QueueGate;
use dgxtop_core::clock::{TimeProvider, ms_to_ns};
use dgxtop_core::identity::AdapterId;
use dgxtop_core::ports::{
    AdapterFailureKind, AdapterJob, Dispatch, DispatchError, IngestionCompletion,
};
use dgxtop_core::view::{HostState, HostStatus};

use crate::broker::{Admission, BrokerKey};
use crate::ipc::IpcLimits;

use super::credit::{discover_credit, read_credit};
use super::report::{HostShutdown, ShutdownReport, StopOutcome};
use super::requests::{self, failed_completion, failure};
use super::shared::{Execution, Shared, SupervisorCommand};

/// Poll step while joining the supervisor thread after its report.
const JOIN_POLL: Duration = Duration::from_millis(1);

pub(crate) struct HandleParts {
    pub shared: Arc<Shared>,
    pub commands: Sender<SupervisorCommand>,
    pub ingress: QueueGate,
    pub limits: IpcLimits,
    pub clock: Arc<dyn TimeProvider>,
    pub finished: Receiver<ShutdownReport>,
    pub thread: JoinHandle<()>,
    pub default_shutdown: Duration,
}

struct HandleInner {
    shared: Arc<Shared>,
    commands: Sender<SupervisorCommand>,
    ingress: QueueGate,
    limits: IpcLimits,
    clock: Arc<dyn TimeProvider>,
    finished: Receiver<ShutdownReport>,
    thread: Mutex<Option<JoinHandle<()>>>,
    report: Mutex<Option<ShutdownReport>>,
    default_shutdown: Duration,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        // The last handle going away stops the hosts (bounded) instead of
        // leaving unsupervised children behind.
        if !self.shared.shutdown_requested() {
            let deadline = self
                .clock
                .now_boot_ns()
                .saturating_add(duration_ns(self.default_shutdown));
            self.shared.request_shutdown(deadline);
        }
    }
}

/// Submission and control handle (`Dispatch<AdapterJob>`, `Clone`, `Send`).
///
/// * `try_dispatch` never blocks: `Busy` when the host has a request in
///   flight or is not ready, `Capacity` when the read's worst-case ingress
///   credit cannot be reserved, `Closed` after shutdown or for an unknown
///   adapter. Invalid jobs (bad plan, expired deadline, stale generation)
///   are accepted and answered with a failure completion, so the Manager
///   always hears back through the same channel.
/// * An identical read (same broker key) already in flight is **joined**:
///   `try_dispatch` returns `Ok(())`, no second host request is sent, and
///   the joiner later receives its own `IngestionCompletion::Batch` carrying
///   *its* `request_id` and the *same* `Arc<IngressBatch>` (the batch's
///   inner `request_id` stays the dispatched one). A read identical to one
///   that completed before its deadline is answered from the broker's
///   recycle area the same way. Failures are shared identically.
#[derive(Clone)]
pub struct SupervisorHandle {
    inner: Arc<HandleInner>,
}

impl fmt::Debug for SupervisorHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SupervisorHandle")
            .field("statuses", &self.statuses())
            .finish()
    }
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

impl SupervisorHandle {
    pub(crate) fn new(parts: HandleParts) -> Self {
        Self {
            inner: Arc::new(HandleInner {
                shared: parts.shared,
                commands: parts.commands,
                ingress: parts.ingress,
                limits: parts.limits,
                clock: parts.clock,
                finished: parts.finished,
                thread: Mutex::new(Some(parts.thread)),
                report: Mutex::new(None),
                default_shutdown: parts.default_shutdown,
            }),
        }
    }

    /// Latest status of every host (includes the last success time).
    pub fn statuses(&self) -> Vec<HostStatus> {
        self.inner.shared.lock().statuses.clone()
    }

    /// PIDs of live and quarantined children: protected from process actions
    /// and counted in process-tree statistics.
    pub fn managed_pids(&self) -> Vec<i32> {
        self.inner
            .shared
            .lock()
            .pids
            .iter()
            .flatten()
            .copied()
            .collect()
    }

    /// Adapters supervised by this handle, in spec order.
    pub fn adapters(&self) -> Vec<AdapterId> {
        self.inner
            .shared
            .lock()
            .gates
            .iter()
            .map(|gate| gate.adapter_id.clone())
            .collect()
    }

    /// Stops every host (protocol shutdown → SIGTERM → SIGKILL → reap) and
    /// returns within `deadline`. Hosts that could not be reaped in time are
    /// reported as unreaped; this never waits forever. Idempotent.
    pub fn shutdown(&self, deadline: Duration) -> ShutdownReport {
        let mut cached = self
            .inner
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(report) = cached.as_ref() {
            return report.clone();
        }
        let started = Instant::now();
        let until = started + deadline;
        let deadline_ns = self
            .inner
            .clock
            .now_boot_ns()
            .saturating_add(duration_ns(deadline));
        self.inner.shared.request_shutdown(deadline_ns);
        let report = match self.inner.finished.recv_timeout(deadline) {
            Ok(mut report) => {
                report.elapsed = started.elapsed();
                self.join_thread(until);
                report
            }
            Err(_) => self.partial_report(started.elapsed()),
        };
        *cached = Some(report.clone());
        report
    }

    fn join_thread(&self, until: Instant) {
        let mut slot = self
            .inner
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(thread) = slot.take() else {
            return;
        };
        while !thread.is_finished() && Instant::now() < until {
            std::thread::sleep(JOIN_POLL);
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            *slot = Some(thread);
        }
    }

    /// Report assembled from the shared view when the thread missed the
    /// deadline: nothing is claimed as reaped.
    fn partial_report(&self, elapsed: Duration) -> ShutdownReport {
        let state = self.inner.shared.lock();
        let hosts = state
            .gates
            .iter()
            .zip(state.statuses.iter().zip(state.pids.iter()))
            .map(|(gate, (status, pid))| HostShutdown {
                adapter_id: gate.adapter_id.clone(),
                generation: status.generation,
                outcome: if pid.is_some() || status.state == HostState::Quarantined {
                    StopOutcome::Unreaped { pid: *pid }
                } else {
                    StopOutcome::NotRunning
                },
            })
            .collect();
        ShutdownReport {
            elapsed,
            completed_within_deadline: false,
            hosts,
        }
    }

    fn deliver(&self, completion: IngestionCompletion) -> Result<(), DispatchError> {
        match self
            .inner
            .commands
            .try_send(SupervisorCommand::Deliver(Box::new(completion)))
        {
            Ok(()) => {
                self.inner.shared.wake();
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(DispatchError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(DispatchError::Closed),
        }
    }

    fn reject(
        &self,
        job: &AdapterJob,
        kind: AdapterFailureKind,
        message: &str,
    ) -> Result<(), DispatchError> {
        let request_id = requests::request_id_of(job);
        self.deliver(failed_completion(job, request_id, failure(kind, message)))
    }

    fn credit_for(&self, job: &AdapterJob) -> u64 {
        match job {
            AdapterJob::Read(plan) => read_credit(plan, &self.inner.limits),
            AdapterJob::Discover(_) => discover_credit(&self.inner.limits),
        }
    }
}

impl Dispatch<AdapterJob> for SupervisorHandle {
    fn try_dispatch(&self, job: AdapterJob) -> Result<(), DispatchError> {
        let inner = &self.inner;
        if inner.shared.shutdown_requested() {
            return Err(DispatchError::Closed);
        }
        let request = match requests::request_for(&job) {
            Ok(request) => request,
            Err(reason) => {
                let message = format!("invalid job: {reason}");
                return self.reject(&job, AdapterFailureKind::Protocol, &message);
            }
        };
        let now = inner.clock.now_boot_ns();
        if request.deadline_boot_ns <= now {
            return self.reject(
                &job,
                AdapterFailureKind::TimedOut,
                "deadline passed before dispatch",
            );
        }
        let broker_key = match &job {
            AdapterJob::Read(plan) => Some(BrokerKey::from_plan(plan)),
            AdapterJob::Discover(_) => None,
        };
        let mut state = inner.shared.lock();
        let index = state
            .index_of(&request.adapter_id)
            .ok_or(DispatchError::Closed)?;
        let current = state.gates[index].generation;
        if request.host_generation != current {
            drop(state);
            let message = format!(
                "plan names host generation {}, current is {current}",
                request.host_generation
            );
            return self.reject(&job, AdapterFailureKind::HostUnavailable, &message);
        }
        if let Some(key) = &broker_key {
            match state
                .broker
                .admit(key, request.request_id, request.deadline_boot_ns, now)
            {
                Admission::Dispatch => {}
                Admission::Joined => return Ok(()),
                Admission::Full => return Err(DispatchError::Busy),
                Admission::Cached(result) => {
                    drop(state);
                    return self.deliver(IngestionCompletion::Batch {
                        adapter_id: request.adapter_id.clone(),
                        request_id: request.request_id,
                        host_generation: request.host_generation,
                        result,
                    });
                }
            }
        }
        let gate = &state.gates[index];
        if !gate.ready || gate.claimed {
            if let Some(key) = &broker_key {
                state.broker.abandon(key, request.request_id);
            }
            return Err(DispatchError::Busy);
        }
        let ticket = match inner.ingress.try_admit(self.credit_for(&job)) {
            Ok(ticket) => ticket,
            Err(error) => {
                if let Some(key) = &broker_key {
                    state.broker.abandon(key, request.request_id);
                }
                return Err(DispatchError::Capacity(error));
            }
        };
        state.gates[index].claimed = true;
        let request_id = request.request_id;
        let execution = Execution {
            index,
            job,
            request,
            ticket,
            broker_key: broker_key.clone(),
        };
        if let Err(error) = inner
            .commands
            .try_send(SupervisorCommand::Execute(Box::new(execution)))
        {
            state.gates[index].claimed = false;
            if let Some(key) = &broker_key {
                state.broker.abandon(key, request_id);
            }
            return Err(match error {
                TrySendError::Full(_) => DispatchError::Busy,
                TrySendError::Disconnected(_) => DispatchError::Closed,
            });
        }
        drop(state);
        inner.shared.wake();
        Ok(())
    }
}

/// Default bounded shutdown used when the last handle is dropped.
pub(crate) fn default_shutdown(config: &super::SupervisorConfig) -> Duration {
    Duration::from_nanos(ms_to_ns(
        config.child_grace_ms + config.child_term_ms + config.child_reap_ms + 500,
    ))
}
