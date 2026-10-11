//! The public Manager: construction, sessions, shutdown trigger and the
//! single control thread (RUNTIME.md §4).
//!
//! One iteration, within `runtime.control_slice_ms`:
//! 1. out-of-band shutdown flag;
//! 2. ≤ 8 control completions (config, audit, actions, history);
//! 3. ≤ 8 commands;
//! 4. ≤ 4 ingestion completions;
//! 5. due timers.
//!
//! Each category processes at least one ready item even when the slice is
//! spent, so none can starve another, and none is drained unboundedly. When
//! nothing is ready the thread sleeps on the channels (completions, commands
//! and the wake token of shutdowns and viewport changes) until the next
//! deadline, at most [`IDLE_TICK`] (no busy spin, no idle polling).

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Select, Sender, bounded};
use dgxtop_core::clock::until;
use dgxtop_core::ports::{ControlCompletion, IngestionCompletion};

use crate::control::{ControlCore, QueueLevels};
use crate::error::ManagerError;
use crate::session::{CommandInbox, InboxParts, Outstanding, QueuedCommand, SessionHandle};
use crate::setup::{
    CONTROL_INBOX_CAPACITY, INGESTION_INBOX_CAPACITY, ManagerInboxes, ManagerPorts, ManagerSetup,
    validate,
};
use crate::shutdown::{ShutdownSignal, ShutdownSummary, ShutdownTrigger};

/// Control completions per iteration.
const CONTROL_BATCH: usize = 8;
/// Commands per iteration.
const COMMAND_BATCH: usize = 8;
/// Ingestion completions per iteration.
const INGESTION_BATCH: usize = 4;
/// Longest idle sleep. Everything that needs the loop either arrives on a
/// channel or has a deadline; this only bounds what neither covers, such
/// as reaping a session whose handles were dropped.
const IDLE_TICK: Duration = Duration::from_millis(500);
/// Shortest sleep when nothing is ready, so an overdue deadline can never
/// turn into a busy spin.
const MIN_WAIT: Duration = Duration::from_millis(1);

struct Receivers {
    ingestion: Receiver<IngestionCompletion>,
    control: Receiver<ControlCompletion>,
    commands: Receiver<QueuedCommand>,
    wake: Receiver<()>,
}

/// What one iteration left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Iteration {
    pub(crate) more_ready: bool,
    pub(crate) next_deadline_ns: Option<u64>,
}

/// Bounded inbox channels of one Manager, created before the Manager so the
/// senders can be handed to runtime workers first.
#[derive(Debug)]
pub struct ManagerChannels {
    ingestion_sender: Sender<IngestionCompletion>,
    ingestion: Receiver<IngestionCompletion>,
    control_sender: Sender<ControlCompletion>,
    control: Receiver<ControlCompletion>,
}

impl ManagerChannels {
    /// Creates both inboxes with their fixed capacities.
    pub fn create() -> Self {
        let (ingestion_sender, ingestion) = bounded(INGESTION_INBOX_CAPACITY);
        let (control_sender, control) = bounded(CONTROL_INBOX_CAPACITY);
        Self {
            ingestion_sender,
            ingestion,
            control_sender,
            control,
        }
    }

    /// Senders for the runtime workers.
    pub fn inboxes(&self) -> ManagerInboxes {
        ManagerInboxes {
            ingestion: self.ingestion_sender.clone(),
            control: self.control_sender.clone(),
        }
    }
}

/// The application layer. Construct with [`Manager::new`], open sessions,
/// then move it to its own thread and call [`Manager::run`].
pub struct Manager {
    core: ControlCore,
    receivers: Receivers,
    summary: Option<ShutdownSummary>,
    // Held so the inboxes never disconnect (a disconnected receiver would
    // always be "ready" and spin the select).
    _ingestion_sender: Sender<IngestionCompletion>,
    _control_sender: Sender<ControlCompletion>,
}

impl Manager {
    pub fn new(
        setup: ManagerSetup,
        ports: ManagerPorts,
    ) -> Result<(Manager, ManagerInboxes), ManagerError> {
        let channels = ManagerChannels::create();
        let inboxes = channels.inboxes();
        let manager = Self::with_channels(setup, ports, channels)?;
        Ok((manager, inboxes))
    }

