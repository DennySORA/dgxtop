//! State machine of one supervised backend.
//!
//! ```text
//! NotStarted ─launch→ AwaitHello ─hello ok→ Idle ⇄ Busy (single in-flight)
//!                          │                         │ deadline: failure emitted,
//!                          │                         │ late reply awaited & skipped
//!     crash / protocol error / hello timeout / threshold reached
//!                          ↓
//!   Stopping: protocol shutdown → SIGTERM → SIGKILL → reap
//!        reaped & below threshold → relaunch (generation + 1)
//!        reaped & circuit tripped → CircuitOpen(backoff) → half-open launch
//!                                   → discover probe → Idle (healthy)
//!        not reaped               → Quarantined (never respawned)
//! ```
//!
//! A timeout never pretends to cancel the host's blocking call: the request
//! stays outstanding inside the child, its late reply is read and discarded,
//! and each further deadline-length of silence counts as another failure so
//! a hung host reaches the restart threshold even though no new request can
//! be sent to it.

use std::sync::Arc;

use dgxtop_core::budget::{BytePermit, QueueTicket};
use dgxtop_core::clock::{NANOS_PER_MILLI, NANOS_PER_SECOND, ms_to_ns, ns_to_ms};
use dgxtop_core::identity::AdapterId;
use dgxtop_core::observation::{
    CatalogReply, DiscoverRequest, HostHello, Isolation, ObservationBatch, ReadPlan,
};
use dgxtop_core::ports::{
    AdapterFailure, AdapterFailureKind, AdapterJob, IngestionCompletion, IngressBatch, LogLevel,
};
use dgxtop_core::text::sanitize_message;
use dgxtop_core::view::{HostState, HostStatus};
use nix::poll::{PollFd, PollFlags};
use uuid::Uuid;

use crate::broker::{BrokerKey, BrokerResult};
use crate::ipc::{
    DecodedSize, FrameDecoder, HELLO_FRAME_BYTES, HelloExpectation, IpcError, REQUEST_FRAME_BYTES,
    Step, decode_batch, decode_catalog, decode_hello, encode_frame,
};

use super::circuit::{CircuitBreaker, backoff_seed};
use super::config::SupervisorConfig;
use super::connection::{Connection, DiagnosticsRead, ReadOutcome};
use super::context::{HOST_LOG_TARGET, LoopContext};
use super::credit::{discover_credit, frame_credit};
use super::launch::{
    ChildControl, ExitSummary, HostLaunch, HostSpec, LaunchRequest, SignalDelivery,
};
use super::report::{HostShutdown, StopOutcome, StopStage};
use super::requests::{self, Revisions, failed_completion, failure};
use super::shared::Execution;
use super::status_emission;
use super::stderr_drain::StderrDrain;

/// Poll interval while a reap or a credit retry is pending.
pub(crate) const FAST_POLL_NS: u64 = 5 * NANOS_PER_MILLI;
/// Interval of opportunistic reaping of a quarantined child.
const QUARANTINE_RECHECK_NS: u64 = NANOS_PER_SECOND;
/// Smallest interval between unresponsive-host strikes.
const MIN_STRIKE_INTERVAL_NS: u64 = 10 * NANOS_PER_MILLI;
/// Reads per readiness event, so one chatty host cannot starve others.
const MAX_READS_PER_WAKE: usize = 64;
/// Diagnostics reads per readiness event.
const MAX_DIAGNOSTIC_READS_PER_WAKE: usize = 4;
/// Bound of the operator-facing status detail (newest note first).
const MAX_DETAIL_BYTES: usize = 512;
/// Final stderr reads after a host was reaped.
const MAX_FINAL_DIAGNOSTIC_READS: usize = 16;

/// Which pipe a poll entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipeRole {
    Replies,
    Diagnostics,
    Requests,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterStop {
    Restart,
    Retire,
}

#[derive(Debug)]
struct StopState {
    stage: StopStage,
    stage_deadline_boot_ns: u64,
    after: AfterStop,
}

enum InFlightKind {
    Read {
        plan: Box<ReadPlan>,
        key: BrokerKey,
    },
    Discover {
        request: Box<DiscoverRequest>,
    },
    /// Half-open discover probe of a new generation (never forwarded).
    Probe {
        request_id: Uuid,
    },
}

struct InFlight {
    kind: InFlightKind,
    sent_boot_ns: u64,
    deadline_boot_ns: u64,
    credit: Vec<QueueTicket>,
    /// The deadline passed: the failure was emitted, the reply is skipped.
    late: bool,
    /// Whether continued silence counts as further failures.
    count_strikes: bool,
    next_strike_boot_ns: u64,
    strike_interval_ns: u64,
}

impl InFlight {
    fn new(kind: InFlightKind, now: u64, deadline: u64, credit: Vec<QueueTicket>) -> Self {
        let interval = deadline.saturating_sub(now).max(MIN_STRIKE_INTERVAL_NS);
        Self {
            kind,
            sent_boot_ns: now,
            deadline_boot_ns: deadline,
            credit,
            late: false,
            count_strikes: true,
            next_strike_boot_ns: deadline.saturating_add(interval),
            strike_interval_ns: interval,
        }
    }

    fn held_credit(&self) -> u64 {
        self.credit.iter().map(QueueTicket::bytes).sum()
    }
}

enum Phase {
    NotStarted,
    AwaitHello {
        deadline_boot_ns: u64,
        probe_after: bool,
    },
    Idle,
    Busy(Box<InFlight>),
    Stopping(StopState),
    CircuitOpen {
        until_boot_ns: u64,
    },
    Quarantined,
    Retired,
}

/// What a timer tick must do (decided before mutating).
enum TimerAction {
    Nothing,
    HelloTimeout,
    Deadline,
    Strike,
    RetryCredit,
    AdvanceStop,
    HalfOpen,
    RecheckQuarantine,
}

