//! The Manager control core: every service and the plumbing between them,
//! driven by one thread (RUNTIME.md §4).
//!
//! Handlers here only route typed inputs to the owning service and apply the
//! returned effects (results, events, gaps, dirty sessions) in one place.
//! Nothing in this module performs I/O: all work leaves through `Dispatch`
//! ports and comes back as completions.

mod commands;
mod completions;
mod shutdown;
mod timers;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dgxtop_core::budget::{ByteLedger, QueueGate};
use dgxtop_core::canonical::GapRecord;
use dgxtop_core::clock::{TimeProvider, ms_to_ns};
use dgxtop_core::commands::{CommandResult, CommandStatus};
use dgxtop_core::diagnostics::{Diagnostics, names};
use dgxtop_core::errors::ResultCode;
use dgxtop_core::event::{CompletionStatus, EventKind};
use dgxtop_core::identity::{BootId, HostId, SessionId};
use dgxtop_core::naming::{self, adapters, groups};
use dgxtop_core::ports::{LogLevel, Logger};
use dgxtop_core::text::sanitize_display;
use uuid::Uuid;

use crate::archive::ArchiveTracker;
use crate::catalog::DeviceCatalog;
use crate::config::ConfigurationService;
use crate::effects::Effects;
use crate::error::ManagerError;
use crate::events::{EVENT_LOG_CAPACITY, EventFeed, EventRecorder};
use crate::history::{ArchiveSide, HistoryService, HistorySettings};
use crate::hosts::HostTable;
use crate::ingestion::trends::TrendRecorder;
use crate::ingestion::{IngestionPorts, IngestionService, IngestionSettings, collection_limits};
use crate::overload::OverloadController;
use crate::pacing::FramePacing;
use crate::policy::QualityPolicyService;
use crate::presentation::{HostFacts, ProcessMemoryBook};
use crate::process::{ActionIdentity, Now, ProcessPorts, ProcessService};
use crate::process_history::{ProcessHistoryPorts, ProcessHistoryService};
use crate::projection::PresentationService;
use crate::results::ResultCache;
use crate::scheduler::SamplingScheduler;
use crate::session::{CommandInbox, SessionHandle, SessionShared};
use crate::setup::{ManagerPorts, ManagerSetup};
use crate::shutdown::{ShutdownSignal, ShutdownTrigger};
use crate::views::ViewSessionService;

/// Total time the shutdown waits for config/audit/action completions.
const SHUTDOWN_WAIT_NS: u64 = 3_000_000_000;
/// Cadence of store maintenance requests (retention, minute rollover).
const MAINTAIN_NS: u64 = 1_000_000_000;
/// Events copied into the shutdown summary.
const SUMMARY_EVENTS: usize = 64;
/// Safety timeout of a collector job: this many fast periods (≥ 10 s).
const JOB_TIMEOUT_PERIODS: u64 = 4;

/// Where the single result of an accepted command goes.
#[derive(Debug, Clone)]
struct Route {
    session_id: SessionId,
    accepted_ns: u64,
    command: &'static str,
    /// Non-navigation commands also leave a `CommandCompleted` event.
    notify: bool,
}

#[derive(Debug)]
struct Timers {
    next_overload_ns: u64,
    next_maintain_ns: u64,
}

#[derive(Debug)]
struct Draining {
    reason: String,
    started_ns: u64,
    deadline_ns: u64,
    real_deadline: Instant,
}

#[derive(Debug)]
enum Phase {
    Running,
    Draining(Draining),
    Finished,
}

/// Fill ratios of the Manager's own queues, measured by the loop owner.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct QueueLevels {
    pub(crate) commands: f64,
    pub(crate) ingestion: f64,
}