    /// Builds the Manager on inbox channels created beforehand, so runtime
    /// workers (and the adapter supervisor, which reports host status as
    /// soon as it starts) can hold the senders before the Manager exists.
    pub fn with_channels(
        setup: ManagerSetup,
        ports: ManagerPorts,
        channels: ManagerChannels,
    ) -> Result<Manager, ManagerError> {
        validate(&setup, &ports)?;
        let effective = &setup.effective_config;
        let ManagerChannels {
            ingestion_sender,
            ingestion,
            control_sender,
            control,
        } = channels;
        let command_capacity = usize::try_from(effective.queues.commands)
            .unwrap_or(1)
            .max(1);
        let (command_sender, commands) = bounded(command_capacity);
        let (wake_sender, wake) = bounded(1);
        let shutdown = Arc::new(ShutdownSignal::new(wake_sender));
        let outstanding = Arc::new(Outstanding::new(
            usize::try_from(effective.runtime.max_pending_requests).unwrap_or(1),
        ));
        let inbox = Arc::new(CommandInbox::new(InboxParts {
            sender: command_sender,
            outstanding,
            command_byte_cap: effective.effective_command_bytes(),
            ledger: ports.ledger.clone(),
            shutdown: Arc::clone(&shutdown),
        }));
        let core = ControlCore::new(setup, ports, inbox, shutdown);
        let manager = Manager {
            core,
            receivers: Receivers {
                ingestion,
                control,
                commands,
                wake,
            },
            summary: None,
            _ingestion_sender: ingestion_sender,
            _control_sender: control_sender,
        };
        Ok(manager)
    }

    /// Opens a view session (at most `viewer.max_sessions`).
    pub fn open_session(&mut self) -> Result<SessionHandle, ManagerError> {
        self.core.open_session()
    }

    /// Clonable, thread-safe shutdown request (e.g. for a signal watcher).
    pub fn shutdown_trigger(&self) -> ShutdownTrigger {
        self.core.shutdown_trigger()
    }

    /// Runs the control loop on the calling thread until the bounded
    /// shutdown completes.
    pub fn run(mut self) -> ShutdownSummary {
        loop {
            let iteration = self.iterate();
            if let Some(summary) = self.summary.take() {
                return summary;
            }
            if !iteration.more_ready {
                self.wait(iteration.next_deadline_ns);
            }
        }
    }

    /// One fair iteration (also driven directly by deterministic tests).
    pub(crate) fn iterate(&mut self) -> Iteration {
        if self.core.is_finished() {
            return Iteration {
                more_ready: false,
                next_deadline_ns: None,
            };
        }
        let budget = Slice::start(self.core.slice());
        self.core.start_once();
        self.core.check_shutdown_flag();
        let core = &mut self.core;
        let mut more = drain(
            &self.receivers.control,
            CONTROL_BATCH,
            &budget,
            |completion| {
                core.on_control(completion);
            },
        );
        more |= drain(
            &self.receivers.commands,
            COMMAND_BATCH,
            &budget,
            |command| {
                core.on_command(command);
            },
        );
        more |= drain(
            &self.receivers.ingestion,
            INGESTION_BATCH,
            &budget,
            |completion| {
                core.on_ingestion(completion);
            },
        );
        let levels = QueueLevels {
            commands: fill(&self.receivers.commands),
            ingestion: fill(&self.receivers.ingestion),
        };
        self.core.run_timers(levels);
        if let Some(summary) = self.core.progress_shutdown(&self.receivers.commands) {
            self.summary = Some(summary);
        }
        Iteration {
            more_ready: more && !self.core.is_finished(),
            next_deadline_ns: self.core.next_deadline(),
        }
    }

    /// Sleeps until a channel is ready or the next deadline (≤ [`IDLE_TICK`]).
    fn wait(&self, next_deadline_ns: Option<u64>) {
        let timeout = idle_timeout(self.core.now_boot_ns(), next_deadline_ns);
        let mut select = Select::new();
        select.recv(&self.receivers.control);
        select.recv(&self.receivers.commands);
        select.recv(&self.receivers.ingestion);
        select.recv(&self.receivers.wake);
        // Readiness only: the next iteration takes items in fairness order.
        let _ = select.ready_timeout(timeout);
        while self.receivers.wake.try_recv().is_ok() {}
    }