/// What to do with a frame whose length just arrived.
enum BodyDecision {
    Admit,
    Skip,
    NeedCredit,
    Reject(&'static str),
}

pub(crate) struct HostSlot {
    index: usize,
    adapter_id: AdapterId,
    launch: HostLaunch,
    generation: u32,
    phase: Phase,
    connection: Option<Connection>,
    quarantined: Option<Box<dyn ChildControl>>,
    quarantined_pid: Option<i32>,
    circuit: CircuitBreaker,
    timeouts_total: u64,
    restarts_total: u64,
    late_replies_total: u64,
    last_success_boot_ns: Option<u64>,
    detail: Option<String>,
    revisions: Revisions,
    outcome: StopOutcome,
    next_quarantine_check_ns: u64,
    last_published: Option<HostStatus>,
}

impl HostSlot {
    pub(crate) fn new(index: usize, spec: HostSpec, config: &SupervisorConfig) -> Self {
        let seed = backoff_seed(spec.adapter_id.as_str());
        Self {
            index,
            circuit: CircuitBreaker::new(
                config.failure_threshold,
                config.backoff_base_ms,
                config.max_backoff_ms,
                config.backoff_jitter_permille,
                seed,
            ),
            adapter_id: spec.adapter_id,
            launch: spec.launch,
            generation: 0,
            phase: Phase::NotStarted,
            connection: None,
            quarantined: None,
            quarantined_pid: None,
            timeouts_total: 0,
            restarts_total: 0,
            late_replies_total: 0,
            last_success_boot_ns: None,
            detail: None,
            revisions: Revisions::default(),
            outcome: StopOutcome::NotRunning,
            next_quarantine_check_ns: 0,
            last_published: None,
        }
    }

    pub(crate) fn start(&mut self, now: u64, ctx: &mut LoopContext) {
        self.launch(now, false, ctx);
    }

    // ----- launching -------------------------------------------------------

    fn next_generation(&self) -> Option<u32> {
        let next = self.generation.checked_add(1)?;
        i32::try_from(next).is_ok().then_some(next)
    }

    fn launch(&mut self, now: u64, probe_after: bool, ctx: &mut LoopContext) {
        let Some(generation) = self.next_generation() else {
            self.detail = Some("host generation counter exhausted".into());
            self.phase = Phase::Quarantined;
            self.publish(ctx);
            return;
        };
        if self.generation > 0 {
            self.restarts_total += 1;
        }
        self.generation = generation;
        match self.spawn_connection(generation, now, ctx) {
            Ok(connection) => {
                self.connection = Some(connection);
                self.phase = Phase::AwaitHello {
                    deadline_boot_ns: now.saturating_add(ms_to_ns(ctx.config.hello_timeout_ms)),
                    probe_after,
                };
                if probe_after {
                    self.note(format!("half-open: probing generation {generation}"));
                }
                self.set_ready(ctx, false);
                self.publish(ctx);
            }
            Err(message) => {
                ctx.log(LogLevel::Warn, &format!("{}: {message}", self.adapter_id));
                self.note(message);
                self.circuit.record_failure();
                self.open_circuit(now, ctx);
            }
        }
    }

    fn spawn_connection(
        &self,
        generation: u32,
        now: u64,
        ctx: &LoopContext,
    ) -> Result<Connection, String> {
        let decoder = FrameDecoder::new(ctx.limits.max_frame_bytes())
            .map_err(|error| format!("generation {generation} decoder: {error}"))?;
        let nonce = Uuid::new_v4().simple().to_string();
        let request = LaunchRequest {
            adapter_id: &self.adapter_id,
            launch: &self.launch,
            generation,
            nonce: &nonce,
            clock: &ctx.clock,
            limits: ctx.limits,
        };
        let launched = ctx
            .launcher
            .launch(&request)
            .map_err(|error| format!("launch of generation {generation} failed: {error}"))?;
        let drain = StderrDrain::new(
            ctx.config.stderr_lines_per_window,
            ms_to_ns(ctx.config.stderr_window_ms),
            now,
        );
        Ok(Connection::new(launched, nonce, decoder, drain))
    }

    fn open_circuit(&mut self, now: u64, ctx: &mut LoopContext) {
        if ctx.shutting_down() {
            self.phase = Phase::Retired;
            self.set_ready(ctx, false);
            self.publish(ctx);
            return;
        }
        let delay_ms = self.circuit.next_delay_ms();
        self.phase = Phase::CircuitOpen {
            until_boot_ns: now.saturating_add(ms_to_ns(delay_ms)),
        };
        let message = format!(
            "circuit open for {delay_ms} ms after {} consecutive failures",
            self.circuit.consecutive_failures()
        );
        ctx.log(LogLevel::Warn, &format!("{}: {message}", self.adapter_id));
        self.note(message);
        self.set_ready(ctx, false);
        self.publish(ctx);
    }

    // ----- jobs ------------------------------------------------------------

    pub(crate) fn execute(&mut self, execution: Execution, now: u64, ctx: &mut LoopContext) {
        let ready = matches!(self.phase, Phase::Idle) && self.connection.is_some();
        if !ready {
            let kind = if ctx.shutting_down() {
                AdapterFailureKind::Shutdown
            } else {
                AdapterFailureKind::HostUnavailable
            };
            fail_execution(&execution, failure(kind, "host is not ready"), ctx);
            self.release_claim(ctx);
            return;
        }
        let frame = match encode_frame(&execution.request, REQUEST_FRAME_BYTES) {
            Ok(frame) => frame,
            Err(error) => {
                let message = format!("request encoding failed: {error}");
                fail_execution(
                    &execution,
                    failure(AdapterFailureKind::Protocol, &message),
                    ctx,
                );
                self.release_claim(ctx);
                return;
            }
        };
        self.revisions = Revisions {
            catalog: execution.request.catalog_revision,
            config: execution.request.config_revision,
        };
        let deadline = execution.request.deadline_boot_ns;
        let kind = match (execution.job, execution.broker_key) {
            (AdapterJob::Read(plan), key) => InFlightKind::Read {
                key: key.unwrap_or_else(|| BrokerKey::from_plan(&plan)),
                plan: Box::new(plan),
            },
            (AdapterJob::Discover(request), _) => InFlightKind::Discover {
                request: Box::new(request),
            },
        };
        self.phase = Phase::Busy(Box::new(InFlight::new(
            kind,
            now,
            deadline,
            vec![execution.ticket],
        )));
        self.set_ready(ctx, false);
        self.send_frame(frame, now, ctx);
    }

    fn send_frame(&mut self, frame: Vec<u8>, now: u64, ctx: &mut LoopContext) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        connection.queue_frame(frame);
        if let Err(errno) = connection.flush_requests() {
            let message = format!("request pipe failed: {errno}");
            self.host_failed(now, AdapterFailureKind::HostUnavailable, &message, ctx);
        }
    }