pub(crate) struct ControlCore {
    clock: Arc<dyn TimeProvider>,
    logger: Arc<dyn Logger>,
    diagnostics: Diagnostics,
    events: EventRecorder,
    host_id: HostId,
    boot_id: BootId,
    facts: HostFacts,
    hosts: HostTable,
    catalog: DeviceCatalog,
    policy: QualityPolicyService,
    scheduler: SamplingScheduler,
    ingestion: IngestionService,
    trends: TrendRecorder,
    process_memory: ProcessMemoryBook,
    process_history: ProcessHistoryService,
    history: HistoryService,
    config: ConfigurationService,
    process: ProcessService,
    sessions: ViewSessionService,
    presentation: PresentationService,
    archive: ArchiveTracker,
    overload: OverloadController,
    results: ResultCache,
    routes: BTreeMap<Uuid, Route>,
    inbox: Arc<CommandInbox>,
    shutdown: Arc<ShutdownSignal>,
    ingress: QueueGate,
    ledger: ByteLedger,
    timers: Timers,
    phase: Phase,
    started: bool,
    revoked_tokens: u32,
    rejected_commands: u32,
    pending_commit_ns: Option<u64>,
    slice: Duration,
}

impl ControlCore {
    pub(crate) fn new(
        setup: ManagerSetup,
        ports: ManagerPorts,
        inbox: Arc<CommandInbox>,
        shutdown: Arc<ShutdownSignal>,
    ) -> Self {
        let now = ports.clock.now_boot_ns();
        let effective = setup.effective_config.clone();
        let scheduler = SamplingScheduler::new(ports.adapters, &setup.adapters, &effective, now);
        let fast_ms = scheduler.effective_fast_interval_ms();
        let periods = scheduler.effective_periods();
        let process_ms = process_list_period_ms(&periods).unwrap_or(u64::from(fast_ms));
        let process_memory = ProcessMemoryBook::new(process_ms);
        let policy = QualityPolicyService::new(&effective, periods, setup.namespace);
        let ingestion = IngestionService::new(
            IngestionPorts {
                collectors: ports.collectors,
                store: ports.store,
                snapshots: ports.snapshots,
                registry: ports.collector_registry,
            },
            IngestionSettings {
                host_id: setup.host_id.clone(),
                boot_id: setup.boot_id.clone(),
                limits: collection_limits(&effective),
                max_pending_per_collector: usize::try_from(effective.queues.collection_batches)
                    .unwrap_or(1),
                job_timeout_ns: ms_to_ns(u64::from(fast_ms)).saturating_mul(JOB_TIMEOUT_PERIODS),
            },
        );
        // The archive answers for the time before this run (D-51).
        let archive_reads = ports.archive_reads.map(|port| ArchiveSide {
            port,
            since_boot_ns: now,
        });
        let history = HistoryService::new(
            ports.history,
            archive_reads,
            Arc::clone(&ports.clock),
            HistorySettings {
                max_inflight: usize::try_from(effective.queues.history_requests).unwrap_or(1),
                deadline_ms: effective.runtime.history_deadline_ms,
                max_points: effective.store.max_history_points,
            },
            fast_ms,
        );
        let process = ProcessService::new(
            ProcessPorts {
                audit: ports.audit,
                actions: ports.actions,
            },
            ActionIdentity {
                host_id: setup.host_id.clone(),
                boot_id: setup.boot_id.clone(),
                pid_namespace: setup.pid_namespace.clone(),
                euid: setup.euid,
                self_pid: setup.self_pid,
            },
            &effective,
            ports.diagnostics.clone(),
        );
        let process_history = ProcessHistoryService::new(
            ProcessHistoryPorts {
                probe: ports.process_probe,
                probe_deriver: ports.probe_deriver,
                store: ports.process_history,
            },
            (setup.boot_id.clone(), setup.facts.utc_offset_s),
            Arc::clone(&ports.logger),
            now,
        );
        let catalog = DeviceCatalog::new(
            setup.host_id.clone(),
            setup.boot_id.clone(),
            effective.store.max_sources,
        );
        let config = ConfigurationService::new(
            ports.config,
            setup.saved_config,
            setup.effective_config,
            setup.value_origins,
        );
        Self {
            events: EventRecorder::new(setup.boot_id.clone(), EVENT_LOG_CAPACITY),
            hosts: HostTable::new(&setup.adapters),
            trends: TrendRecorder::new(u64::from(fast_ms)),
            process_memory,
            process_history,
            sessions: ViewSessionService::new(effective.viewer.max_sessions),
            presentation: PresentationService::new(
                ports.ledger.clone(),
                effective.runtime.max_projection_bytes,
                ports.diagnostics.clone(),
            ),
            archive: ArchiveTracker::new(ports.diagnostics.clone()),
            overload: OverloadController::new(&effective.runtime, now),
            results: ResultCache::new(
                usize::try_from(effective.runtime.result_cache_entries).unwrap_or(128),
                ms_to_ns(u64::from(effective.runtime.result_cache_ttl_ms)),
            ),
            slice: Duration::from_millis(u64::from(effective.runtime.control_slice_ms)),
            clock: ports.clock,
            logger: ports.logger,
            diagnostics: ports.diagnostics,
            host_id: setup.host_id,
            boot_id: setup.boot_id,
            facts: setup.facts,
            catalog,
            policy,
            scheduler,
            ingestion,
            history,
            config,
            process,
            routes: BTreeMap::new(),
            inbox,
            shutdown,
            ingress: ports.ingress,
            ledger: ports.ledger,
            timers: Timers {
                next_overload_ns: now,
                next_maintain_ns: now,
            },
            phase: Phase::Running,
            started: false,
            revoked_tokens: 0,
            rejected_commands: 0,
            pending_commit_ns: None,
        }
    }