    #[cfg(test)]
    pub(crate) fn summary(&mut self) -> Option<ShutdownSummary> {
        self.summary.take()
    }

    #[cfg(test)]
    pub(crate) fn core(&self) -> &ControlCore {
        &self.core
    }

    /// `(control, commands, ingestion)` items still queued.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> (usize, usize, usize) {
        (
            self.receivers.control.len(),
            self.receivers.commands.len(),
            self.receivers.ingestion.len(),
        )
    }
}

impl Drop for Manager {
    /// Runs on a normal return of [`Manager::run`] and while a panic unwinds
    /// the control thread (fail-stop, SPEC.md §7): every frontend then stops
    /// and restores its terminal instead of showing a frozen view.
    fn drop(&mut self) {
        self.core.release_sessions();
    }
}

/// How long an idle loop sleeps at `now_ns`: until the next deadline,
/// within [`MIN_WAIT`]..=[`IDLE_TICK`].
fn idle_timeout(now_ns: u64, next_deadline_ns: Option<u64>) -> Duration {
    next_deadline_ns
        .map_or(IDLE_TICK, |deadline| until(now_ns, deadline))
        .clamp(MIN_WAIT, IDLE_TICK)
}

/// Real-time execution budget of one iteration.
struct Slice {
    started: Instant,
    budget: Duration,
}

impl Slice {
    fn start(budget: Duration) -> Self {
        Self {
            started: Instant::now(),
            budget,
        }
    }

    fn exhausted(&self) -> bool {
        self.started.elapsed() >= self.budget
    }
}

/// Handles up to `cap` items (at least one when ready); returns whether
/// items are still waiting.
fn drain<T>(receiver: &Receiver<T>, cap: usize, budget: &Slice, mut handle: impl FnMut(T)) -> bool {
    for _ in 0..cap {
        let Ok(item) = receiver.try_recv() else {
            return false;
        };
        handle(item);
        if budget.exhausted() {
            break;
        }
    }
    !receiver.is_empty()
}

#[allow(clippy::cast_precision_loss)]
fn fill<T>(receiver: &Receiver<T>) -> f64 {
    match receiver.capacity() {
        Some(capacity) if capacity > 0 => receiver.len() as f64 / capacity as f64,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(items: usize) -> Receiver<usize> {
        let (sender, receiver) = bounded(items.max(1));
        for item in 0..items {
            sender.send(item).unwrap();
        }
        receiver
    }

    #[test]
    fn a_spent_slice_still_serves_one_item_per_category_and_yields() {
        // AT23: the control slice bounds an iteration; a category never
        // starves, but it stops after one item once the slice is spent.
        let spent = Slice::start(Duration::ZERO);
        let ready = queue(8);
        let mut handled = Vec::new();
        let more = drain(&ready, COMMAND_BATCH, &spent, |item| handled.push(item));
        assert_eq!(handled, vec![0]);
        assert!(more, "the rest waits for the next iteration");
    }

    /// An idle loop sleeps until its next deadline instead of waking on a
    /// fixed tick, never spinning and never past the idle bound.
    #[test]
    fn an_idle_loop_sleeps_until_its_next_deadline() {
        const MS: u64 = 1_000_000;
        assert_eq!(idle_timeout(1_000 * MS, None), IDLE_TICK);
        assert_eq!(
            idle_timeout(1_000 * MS, Some(1_120 * MS)),
            Duration::from_millis(120)
        );
        assert_eq!(idle_timeout(1_000 * MS, Some(1_000 * MS)), MIN_WAIT);
        assert_eq!(idle_timeout(1_000 * MS, Some(10 * MS)), MIN_WAIT);
        assert_eq!(idle_timeout(0, Some(60_000 * MS)), IDLE_TICK);
        assert_eq!(IDLE_TICK, Duration::from_millis(500));
    }

    #[test]
    fn a_fresh_slice_serves_at_most_the_category_batch() {
        let fresh = Slice::start(Duration::from_secs(60));
        let ready = queue(20);
        let mut handled = 0usize;
        let more = drain(&ready, CONTROL_BATCH, &fresh, |_| handled += 1);
        assert_eq!(handled, CONTROL_BATCH);
        assert!(more);
        let empty = queue(0);
        assert!(!drain(&empty, INGESTION_BATCH, &fresh, |_| unreachable!()));
    }
}