    fn send_probe(&mut self, now: u64, ctx: &mut LoopContext) {
        let deadline = now.saturating_add(ms_to_ns(ctx.config.probe_deadline_ms));
        let request =
            requests::probe_request(&self.adapter_id, self.generation, deadline, self.revisions);
        let frame = request.and_then(|request| {
            encode_frame(&request, REQUEST_FRAME_BYTES)
                .map(|frame| (request.request_id, frame))
                .map_err(|error| error.to_string())
        });
        let (request_id, frame) = match frame {
            Ok(encoded) => encoded,
            Err(message) => {
                self.host_failed(now, AdapterFailureKind::Protocol, &message, ctx);
                return;
            }
        };
        // Missing credit is topped up when the reply's length is known.
        let credit = ctx.ingress.try_admit(discover_credit(&ctx.limits)).ok();
        self.phase = Phase::Busy(Box::new(InFlight::new(
            InFlightKind::Probe { request_id },
            now,
            deadline,
            credit.into_iter().collect(),
        )));
        self.set_ready(ctx, false);
        self.publish(ctx);
        self.send_frame(frame, now, ctx);
    }

    // ----- timers ----------------------------------------------------------

    fn timer_action(&self, now: u64, shutting_down: bool) -> TimerAction {
        match &self.phase {
            Phase::AwaitHello {
                deadline_boot_ns, ..
            } if now >= *deadline_boot_ns => TimerAction::HelloTimeout,
            Phase::Busy(inflight) => {
                let paused = self.connection.as_ref().is_some_and(Connection::is_paused);
                if !inflight.late && now >= inflight.deadline_boot_ns {
                    TimerAction::Deadline
                } else if !inflight.late && paused {
                    TimerAction::RetryCredit
                } else if inflight.late
                    && inflight.count_strikes
                    && now >= inflight.next_strike_boot_ns
                {
                    TimerAction::Strike
                } else {
                    TimerAction::Nothing
                }
            }
            Phase::Stopping(_) => TimerAction::AdvanceStop,
            Phase::CircuitOpen { until_boot_ns } if now >= *until_boot_ns && !shutting_down => {
                TimerAction::HalfOpen
            }
            Phase::Quarantined
                if self.quarantined.is_some() && now >= self.next_quarantine_check_ns =>
            {
                TimerAction::RecheckQuarantine
            }
            _ => TimerAction::Nothing,
        }
    }

    pub(crate) fn tick(&mut self, now: u64, ctx: &mut LoopContext) {
        match self.timer_action(now, ctx.shutting_down()) {
            TimerAction::Nothing => {}
            TimerAction::HelloTimeout => self.hello_timed_out(now, ctx),
            TimerAction::Deadline => self.on_deadline(now, ctx),
            TimerAction::Strike => self.strike(now, ctx),
            TimerAction::RetryCredit => self.retry_paused_credit(ctx),
            TimerAction::AdvanceStop => self.advance_stop(now, ctx),
            TimerAction::HalfOpen => self.launch(now, true, ctx),
            TimerAction::RecheckQuarantine => self.recheck_quarantine(now, ctx),
        }
    }

    /// Earliest boot time this slot needs attention.
    pub(crate) fn next_deadline(&self, now: u64) -> Option<u64> {
        let soon = now.saturating_add(FAST_POLL_NS);
        match &self.phase {
            Phase::AwaitHello {
                deadline_boot_ns, ..
            } => Some(*deadline_boot_ns),
            Phase::Busy(inflight) => {
                let paused = self.connection.as_ref().is_some_and(Connection::is_paused);
                if !inflight.late {
                    Some(if paused {
                        soon.min(inflight.deadline_boot_ns)
                    } else {
                        inflight.deadline_boot_ns
                    })
                } else if inflight.count_strikes {
                    Some(inflight.next_strike_boot_ns)
                } else {
                    None
                }
            }
            Phase::Stopping(stop) => Some(soon.min(stop.stage_deadline_boot_ns)),
            Phase::CircuitOpen { until_boot_ns } => Some(*until_boot_ns),
            Phase::Quarantined if self.quarantined.is_some() => Some(self.next_quarantine_check_ns),
            _ => None,
        }
    }

    fn hello_timed_out(&mut self, now: u64, ctx: &mut LoopContext) {
        self.timeouts_total += 1;
        let message = format!(
            "generation {} sent no hello within {} ms",
            self.generation, ctx.config.hello_timeout_ms
        );
        self.host_failed(now, AdapterFailureKind::HostUnavailable, &message, ctx);
    }

    fn on_deadline(&mut self, now: u64, ctx: &mut LoopContext) {
        let paused = self.connection.as_ref().is_some_and(Connection::is_paused);
        if paused {
            self.reject_for_credit(now, ctx);
            return;
        }
        let receiving = self
            .connection
            .as_ref()
            .is_some_and(|connection| connection.decoder().is_receiving_body());
        let Phase::Busy(inflight) = &mut self.phase else {
            return;
        };
        let waited_ms = ns_to_ms(
            inflight
                .deadline_boot_ns
                .saturating_sub(inflight.sent_boot_ns),
        );
        let is_probe = matches!(inflight.kind, InFlightKind::Probe { .. });
        if !receiving {
            // The late reply will be skipped without allocation.
            inflight.credit.clear();
        }
        self.timeouts_total += 1;
        let message = format!("no reply within {waited_ms} ms; a late reply will be discarded");
        if is_probe {
            self.probe_failed(now, &message, ctx);
            return;
        }
        self.fail_active_request(AdapterFailureKind::TimedOut, &message, ctx);
        if let Phase::Busy(inflight) = &mut self.phase {
            // fail_active_request disabled strikes; a timeout keeps them.
            inflight.count_strikes = true;
        }
        self.note(message);
        if self.circuit.record_failure() {
            self.begin_stop(now, AfterStop::Restart, ctx);
        } else {
            self.publish(ctx);
        }
    }

    /// The host answered in time but the ingress budget had no room for the
    /// reply until the deadline: the whole batch is rejected (no partial
    /// commit) and the frame is skipped. The host itself is healthy.
    fn reject_for_credit(&mut self, now: u64, ctx: &mut LoopContext) {
        if let Some(connection) = self.connection.as_mut() {
            connection.set_paused(false);
            if connection.decoder_mut().skip_body().is_err() {
                let error = IpcError::contract("credit pause without a pending frame");
                self.protocol_failure(now, &error, ctx);
                return;
            }
        }
        let is_probe = matches!(
            &self.phase,
            Phase::Busy(inflight) if matches!(inflight.kind, InFlightKind::Probe { .. })
        );
        if is_probe {
            self.probe_failed(
                now,
                "probe reply exceeded the available ingress credit",
                ctx,
            );
            return;
        }
        let message = "ingress credit unavailable until the deadline; batch rejected";
        self.fail_active_request(AdapterFailureKind::CapacityExceeded, message, ctx);
        if let Phase::Busy(inflight) = &mut self.phase {
            inflight.credit.clear();
        }
        self.note(message.into());
        self.publish(ctx);
    }

