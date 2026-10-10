//! State shared between handles (any thread) and the supervisor thread.
//!
//! Critical sections are tiny and never include I/O: handles check/claim a
//! host gate and the broker table; the supervisor thread publishes host
//! readiness, statuses and managed pids.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use dgxtop_core::identity::AdapterId;
use dgxtop_core::ports::IngestionCompletion;
use dgxtop_core::view::HostStatus;
use uuid::Uuid;

use crate::broker::{BrokerKey, ReadBroker};
use dgxtop_core::budget::QueueTicket;
use dgxtop_core::observation::IpcRequest;
use dgxtop_core::ports::AdapterJob;

/// Dispatch gate of one host.
#[derive(Debug, Clone)]
pub(crate) struct HostGate {
    pub adapter_id: AdapterId,
    /// Idle and verified: a job may be dispatched.
    pub ready: bool,
    /// A handle claimed the host; cleared when the request finishes.
    pub claimed: bool,
    /// Generation that plans must name.
    pub generation: u32,
}

#[derive(Debug)]
pub(crate) struct SharedState {
    pub gates: Vec<HostGate>,
    pub broker: ReadBroker,
    pub statuses: Vec<HostStatus>,
    pub pids: Vec<Option<i32>>,
}

impl SharedState {
    pub(crate) fn index_of(&self, adapter_id: &AdapterId) -> Option<usize> {
        self.gates
            .iter()
            .position(|gate| &gate.adapter_id == adapter_id)
    }
}

/// A job accepted by a handle, travelling to the supervisor thread.
#[derive(Debug)]
pub(crate) struct Execution {
    pub index: usize,
    pub job: AdapterJob,
    pub request: IpcRequest,
    pub ticket: QueueTicket,
    pub broker_key: Option<BrokerKey>,
}

impl Execution {
    pub(crate) fn request_id(&self) -> Uuid {
        self.request.request_id
    }
}

/// Messages from handles to the supervisor thread.
#[derive(Debug)]
pub(crate) enum SupervisorCommand {
    Execute(Box<Execution>),
    /// A completion produced on the handle side (cached broker result or an
    /// invalid job); routed through the thread so it is the only producer.
    Deliver(Box<IngestionCompletion>),
}

#[derive(Debug)]
pub(crate) struct Shared {
    state: Mutex<SharedState>,
    wake: OwnedFd,
    shutdown_requested: AtomicBool,
    shutdown_deadline_boot_ns: AtomicU64,
}

impl Shared {
    pub(crate) fn new(state: SharedState, wake: OwnedFd) -> Self {
        Self {
            state: Mutex::new(state),
            wake,
            shutdown_requested: AtomicBool::new(false),
            shutdown_deadline_boot_ns: AtomicU64::new(0),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, SharedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wakes the supervisor's poll. A full wake pipe already guarantees a
    /// pending wake-up, so `EAGAIN` is ignored.
    pub(crate) fn wake(&self) {
        let _ = nix::unistd::write(&self.wake, &[1]);
    }

    pub(crate) fn request_shutdown(&self, deadline_boot_ns: u64) {
        // The earliest requested deadline wins.
        let _ = self.shutdown_deadline_boot_ns.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| {
                Some(if current == 0 {
                    deadline_boot_ns
                } else {
                    current.min(deadline_boot_ns)
                })
            },
        );
        self.shutdown_requested.store(true, Ordering::Release);
        self.wake();
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    /// Requested deadline, if one was given.
    pub(crate) fn shutdown_deadline(&self) -> Option<u64> {
        match self.shutdown_deadline_boot_ns.load(Ordering::Acquire) {
            0 => None,
            deadline => Some(deadline),
        }
    }
}