    /// Per-iteration CPU budget of the control thread.
    pub(crate) fn slice(&self) -> Duration {
        self.slice
    }

    pub(crate) fn now_boot_ns(&self) -> u64 {
        self.clock.now_boot_ns()
    }

    fn now(&self) -> Now {
        Now {
            boot_ns: self.clock.now_boot_ns(),
            utc_ns: self.clock.now_utc_ns(),
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        matches!(self.phase, Phase::Running)
    }

    pub(crate) fn is_finished(&self) -> bool {
        matches!(self.phase, Phase::Finished)
    }

    /// First-iteration work (audit recovery, initial health requests).
    pub(crate) fn start_once(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        let now = self.now();
        self.process.start(now);
        self.process_history.start();
    }

    pub(crate) fn open_session(&mut self) -> Result<SessionHandle, ManagerError> {
        if !self.is_running() || self.shutdown.is_requested() {
            return Err(ManagerError::ShuttingDown);
        }
        let session_id = SessionId::new(format!("session-{}", Uuid::new_v4().simple()))
            .map_err(|error| ManagerError::InvalidSetup(error.to_string()))?;
        let shared = Arc::new(SessionShared::new(
            session_id.clone(),
            self.inbox.outstanding(),
        ));
        self.sessions.open(Arc::clone(&shared))?;
        self.history.open_session(&session_id);
        Ok(SessionHandle::new(shared, Arc::clone(&self.inbox)))
    }

    pub(crate) fn shutdown_trigger(&self) -> ShutdownTrigger {
        ShutdownTrigger::new(Arc::clone(&self.shutdown))
    }

    /// The control thread is gone (normally or by a panic): no session may
    /// keep waiting for it.
    pub(crate) fn release_sessions(&self) {
        self.shutdown.request("manager stopped");
        self.sessions.mark_shutdown_requested_all();
    }

    pub(crate) fn event_feed(&self) -> EventFeed {
        self.events.feed()
    }

    /// The frame pacing in force: the viewer's frame rate for the current
    /// overload state, and whether a collection cycle is still committing.
    pub(crate) fn frame_pacing(&self) -> FramePacing {
        let fps = self.overload.max_fps(self.config.effective().viewer.fps);
        FramePacing {
            input_interval_ns: crate::projection::projection_interval_ns(
                self.overload.state() == dgxtop_core::view::OverloadState::Normal,
                fps,
            ),
            data_interval_ns: crate::projection::frame_interval_ns(fps),
            cycle_settling: self.ingestion.is_collecting() || self.scheduler.has_read_in_flight(),
        }
    }

    /// Earliest instant at which some timer or deadline needs the loop.
    pub(crate) fn next_deadline(&self) -> Option<u64> {
        let frame = self.sessions.next_frame_ns(&self.frame_pacing());
        let draining = match &self.phase {
            Phase::Draining(draining) => Some(draining.deadline_ns),
            _ => None,
        };
        let running = self.is_running();
        [
            running.then(|| self.scheduler.next_deadline()).flatten(),
            running.then(|| self.ingestion.next_deadline()).flatten(),
            running.then(|| self.history.next_deadline()).flatten(),
            running.then_some(self.timers.next_overload_ns),
            running.then_some(self.timers.next_maintain_ns),
            running.then_some(frame).flatten(),
            self.config.next_deadline(),
            self.process.next_deadline(),
            draining,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Applies the effects a service produced.
    fn apply(&mut self, fx: Effects) {
        let now = self.clock.now_boot_ns();
        for result in fx.results {
            self.finish(result);
        }
        for (correlation_id, kind) in fx.events {
            self.events.emit(now, correlation_id, kind);
        }
        for gap in fx.gaps {
            self.record_gap(gap);
        }
        if fx.dirty_all {
            self.sessions.mark_all_dirty();
        }
    }

    /// Delivers the single result of an accepted command.
    fn finish(&mut self, result: CommandResult) {
        let request_id = result.request_id;
        let Some(route) = self.routes.remove(&request_id) else {
            self.logger.log(
                LogLevel::Warn,
                "manager.commands",
                &format!("result for unknown request {request_id} dropped"),
            );
            return;
        };
        let now = self.clock.now_boot_ns();
        self.results.complete(request_id, &result, now);
        self.diagnostics.observe_us(
            names::MANAGER_COMMAND_ACK_MS,
            route.command,
            now.saturating_sub(route.accepted_ns) / 1_000,
        );
        self.count_rejection(&result);
        if route.notify {
            self.events.emit(
                now,
                request_id,
                EventKind::CommandCompleted {
                    request_id,
                    status: completion_status(result.status),
                    code: result.code.as_str().to_owned(),
                },
            );
        }
        self.deliver(&route.session_id, result);
    }

    /// Hands a result to a session mailbox (or releases its slot when the
    /// session is already gone, so the outstanding bound never leaks).
    fn deliver(&self, session_id: &SessionId, result: CommandResult) {
        match self.sessions.shared(session_id) {
            Some(shared) => shared.deliver(result),
            None => self.inbox.release_orphan_slot(),
        }
    }

    fn count_rejection(&self, result: &CommandResult) {
        if let ResultCode::Error(code) = result.code {
            self.diagnostics
                .increment(names::COMMAND_REJECTED_TOTAL, code.as_str(), 1);
        }
    }

    /// Records a visible gap: store, event and counter.
    fn record_gap(&mut self, gap: GapRecord) {
        let now = self.clock.now_boot_ns();
        let label = gap
            .adapter_id
            .as_ref()
            .map_or_else(|| "none".to_owned(), ToString::to_string);
        self.diagnostics
            .increment(names::GAP_RECORDED_TOTAL, &label, 1);
        self.events.emit(
            now,
            Uuid::new_v4(),
            EventKind::GapRecorded {
                start_boot_ns: gap.start_boot_ns,
                end_boot_ns: gap.end_boot_ns,
                reason: sanitize_display(&gap.reason, 128),
                affected_count: gap.affected_count,
            },
        );
        if self.ingestion.record_gap(gap).is_err() {
            self.diagnostics
                .increment(names::QUEUE_DROPPED_TOTAL, "gap_store_busy", 1);
        }
    }

    /// Re-publishes the policy after period or quality changes; new periods
    /// also set how long a process table's values hold in the process
    /// memory record.
    fn refresh_policy(&mut self) {
        let periods = self.scheduler.effective_periods();
        if let Some(process_ms) = process_list_period_ms(&periods) {
            self.process_memory.set_interval(process_ms);
        }
        self.policy.update(self.config.effective(), periods);
    }
}

/// The period of the procfs process group, which rebuilds the process table.
fn process_list_period_ms(periods: &BTreeMap<String, u32>) -> Option<u64> {
    periods
        .get(&naming::period_key(adapters::PROCFS, groups::PROCESS))
        .map(|period| u64::from(*period))
}

fn completion_status(status: CommandStatus) -> CompletionStatus {
    match status {
        CommandStatus::Rejected => CompletionStatus::Rejected,
        CommandStatus::Unknown => CompletionStatus::Unknown,
        CommandStatus::Completed | CommandStatus::Accepted | CommandStatus::InProgress => {
            CompletionStatus::Completed
        }
    }
}

#[cfg(test)]
mod tests;