    fn strike(&mut self, now: u64, ctx: &mut LoopContext) {
        if let Phase::Busy(inflight) = &mut self.phase {
            inflight.next_strike_boot_ns = inflight
                .next_strike_boot_ns
                .saturating_add(inflight.strike_interval_ns);
        }
        let tripped = self.circuit.record_failure();
        self.note(format!(
            "host still unresponsive after its deadline ({} consecutive failures)",
            self.circuit.consecutive_failures()
        ));
        if tripped {
            self.begin_stop(now, AfterStop::Restart, ctx);
        } else {
            self.publish(ctx);
        }
    }

    fn retry_paused_credit(&mut self, ctx: &mut LoopContext) {
        let Some(declared) = self
            .connection
            .as_ref()
            .and_then(|connection| connection.decoder().pending_declared())
        else {
            return;
        };
        if !self.ensure_credit(declared, ctx) {
            return;
        }
        if let Some(connection) = self.connection.as_mut() {
            connection.set_paused(false);
            // A pending decision exists (checked above), so admission works.
            let _ = connection.decoder_mut().admit_body();
        }
    }

    // ----- reading ---------------------------------------------------------

    pub(crate) fn on_replies_ready(&mut self, now: u64, ctx: &mut LoopContext) {
        for _ in 0..MAX_READS_PER_WAKE {
            let Some(connection) = self.connection.as_mut() else {
                return;
            };
            if connection.is_paused() {
                return;
            }
            if let Some(declared) = connection.decoder().pending_declared() {
                if !self.decide_body(declared, now, ctx) {
                    return;
                }
                continue;
            }
            match connection.read_step(&mut ctx.scratch) {
                ReadOutcome::WouldBlock => return,
                ReadOutcome::Eof => {
                    self.replies_closed(now, None, ctx);
                    return;
                }
                ReadOutcome::Io(message) => {
                    self.replies_closed(now, Some(message), ctx);
                    return;
                }
                ReadOutcome::Protocol(error) => {
                    self.protocol_failure(now, &error, ctx);
                    return;
                }
                ReadOutcome::Step(step) => {
                    if !self.on_step(step, now, ctx) {
                        return;
                    }
                }
            }
        }
    }

    /// Handles one decoder step; `false` stops reading for this wake-up.
    fn on_step(&mut self, step: Step, now: u64, ctx: &mut LoopContext) -> bool {
        match step {
            Step::NeedMore { .. } => true,
            Step::Header { declared, .. } => self.decide_body(declared, now, ctx),
            Step::Frame { payload, .. } => {
                self.on_frame(payload, now, ctx);
                self.connection.is_some()
            }
            Step::Skipped { .. } => {
                self.on_skipped(now, ctx);
                true
            }
        }
    }

    fn body_decision(&self, declared: usize) -> BodyDecision {
        match &self.phase {
            Phase::AwaitHello { .. } if declared <= HELLO_FRAME_BYTES => BodyDecision::Admit,
            Phase::AwaitHello { .. } => BodyDecision::Reject("hello frame exceeds 64 KiB"),
            Phase::Busy(inflight) if inflight.late => BodyDecision::Skip,
            Phase::Busy(_) => BodyDecision::NeedCredit,
            // Shutdown acks are tiny; anything else while stopping is a late
            // reply that is skipped.
            Phase::Stopping(_) if declared <= HELLO_FRAME_BYTES => BodyDecision::Admit,
            Phase::Stopping(_) => BodyDecision::Skip,
            _ => BodyDecision::Reject("unsolicited frame without a request in flight"),
        }
    }

    /// Admits, skips or pauses the pending body; `false` stops reading.
    fn decide_body(&mut self, declared: usize, now: u64, ctx: &mut LoopContext) -> bool {
        let admitted = match self.body_decision(declared) {
            BodyDecision::Admit => self.admit_pending(),
            BodyDecision::Skip => self
                .connection
                .as_mut()
                .map_or(Ok(()), |connection| connection.decoder_mut().skip_body()),
            BodyDecision::NeedCredit if self.ensure_credit(declared, ctx) => self.admit_pending(),
            BodyDecision::NeedCredit => {
                if let Some(connection) = self.connection.as_mut() {
                    connection.set_paused(true);
                }
                return false;
            }
            BodyDecision::Reject(reason) => Err(IpcError::contract(reason)),
        };
        match admitted {
            Ok(()) => true,
            Err(error) => {
                self.protocol_failure(now, &error, ctx);
                false
            }
        }
    }

    fn admit_pending(&mut self) -> Result<(), IpcError> {
        self.connection
            .as_mut()
            .map_or(Ok(()), |connection| connection.decoder_mut().admit_body())
    }

    /// Makes sure the in-flight request holds wire + decoded credit for a
    /// frame of `declared` bytes, topping up from the ingress gate.
    fn ensure_credit(&mut self, declared: usize, ctx: &mut LoopContext) -> bool {
        let required = frame_credit(declared, &ctx.limits);
        let Phase::Busy(inflight) = &mut self.phase else {
            return false;
        };
        let held = inflight.held_credit();
        if held >= required {
            return true;
        }
        match ctx.ingress.try_admit(required - held) {
            Ok(ticket) => {
                inflight.credit.push(ticket);
                true
            }
            Err(_) => false,
        }
    }

    fn on_frame(&mut self, payload: Vec<u8>, now: u64, ctx: &mut LoopContext) {
        match &self.phase {
            Phase::AwaitHello { probe_after, .. } => {
                let probe_after = *probe_after;
                self.on_hello(&payload, probe_after, now, ctx);
            }
            Phase::Busy(inflight) if inflight.late => self.discard_late_reply(now, ctx),
            Phase::Busy(_) => self.on_reply(payload, now, ctx),
            // Shutdown acknowledgements need no action: the reap decides.
            Phase::Stopping(_) => {}
            _ => {
                let error = IpcError::contract("unexpected frame");
                self.protocol_failure(now, &error, ctx);
            }
        }
    }

