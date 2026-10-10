//! The single supervisor thread: `poll` over every host pipe plus a wake
//! pipe, timers derived from the boot clock, and nothing that can block.

use std::os::fd::{AsFd, OwnedFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use dgxtop_core::budget::BytePermit;
use dgxtop_core::clock::{NANOS_PER_MILLI, ms_to_ns};
use dgxtop_core::ports::{AdapterFailureKind, IngestionCompletion, LogLevel};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use super::context::LoopContext;
use super::report::ShutdownReport;
use super::requests::failure;
use super::shared::SupervisorCommand;
use super::slot::{HostSlot, PipeRole};

/// Longest poll wait without any timer.
const IDLE_POLL_NS: u64 = 50 * NANOS_PER_MILLI;
/// Retry interval while the ingestion channel is full.
const OUTBOX_RETRY_NS: u64 = 10 * NANOS_PER_MILLI;
/// Commands handled per loop turn, so a burst cannot starve pipe I/O.
const MAX_COMMANDS_PER_TURN: usize = 64;
/// Extra time on top of the stop sequence when no deadline was given.
const DEFAULT_SHUTDOWN_SLACK_MS: u64 = 500;
/// Wake-pipe reads per turn; a still-readable pipe just wakes the next poll.
const MAX_WAKE_READS: usize = 16;

pub(crate) struct EventLoop {
    pub slots: Vec<HostSlot>,
    pub ctx: LoopContext,
    pub commands: Receiver<SupervisorCommand>,
    pub wake_reader: OwnedFd,
    pub finished: Sender<ShutdownReport>,
    /// Fixed working set (scratch, hello and request buffers) reserved from
    /// the ledger for the supervisor's lifetime.
    pub working_set: BytePermit,
    pub shutdown_started: Option<Instant>,
}

impl EventLoop {
    pub(crate) fn run(mut self) {
        let outcome = catch_unwind(AssertUnwindSafe(|| self.run_loop()));
        let completed = outcome.unwrap_or_else(|_| {
            self.ctx.log(
                LogLevel::Error,
                "supervisor thread panicked; managed hosts are being killed",
            );
            false
        });
        self.fail_leftovers();
        let report = self.report(completed);
        let _ = self.finished.try_send(report);
        drop(self.working_set);
    }

    fn now(&self) -> u64 {
        self.ctx.clock.now_boot_ns()
    }

    /// Returns whether every host settled before the shutdown deadline.
    fn run_loop(&mut self) -> bool {
        let now = self.now();
        for slot in &mut self.slots {
            slot.start(now, &mut self.ctx);
        }
        loop {
            let now = self.now();
            self.observe_shutdown_request(now);
            self.process_commands(now);
            // Recycled broker results hold ingress bytes until their deadline;
            // purge them even when no new read arrives.
            self.ctx.shared.lock().broker.purge_expired(now);
            for slot in &mut self.slots {
                slot.tick(now, &mut self.ctx);
                slot.refresh_shared_status(&self.ctx);
            }
            self.ctx.outbox.flush();
            if let Some(completed) = self.shutdown_finished(now) {
                return completed;
            }
            let timeout = self.poll_timeout(now);
            self.wait_and_dispatch(timeout);
        }
    }

    fn observe_shutdown_request(&mut self, now: u64) {
        if self.ctx.shutting_down() || !self.ctx.shared.shutdown_requested() {
            return;
        }
        let config = &self.ctx.config;
        let default_ms = config.child_grace_ms
            + config.child_term_ms
            + config.child_reap_ms
            + DEFAULT_SHUTDOWN_SLACK_MS;
        let deadline = self
            .ctx
            .shared
            .shutdown_deadline()
            .unwrap_or_else(|| now.saturating_add(ms_to_ns(default_ms)));
        self.ctx.shutdown_deadline = Some(deadline);
        self.shutdown_started = Some(Instant::now());
        self.ctx
            .log(LogLevel::Info, "supervisor shutting down adapter hosts");
        for slot in &mut self.slots {
            slot.begin_shutdown(now, &mut self.ctx);
        }
    }

    fn shutdown_finished(&self, now: u64) -> Option<bool> {
        let deadline = self.ctx.shutdown_deadline?;
        if self.slots.iter().all(HostSlot::is_settled) {
            return Some(true);
        }
        (now >= deadline).then_some(false)
    }

    fn process_commands(&mut self, now: u64) {
        for _ in 0..MAX_COMMANDS_PER_TURN {
            match self.commands.try_recv() {
                Ok(command) => self.handle_command(command, now),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    fn handle_command(&mut self, command: SupervisorCommand, now: u64) {
        match command {
            SupervisorCommand::Execute(execution) => {
                if let Some(slot) = self.slots.get_mut(execution.index) {
                    slot.execute(*execution, now, &mut self.ctx);
                }
            }
            SupervisorCommand::Deliver(completion) => self.ctx.emit(*completion),
        }
    }

    fn poll_timeout(&self, now: u64) -> PollTimeout {
        let mut next = now.saturating_add(IDLE_POLL_NS);
        for slot in &self.slots {
            if let Some(deadline) = slot.next_deadline(now) {
                next = next.min(deadline);
            }
        }
        if !self.ctx.outbox.is_empty() {
            next = next.min(now.saturating_add(OUTBOX_RETRY_NS));
        }
        if let Some(deadline) = self.ctx.shutdown_deadline {
            next = next.min(deadline);
        }
        let wait_ns = next.saturating_sub(now);
        let millis = wait_ns.div_ceil(NANOS_PER_MILLI);
        PollTimeout::from(u16::try_from(millis).unwrap_or(u16::MAX))
    }

    fn wait_and_dispatch(&mut self, timeout: PollTimeout) {
        let ready = self.poll_ready(timeout);
        let now = self.now();
        for (role, _events) in ready {
            match role {
                None => self.drain_wake_pipe(),
                Some((index, pipe)) => self.dispatch_ready(index, pipe, now),
            }
        }
    }

    /// Polls and returns the ready entries (`None` = wake pipe).
    fn poll_ready(&self, timeout: PollTimeout) -> Vec<(Option<(usize, PipeRole)>, PollFlags)> {
        let mut descriptors = vec![PollFd::new(self.wake_reader.as_fd(), PollFlags::POLLIN)];
        let mut roles = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            slot.register(index, &mut descriptors, &mut roles);
        }
        match poll(&mut descriptors, timeout) {
            Ok(0) | Err(Errno::EINTR) => return Vec::new(),
            Ok(_) => {}
            Err(errno) => {
                self.ctx
                    .log(LogLevel::Error, &format!("supervisor poll failed: {errno}"));
                // Avoid a hot loop on a persistent poll error.
                std::thread::sleep(Duration::from_millis(10));
                return Vec::new();
            }
        }
        let wake = std::iter::once(None);
        let pipes = roles.into_iter().map(Some);
        descriptors
            .iter()
            .zip(wake.chain(pipes))
            .filter_map(|(descriptor, role)| {
                let events = descriptor.revents().unwrap_or(PollFlags::empty());
                (!events.is_empty()).then_some((role, events))
            })
            .collect()
    }

    fn dispatch_ready(&mut self, index: usize, pipe: PipeRole, now: u64) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        match pipe {
            PipeRole::Replies => slot.on_replies_ready(now, &mut self.ctx),
            PipeRole::Diagnostics => slot.on_diagnostics_ready(now, &mut self.ctx),
            PipeRole::Requests => slot.on_requests_writable(now, &mut self.ctx),
        }
    }

    fn drain_wake_pipe(&self) {
        let mut sink = [0u8; 256];
        for _ in 0..MAX_WAKE_READS {
            match nix::unistd::read(&self.wake_reader, &mut sink) {
                Ok(read) if read > 0 => {}
                _ => return,
            }
        }
    }

    /// Fails jobs that were queued or joined but will never run.
    fn fail_leftovers(&mut self) {
        let now = self.now();
        while let Ok(command) = self.commands.try_recv() {
            self.handle_command(command, now);
        }
        let stopped = failure(AdapterFailureKind::Shutdown, "supervisor stopped");
        let pending = self.ctx.shared.lock().broker.fail_all_in_flight(&stopped);
        for (key, requesters) in pending {
            for request_id in requesters {
                self.ctx.emit(IngestionCompletion::Batch {
                    adapter_id: key.adapter_id.clone(),
                    request_id,
                    host_generation: key.host_generation,
                    result: Err(stopped.clone()),
                });
            }
        }
        self.ctx.outbox.flush();
    }

    fn report(&mut self, completed: bool) -> ShutdownReport {
        let elapsed = self
            .shutdown_started
            .map_or(Duration::ZERO, |started| started.elapsed());
        ShutdownReport {
            elapsed,
            completed_within_deadline: completed,
            hosts: self
                .slots
                .iter_mut()
                .map(HostSlot::shutdown_outcome)
                .collect(),
        }
    }
}
