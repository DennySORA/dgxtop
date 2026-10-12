//! Adapter-host supervision (`RUNTIME.md` §1, §8, §9; `INTERFACES.md` §7).
//!
//! One thread multiplexes every host's pipes with `poll` (plus a self-pipe
//! woken by [`SupervisorHandle`]); it never runs adapter code and never
//! blocks. Each backend has exactly one incarnation at a time and at most
//! one request in flight. Deadlines come from the request
//! (`ReadPlan::deadline_boot_ns`); a timeout emits
//! `AdapterFailureKind::TimedOut` but is *not* cancellation — the late reply
//! is awaited and discarded, never forwarded. Repeated failures restart the
//! host through protocol shutdown → SIGTERM → SIGKILL (only ever to the
//! managed, unreaped child) → reap; an unreapable child is quarantined and
//! never respawned, and still counts toward the host limit. After the
//! threshold the circuit opens with exponential, deterministic backoff and a
//! half-open discover probe of the next generation.

mod circuit;
mod config;
mod connection;
mod context;
mod credit;
mod event_loop;
mod handle;
mod launch;
mod outbox;
mod report;
mod requests;
mod shared;
mod slot;
mod status_emission;
mod stderr_drain;

use std::collections::HashSet;
use std::sync::Arc;

use crossbeam_channel::{Sender, bounded};
use dgxtop_core::budget::{ByteLedger, Partition, QueueGate};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::InfraError;
use dgxtop_core::ports::{IngestionCompletion, Logger};
use dgxtop_core::view::{HostState, HostStatus};
use nix::fcntl::OFlag;

pub use circuit::{CircuitBreaker, backoff_seed};
pub use config::{MAX_HOSTS, SupervisorConfig};
pub use credit::{decoded_credit, discover_credit, frame_credit, read_credit};
pub use handle::SupervisorHandle;
pub use launch::{
    AdapterFactory, ChildControl, ExitSummary, HostLaunch, HostLauncher, HostSpec, LaunchRequest,
    LaunchedHost, SignalDelivery, StandardLauncher,
};
pub use report::{HostShutdown, ShutdownReport, StopOutcome, StopStage};

use crate::broker::ReadBroker;
use crate::ipc::{HELLO_FRAME_BYTES, REQUEST_FRAME_BYTES};
use context::LoopContext;
use event_loop::EventLoop;
use handle::HandleParts;
use outbox::Outbox;
use shared::{HostGate, Shared, SharedState};
use slot::HostSlot;
use stderr_drain::MAX_STDERR_LINE_BYTES;

/// Read scratch shared by all pipes of the supervisor thread.
const SCRATCH_BYTES: usize = 64 * 1024;

/// Entry point; see the module documentation.
#[derive(Debug, Clone, Copy, Default)]
pub struct Supervisor;

impl Supervisor {
    /// Starts the supervisor thread and launches every host.
    #[allow(clippy::too_many_arguments)] // signature fixed by the composition root
    pub fn start(
        config: SupervisorConfig,
        specs: Vec<HostSpec>,
        ledger: ByteLedger,
        ingress: QueueGate,
        completions: Sender<IngestionCompletion>,
        clock: Arc<dyn TimeProvider>,
        logger: Arc<dyn Logger>,
    ) -> Result<SupervisorHandle, InfraError> {
        Self::start_with_launcher(
            config,
            specs,
            ledger,
            ingress,
            completions,
            clock,
            logger,
            Arc::new(StandardLauncher),
        )
    }