    fn on_skipped(&mut self, now: u64, ctx: &mut LoopContext) {
        if matches!(&self.phase, Phase::Busy(inflight) if inflight.late) {
            self.discard_late_reply(now, ctx);
        }
    }

    fn discard_late_reply(&mut self, now: u64, ctx: &mut LoopContext) {
        self.late_replies_total += 1;
        self.become_idle(now, false, ctx);
        self.publish(ctx);
    }

    fn on_hello(&mut self, payload: &[u8], probe_after: bool, now: u64, ctx: &mut LoopContext) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let expectation = HelloExpectation {
            adapter_id: &self.adapter_id,
            generation: self.generation,
            nonce: connection.nonce(),
        };
        let limits = ctx.limits.with_frame_cap(HELLO_FRAME_BYTES);
        let verdict = decode_hello(payload, &limits, &expectation)
            .and_then(|hello| check_isolation(&hello, &self.launch));
        match verdict {
            Err(error) => self.protocol_failure(now, &error, ctx),
            Ok(()) if probe_after => self.send_probe(now, ctx),
            Ok(()) => {
                self.become_idle(now, false, ctx);
                self.publish(ctx);
            }
        }
    }

    fn on_reply(&mut self, payload: Vec<u8>, now: u64, ctx: &mut LoopContext) {
        let Phase::Busy(inflight) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            return;
        };
        let inflight = *inflight;
        match inflight.kind {
            InFlightKind::Read { plan, key } => {
                let reply = ReadReply {
                    plan: &plan,
                    key: &key,
                    credit: inflight.credit,
                };
                self.finish_read(payload, reply, now, ctx);
            }
            InFlightKind::Discover { request } => {
                self.finish_discover(&payload, &request, now, ctx)
            }
            InFlightKind::Probe { request_id } => self.finish_probe(&payload, request_id, now, ctx),
        }
        self.publish(ctx);
    }

    fn finish_read(
        &mut self,
        payload: Vec<u8>,
        reply: ReadReply<'_>,
        now: u64,
        ctx: &mut LoopContext,
    ) {
        let decoded = decode_validated_batch(&payload, reply.plan, ctx);
        drop(payload);
        let batch = match decoded {
            Ok(batch) => batch,
            Err(error) => {
                let message = format!("invalid batch: {error}");
                complete_read(
                    reply.key,
                    reply.plan,
                    Err(failure(AdapterFailureKind::Protocol, &message)),
                    ctx,
                );
                self.protocol_failure(now, &error, ctx);
                return;
            }
        };
        match account(batch.decoded_bytes(), reply.credit, ctx) {
            Ok(permit) => {
                let shared = Arc::new(IngressBatch::new(batch, Some(permit)));
                complete_read(reply.key, reply.plan, Ok(shared), ctx);
            }
            Err(message) => {
                let rejected = failure(AdapterFailureKind::CapacityExceeded, &message);
                complete_read(reply.key, reply.plan, Err(rejected), ctx);
            }
        }
        self.become_idle(now, true, ctx);
    }

    fn finish_discover(
        &mut self,
        payload: &[u8],
        request: &DiscoverRequest,
        now: u64,
        ctx: &mut LoopContext,
    ) {
        match decode_validated_catalog(payload, request.request_id, self.generation, ctx) {
            Ok(reply) => {
                ctx.emit(IngestionCompletion::Catalog {
                    adapter_id: self.adapter_id.clone(),
                    request_id: request.request_id,
                    host_generation: request.host_generation,
                    result: Ok(reply.delta),
                });
                self.become_idle(now, true, ctx);
            }
            Err(error) => {
                let message = format!("invalid catalog: {error}");
                ctx.emit(IngestionCompletion::Catalog {
                    adapter_id: self.adapter_id.clone(),
                    request_id: request.request_id,
                    host_generation: request.host_generation,
                    result: Err(failure(AdapterFailureKind::Protocol, &message)),
                });
                self.protocol_failure(now, &error, ctx);
            }
        }
    }

    fn finish_probe(&mut self, payload: &[u8], request_id: Uuid, now: u64, ctx: &mut LoopContext) {
        match decode_validated_catalog(payload, request_id, self.generation, ctx) {
            Ok(_) => {
                self.become_idle(now, true, ctx);
                self.note(format!("generation {} passed its probe", self.generation));
            }
            Err(error) => self.probe_failed(now, &error.to_string(), ctx),
        }
    }

    fn probe_failed(&mut self, now: u64, message: &str, ctx: &mut LoopContext) {
        self.circuit.record_failure();
        self.note(format!(
            "half-open probe failed: {}",
            sanitize_message(message)
        ));
        self.begin_stop(now, AfterStop::Restart, ctx);
    }

    fn replies_closed(&mut self, now: u64, io_error: Option<String>, ctx: &mut LoopContext) {
        let truncated = self
            .connection
            .as_ref()
            .and_then(|connection| connection.decoder().finish().err());
        if let Some(connection) = self.connection.as_mut() {
            connection.close_replies();
        }
        match self.phase {
            Phase::Stopping(_) => self.advance_stop(now, ctx),
            Phase::NotStarted | Phase::CircuitOpen { .. } | Phase::Quarantined | Phase::Retired => {
            }
            _ => {
                let mut message = String::from("host closed its reply pipe");
                if let Some(error) = io_error {
                    message.push_str(&format!(" ({error})"));
                }
                if let Some(error) = truncated {
                    message.push_str(&format!("; {error}"));
                }
                self.host_failed(now, AdapterFailureKind::HostUnavailable, &message, ctx);
            }
        }
    }

    pub(crate) fn on_diagnostics_ready(&mut self, now: u64, ctx: &mut LoopContext) {
        for _ in 0..MAX_DIAGNOSTIC_READS_PER_WAKE {
            if !self.drain_diagnostics_once(now, ctx) {
                return;
            }
        }
    }

    /// One non-blocking read of the host's stderr; `false` when drained.
    fn drain_diagnostics_once(&mut self, now: u64, ctx: &mut LoopContext) -> bool {
        let Some(connection) = self.connection.as_mut() else {
            return false;
        };
        let adapter = &self.adapter_id;
        let logger = &ctx.logger;
        let mut forward = |line: String| {
            logger.log(
                LogLevel::Warn,
                HOST_LOG_TARGET,
                &format!("{adapter}: {line}"),
            );
        };
        match connection.read_diagnostics(&mut ctx.scratch) {
            DiagnosticsRead::Bytes(read) => {
                connection
                    .drain()
                    .absorb(&ctx.scratch[..read], now, &mut forward);
                true
            }
            DiagnosticsRead::WouldBlock => false,
            DiagnosticsRead::Eof => {
                connection.drain().finish(now, &mut forward);
                false
            }
        }
    }

    pub(crate) fn on_requests_writable(&mut self, now: u64, ctx: &mut LoopContext) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        if let Err(errno) = connection.flush_requests()
            && !matches!(self.phase, Phase::Stopping(_))
        {
            let message = format!("request pipe failed: {errno}");
            self.host_failed(now, AdapterFailureKind::HostUnavailable, &message, ctx);
        }
    }

    // ----- failures and stopping -------------------------------------------

    /// The incarnation cannot be trusted any more (crash, protocol error,
    /// broken pipe, missing hello): fail its request and replace it.
    fn host_failed(
        &mut self,
        now: u64,
        kind: AdapterFailureKind,
        message: &str,
        ctx: &mut LoopContext,
    ) {
        self.fail_active_request(kind, message, ctx);
        self.circuit.record_failure();
        let message = sanitize_message(message);
        ctx.log(
            LogLevel::Warn,
            &format!(
                "{} generation {}: {message}",
                self.adapter_id, self.generation
            ),
        );
        self.note(message);
        self.begin_stop(now, AfterStop::Restart, ctx);
    }

    fn protocol_failure(&mut self, now: u64, error: &IpcError, ctx: &mut LoopContext) {
        let message = format!("protocol error: {error}");
        self.host_failed(now, AdapterFailureKind::Protocol, &message, ctx);
    }

    /// Emits the failure for the request in flight (unless its deadline
    /// already produced one). Joined broker requesters get the same failure.
    fn fail_active_request(
        &mut self,
        kind: AdapterFailureKind,
        message: &str,
        ctx: &mut LoopContext,
    ) {
        let Phase::Busy(inflight) = &mut self.phase else {
            return;
        };
        if inflight.late {
            return;
        }
        inflight.late = true;
        inflight.count_strikes = false;
        let reason = failure(kind, message);
        match &inflight.kind {
            InFlightKind::Read { plan, key } => complete_read(key, plan, Err(reason), ctx),
            InFlightKind::Discover { request } => ctx.emit(IngestionCompletion::Catalog {
                adapter_id: request.adapter_id.clone(),
                request_id: request.request_id,
                host_generation: request.host_generation,
                result: Err(reason),
            }),
            InFlightKind::Probe { .. } => {}
        }
    }

    fn begin_stop(&mut self, now: u64, after: AfterStop, ctx: &mut LoopContext) {
        let kind = match after {
            AfterStop::Retire => AdapterFailureKind::Shutdown,
            AfterStop::Restart => AdapterFailureKind::HostUnavailable,
        };
        self.fail_active_request(kind, "host is being stopped", ctx);
        self.set_ready(ctx, false);
        self.release_claim(ctx);
        let grace_deadline = now.saturating_add(ctx.stop_durations(now).grace_ns);
        let Some(connection) = self.connection.as_mut() else {
            self.finish_stop(now, None, after, ctx);
            return;
        };
        connection.set_paused(false);
        let shutdown = requests::shutdown_request(
            &self.adapter_id,
            self.generation,
            grace_deadline,
            self.revisions,
        )
        .and_then(|request| {
            encode_frame(&request, REQUEST_FRAME_BYTES).map_err(|error| error.to_string())
        });
        if let Ok(frame) = shutdown {
            connection.queue_frame(frame);
        }
        connection.close_requests_after_flush();
        // A broken pipe only means the host is already gone; the reap decides.
        let _ = connection.flush_requests();
        self.phase = Phase::Stopping(StopState {
            stage: StopStage::Protocol,
            stage_deadline_boot_ns: grace_deadline,
            after,
        });
        self.publish(ctx);
        self.advance_stop(now, ctx);
    }

    fn advance_stop(&mut self, now: u64, ctx: &mut LoopContext) {
        let Phase::Stopping(stop) = &self.phase else {
            return;
        };
        let (stage, stage_deadline, after) = (stop.stage, stop.stage_deadline_boot_ns, stop.after);
        if let Some(exit) = self.try_reap(ctx) {
            self.finish_stop(now, Some((stage, exit)), after, ctx);
            return;
        }
        if now < stage_deadline {
            return;
        }
        let durations = ctx.stop_durations(now);
        match stage {
            StopStage::Protocol => {
                self.send_signal(StopStage::Terminate, ctx);
                self.set_stage(StopStage::Terminate, now.saturating_add(durations.term_ns));
            }
            StopStage::Terminate => {
                self.send_signal(StopStage::Kill, ctx);
                self.set_stage(StopStage::Kill, now.saturating_add(durations.reap_ns));
            }
            StopStage::Kill => self.quarantine(now, ctx),
        }
    }

    fn set_stage(&mut self, stage: StopStage, deadline: u64) {
        if let Phase::Stopping(stop) = &mut self.phase {
            stop.stage = stage;
            stop.stage_deadline_boot_ns = deadline;
        }
    }

    fn send_signal(&mut self, stage: StopStage, ctx: &mut LoopContext) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let delivery = match stage {
            StopStage::Terminate => connection.control().terminate(),
            _ => connection.control().kill(),
        };
        let note = match delivery {
            Ok(SignalDelivery::Sent | SignalDelivery::AlreadyExited) => return,
            Ok(SignalDelivery::Unsupported) => {
                "in-process host cannot be signalled; waiting for it to return".to_owned()
            }
            Err(error) => format!("{} failed: {error}", stage.label()),
        };
        ctx.log(LogLevel::Warn, &format!("{}: {note}", self.adapter_id));
    }

    fn try_reap(&mut self, ctx: &mut LoopContext) -> Option<ExitSummary> {
        let connection = self.connection.as_mut()?;
        match connection.control().try_reap() {
            Ok(exit) => exit,
            Err(error) => {
                ctx.log(
                    LogLevel::Warn,
                    &format!("{}: reap failed: {error}", self.adapter_id),
                );
                None
            }
        }
    }

    fn finish_stop(
        &mut self,
        now: u64,
        reaped: Option<(StopStage, ExitSummary)>,
        after: AfterStop,
        ctx: &mut LoopContext,
    ) {
        // Bounded: a grandchild holding the inherited stderr could otherwise
        // keep this pipe readable forever.
        for _ in 0..MAX_FINAL_DIAGNOSTIC_READS {
            if !self.drain_diagnostics_once(now, ctx) {
                break;
            }
        }
        self.connection = None;
        self.outcome = match reaped {
            Some((stage, exit)) => {
                let note = format!(
                    "generation {} stopped after {}: {exit}",
                    self.generation,
                    stage.label()
                );
                self.note(note);
                StopOutcome::Reaped { stage, exit }
            }
            None => StopOutcome::NotRunning,
        };
        match after {
            AfterStop::Restart if !ctx.shutting_down() && self.circuit.is_tripped() => {
                self.open_circuit(now, ctx);
            }
            AfterStop::Restart if !ctx.shutting_down() => self.launch(now, false, ctx),
            _ => {
                self.phase = Phase::Retired;
                self.set_ready(ctx, false);
                self.publish(ctx);
            }
        }
    }

    fn quarantine(&mut self, now: u64, ctx: &mut LoopContext) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        let pid = connection.pid();
        // Dropping the pipes avoids leaking descriptors; the control stays so
        // a late exit can still be reaped (but the backend is not respawned).
        self.quarantined = Some(connection.into_control());
        self.quarantined_pid = pid;
        self.phase = Phase::Quarantined;
        self.outcome = StopOutcome::Unreaped { pid };
        self.next_quarantine_check_ns = now.saturating_add(QUARANTINE_RECHECK_NS);
        let message = format!(
            "generation {} (pid {}) not reaped {} ms after SIGKILL; quarantined (requires intervention)",
            self.generation,
            pid.map_or_else(|| "thread".to_owned(), |pid| pid.to_string()),
            ctx.config.child_reap_ms
        );
        ctx.log(LogLevel::Error, &format!("{}: {message}", self.adapter_id));
        self.note(message);
        self.set_ready(ctx, false);
        self.publish(ctx);
    }

    fn recheck_quarantine(&mut self, now: u64, ctx: &mut LoopContext) {
        self.next_quarantine_check_ns = now.saturating_add(QUARANTINE_RECHECK_NS);
        let Some(control) = self.quarantined.as_mut() else {
            return;
        };
        let Ok(Some(exit)) = control.try_reap() else {
            return;
        };
        self.quarantined = None;
        self.quarantined_pid = None;
        self.outcome = StopOutcome::Reaped {
            stage: StopStage::Kill,
            exit: exit.clone(),
        };
        self.note(format!(
            "quarantined generation {} was reaped late ({exit}); backend stays quarantined",
            self.generation
        ));
        self.publish(ctx);
    }

    /// Supervisor shutdown: stop whatever runs, never relaunch.
    pub(crate) fn begin_shutdown(&mut self, now: u64, ctx: &mut LoopContext) {
        let durations = ctx.stop_durations(now);
        match &mut self.phase {
            Phase::NotStarted | Phase::CircuitOpen { .. } => {
                self.phase = Phase::Retired;
                self.outcome = StopOutcome::NotRunning;
                self.set_ready(ctx, false);
                self.publish(ctx);
            }
            Phase::Retired | Phase::Quarantined => {}
            Phase::Stopping(stop) => {
                stop.after = AfterStop::Retire;
                let shortened = now.saturating_add(durations.for_stage(stop.stage));
                stop.stage_deadline_boot_ns = stop.stage_deadline_boot_ns.min(shortened);
            }
            Phase::AwaitHello { .. } | Phase::Idle | Phase::Busy(_) => {
                self.begin_stop(now, AfterStop::Retire, ctx);
            }
        }
    }

    pub(crate) fn is_settled(&self) -> bool {
        matches!(
            self.phase,
            Phase::NotStarted | Phase::Retired | Phase::Quarantined
        )
    }

    /// Final outcome for the shutdown report; best-effort SIGKILL for a host
    /// whose stop sequence ran out of time.
    pub(crate) fn shutdown_outcome(&mut self) -> HostShutdown {
        let outcome = if self.is_settled() {
            self.outcome.clone()
        } else {
            let pid = self.connection.as_ref().and_then(Connection::pid);
            if let Some(connection) = self.connection.as_mut() {
                let _ = connection.control().kill();
            }
            StopOutcome::Unreaped { pid }
        };
        HostShutdown {
            adapter_id: self.adapter_id.clone(),
            generation: self.generation,
            outcome,
        }
    }

    // ----- gates and status ------------------------------------------------

    fn become_idle(&mut self, now: u64, success: bool, ctx: &mut LoopContext) {
        self.phase = Phase::Idle;
        if success {
            self.last_success_boot_ns = Some(now);
            self.circuit.record_success();
            self.detail = None;
        }
        self.set_ready(ctx, true);
        self.release_claim(ctx);
    }

    fn set_ready(&self, ctx: &LoopContext, ready: bool) {
        let mut state = ctx.shared.lock();
        if let Some(gate) = state.gates.get_mut(self.index) {
            gate.ready = ready;
            gate.generation = self.generation.max(1);
        }
    }

    fn release_claim(&self, ctx: &LoopContext) {
        let mut state = ctx.shared.lock();
        if let Some(gate) = state.gates.get_mut(self.index) {
            gate.claimed = false;
        }
    }

    /// Prepends `message` to the status detail (newest first, bounded), so
    /// a restart keeps the reason and the stop outcome visible.
    fn note(&mut self, message: String) {
        let combined = match self.detail.take() {
            Some(previous) => format!("{message}; {previous}"),
            None => message,
        };
        self.detail = Some(dgxtop_core::text::sanitize_display(
            &combined,
            MAX_DETAIL_BYTES,
        ));
    }

    fn host_state(&self) -> HostState {
        let failing = self.circuit.consecutive_failures() > 0;
        match &self.phase {
            Phase::NotStarted
            | Phase::AwaitHello {
                probe_after: false, ..
            } => HostState::Starting,
            Phase::AwaitHello {
                probe_after: true, ..
            } => HostState::CircuitOpen,
            Phase::Busy(inflight) if matches!(inflight.kind, InFlightKind::Probe { .. }) => {
                HostState::CircuitOpen
            }
            Phase::Idle | Phase::Busy(_) if failing => HostState::Degraded,
            Phase::Idle | Phase::Busy(_) => HostState::Healthy,
            Phase::Stopping(stop) if stop.after == AfterStop::Retire => HostState::Unavailable,
            Phase::Stopping(_) if self.circuit.is_tripped() => HostState::CircuitOpen,
            Phase::Stopping(_) => HostState::Degraded,
            Phase::CircuitOpen { .. } => HostState::CircuitOpen,
            Phase::Quarantined => HostState::Quarantined,
            Phase::Retired => HostState::Unavailable,
        }
    }

    fn pid(&self) -> Option<i32> {
        self.connection
            .as_ref()
            .and_then(Connection::pid)
            .or(self.quarantined_pid)
    }

    pub(crate) fn status(&self) -> HostStatus {
        HostStatus {
            adapter_id: self.adapter_id.to_string(),
            state: self.host_state(),
            generation: self.generation.max(1),
            pid: self.pid(),
            last_success_boot_ns: self.last_success_boot_ns,
            timeouts_total: self.timeouts_total,
            consecutive_failures: self.circuit.consecutive_failures(),
            restarts_total: self.restarts_total,
            backoff_until_boot_ns: match self.phase {
                Phase::CircuitOpen { until_boot_ns } => Some(until_boot_ns),
                _ => None,
            },
            detail: self.detail.clone(),
        }
    }

    /// Updates the shared view and emits a `HostStatus` completion when
    /// [`status_emission::should_emit`] says the Manager needs it.
    fn publish(&mut self, ctx: &mut LoopContext) {
        let status = self.status();
        {
            let mut state = ctx.shared.lock();
            if let Some(slot) = state.statuses.get_mut(self.index) {
                *slot = status.clone();
            }
            if let Some(pid) = state.pids.get_mut(self.index) {
                *pid = status.pid;
            }
        }
        if status_emission::should_emit(self.last_published.as_ref(), &status) {
            self.last_published = Some(status.clone());
            ctx.emit(IngestionCompletion::HostStatus(status));
        }
    }

    /// Keeps the shared success timestamp fresh without emitting.
    pub(crate) fn refresh_shared_status(&self, ctx: &LoopContext) {
        let status = self.status();
        let mut state = ctx.shared.lock();
        if let Some(slot) = state.statuses.get_mut(self.index) {
            *slot = status;
        }
    }

    /// Adds this slot's descriptors to the poll set.
    pub(crate) fn register<'a>(
        &'a self,
        index: usize,
        descriptors: &mut Vec<PollFd<'a>>,
        roles: &mut Vec<(usize, PipeRole)>,
    ) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        if connection.wants_read()
            && let Some(fd) = connection.replies_fd()
        {
            descriptors.push(PollFd::new(fd, PollFlags::POLLIN));
            roles.push((index, PipeRole::Replies));
        }
        if let Some(fd) = connection.diagnostics_fd() {
            descriptors.push(PollFd::new(fd, PollFlags::POLLIN));
            roles.push((index, PipeRole::Diagnostics));
        }
        if connection.wants_write()
            && let Some(fd) = connection.requests_fd()
        {
            descriptors.push(PollFd::new(fd, PollFlags::POLLOUT));
            roles.push((index, PipeRole::Requests));
        }
    }
}

/// Borrowed pieces of a completed read.
struct ReadReply<'a> {
    plan: &'a ReadPlan,
    key: &'a BrokerKey,
    credit: Vec<QueueTicket>,
}

fn check_isolation(hello: &HostHello, launch: &HostLaunch) -> Result<(), IpcError> {
    let expected = if launch.is_in_process() {
        Isolation::InProcessReplay
    } else {
        Isolation::PersistentProcess
    };
    if hello.manifest.isolation == expected {
        Ok(())
    } else {
        Err(IpcError::HelloMismatch {
            field: "manifest.isolation",
        })
    }
}

fn decode_validated_batch(
    payload: &[u8],
    plan: &ReadPlan,
    ctx: &LoopContext,
) -> Result<ObservationBatch, IpcError> {
    let batch = decode_batch(payload, &ctx.limits)?;
    batch
        .validate_against(plan, ctx.limits.max_records())
        .map_err(|error| IpcError::contract(&error.to_string()))?;
    Ok(batch)
}

fn decode_validated_catalog(
    payload: &[u8],
    request_id: Uuid,
    generation: u32,
    ctx: &LoopContext,
) -> Result<CatalogReply, IpcError> {
    let reply = decode_catalog(payload, &ctx.limits)?;
    if reply.request_id != request_id {
        return Err(IpcError::contract(
            "catalog request_id differs from the request",
        ));
    }
    if reply.generation != generation {
        return Err(IpcError::contract(
            "catalog generation differs from the host",
        ));
    }
    Ok(reply)
}