    /// [`Supervisor::start`] with an injected launcher (fault injection).
    #[allow(clippy::too_many_arguments)] // `start` plus the injected launcher
    pub fn start_with_launcher(
        config: SupervisorConfig,
        specs: Vec<HostSpec>,
        ledger: ByteLedger,
        ingress: QueueGate,
        completions: Sender<IngestionCompletion>,
        clock: Arc<dyn TimeProvider>,
        logger: Arc<dyn Logger>,
        launcher: Arc<dyn HostLauncher>,
    ) -> Result<SupervisorHandle, InfraError> {
        config.validate()?;
        validate_specs(&specs, &config)?;
        let limits = config.ipc_limits()?;
        let working_set = ledger
            .try_reserve(Partition::Reserve, working_set_bytes(specs.len()))
            .map_err(|error| InfraError::Unavailable(format!("supervisor working set: {error}")))?;
        let (wake_reader, wake_writer) =
            nix::unistd::pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)
                .map_err(|errno| InfraError::Unavailable(format!("wake pipe: {errno}")))?;
        let shared = Arc::new(Shared::new(initial_state(&specs, &config), wake_writer));
        let (commands, command_receiver) = bounded(config.command_capacity + specs.len());
        let (finished_sender, finished) = bounded(1);
        let slots = specs
            .into_iter()
            .enumerate()
            .map(|(index, spec)| HostSlot::new(index, spec, &config))
            .collect();
        let default_shutdown = handle::default_shutdown(&config);
        let event_loop = EventLoop {
            slots,
            ctx: LoopContext {
                outbox: Outbox::new(completions, config.outbox_capacity),
                config,
                limits,
                shared: Arc::clone(&shared),
                clock: Arc::clone(&clock),
                logger,
                launcher,
                ingress: ingress.clone(),
                scratch: vec![0; SCRATCH_BYTES],
                shutdown_deadline: None,
            },
            commands: command_receiver,
            wake_reader,
            finished: finished_sender,
            working_set,
            shutdown_started: None,
        };
        let thread = std::thread::Builder::new()
            .name("dgxtop-supervisor".into())
            .spawn(move || event_loop.run())
            .map_err(|error| InfraError::from_io("supervisor.spawn_thread", &error))?;
        Ok(SupervisorHandle::new(HandleParts {
            shared,
            commands,
            ingress,
            limits,
            clock,
            finished,
            thread,
            default_shutdown,
        }))
    }
}

fn validate_specs(specs: &[HostSpec], config: &SupervisorConfig) -> Result<(), InfraError> {
    if specs.len() > config.max_hosts {
        return Err(InfraError::Unavailable(format!(
            "{} adapter hosts exceed max_hosts {}",
            specs.len(),
            config.max_hosts
        )));
    }
    let mut seen = HashSet::new();
    for spec in specs {
        if !seen.insert(spec.adapter_id.clone()) {
            return Err(InfraError::Unavailable(format!(
                "adapter {} is listed twice",
                spec.adapter_id
            )));
        }
    }
    Ok(())
}

fn working_set_bytes(hosts: usize) -> u64 {
    let per_host = HELLO_FRAME_BYTES + REQUEST_FRAME_BYTES + MAX_STDERR_LINE_BYTES;
    let total = SCRATCH_BYTES.saturating_add(per_host.saturating_mul(hosts));
    u64::try_from(total).unwrap_or(u64::MAX)
}

fn initial_state(specs: &[HostSpec], config: &SupervisorConfig) -> SharedState {
    SharedState {
        gates: specs
            .iter()
            .map(|spec| HostGate {
                adapter_id: spec.adapter_id.clone(),
                ready: false,
                claimed: false,
                generation: 1,
            })
            .collect(),
        broker: ReadBroker::new(config.broker_capacity, config.broker_max_requesters),
        statuses: specs
            .iter()
            .map(|spec| HostStatus {
                adapter_id: spec.adapter_id.to_string(),
                state: HostState::Starting,
                generation: 1,
                pid: None,
                last_success_boot_ns: None,
                timeouts_total: 0,
                consecutive_failures: 0,
                restarts_total: 0,
                backoff_until_boot_ns: None,
                detail: None,
            })
            .collect(),
        pids: vec![None; specs.len()],
    }
}

#[cfg(test)]
mod tests;