/// Converts the request's tickets into one permit sized to the decoded
/// estimate (the wire buffer is already gone).
fn account(
    estimate: u64,
    credit: Vec<QueueTicket>,
    ctx: &LoopContext,
) -> Result<BytePermit, String> {
    let mut permits = credit.into_iter().map(QueueTicket::into_permit);
    let Some(mut permit) = permits.next() else {
        return ctx
            .ingress
            .try_admit(estimate)
            .map(QueueTicket::into_permit)
            .map_err(|error| error.to_string());
    };
    for extra in permits {
        // Same gate, same ledger: merging cannot fail; a mismatch would just
        // release the extra bytes.
        let _ = permit.merge(extra);
    }
    if estimate <= permit.bytes() {
        permit.shrink_to(estimate);
        return Ok(permit);
    }
    permit
        .try_grow(estimate - permit.bytes())
        .map(|()| permit)
        .map_err(|error| error.to_string())
}

/// Delivers a read outcome to every broker requester (dispatcher first).
fn complete_read(key: &BrokerKey, plan: &ReadPlan, result: BrokerResult, ctx: &mut LoopContext) {
    let mut requesters = ctx.shared.lock().broker.complete(key, result.clone());
    if requesters.is_empty() {
        requesters.push(plan.request_id);
    }
    for request_id in requesters {
        ctx.emit(IngestionCompletion::Batch {
            adapter_id: plan.adapter_id.clone(),
            request_id,
            host_generation: plan.host_generation,
            result: result.clone(),
        });
    }
}

/// Fails a job that never reached its host.
fn fail_execution(execution: &Execution, reason: AdapterFailure, ctx: &mut LoopContext) {
    if let (AdapterJob::Read(plan), Some(key)) = (&execution.job, &execution.broker_key) {
        complete_read(key, plan, Err(reason), ctx);
        return;
    }
    ctx.emit(failed_completion(
        &execution.job,
        execution.request_id(),
        reason,
    ));
}
