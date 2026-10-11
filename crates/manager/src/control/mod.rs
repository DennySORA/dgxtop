//! The Manager control core: every service and the plumbing between them,
//! driven by one thread (RUNTIME.md §4).
//!
//! Handlers here only route typed inputs to the owning service and apply the
//! returned effects (results, gaps, dirty sessions) in one place.
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
use dgxtop_core::commands::CommandResult;
use dgxtop_core::identity::{BootId, HostId, SessionId};
use dgxtop_core::naming::{self, adapters, groups};
use dgxtop_core::ports::{LogLevel, Logger, TrendReader};
use uuid::Uuid;

use crate::archive::ArchiveTracker;
use crate::catalog::DeviceCatalog;
use crate::config::ConfigurationService;
use crate::effects::Effects;
use crate::error::ManagerError;
use crate::history::{HistoryService, HistorySettings};
use crate::hosts::HostTable;
use crate::ingestion::{IngestionPorts, IngestionService, IngestionSettings, collection_limits};
use crate::overload::OverloadController;
use crate::pacing::FramePacing;
use crate::policy::QualityPolicyService;
use crate::presentation::HostFacts;
use crate::process::{ActionIdentity, Now, ProcessPorts, ProcessService};
use crate::process_link::ProcessHistoryLink;
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
/// Safety timeout of a collector job: this many fast periods (≥ 10 s).
const JOB_TIMEOUT_PERIODS: u64 = 4;

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
    host_id: HostId,
    boot_id: BootId,
    facts: HostFacts,
    hosts: HostTable,
    catalog: DeviceCatalog,
    policy: QualityPolicyService,
    scheduler: SamplingScheduler,
    ingestion: IngestionService,
    /// The state's short trends (D-99): read, never recorded here.
    trends: Arc<dyn TrendReader>,
    /// How often the process table is read (its memory values hold a few
    /// such periods, D-64).
    process_period_ms: u64,
    process_history: ProcessHistoryLink,
    history: HistoryService,
    config: ConfigurationService,
    process: ProcessService,
    sessions: ViewSessionService,
    presentation: PresentationService,
    archive: ArchiveTracker,
    overload: OverloadController,
    results: ResultCache,
    /// The session each accepted command's single result goes to.
    routes: BTreeMap<Uuid, SessionId>,
    inbox: Arc<CommandInbox>,
    shutdown: Arc<ShutdownSignal>,
    ingress: QueueGate,
    ledger: ByteLedger,
    timers: Timers,
    phase: Phase,
    started: bool,
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
        let history = HistoryService::new(
            ports.history,
            Arc::clone(&ports.clock),
            HistorySettings {
                max_inflight: usize::try_from(effective.queues.history_requests).unwrap_or(1),
                deadline_ms: effective.runtime.history_deadline_ms,
                max_points: effective.store.max_history_points,
                utc_offset_s: setup.facts.utc_offset_s,
                kept_on_disk: effective.store.archive_enabled,
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
        );
        let process_history = ProcessHistoryLink::new(ports.process_history);
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
            setup.config_file,
        );
        Self {
            hosts: HostTable::new(&setup.adapters),
            trends: ports.trends,
            process_period_ms: process_ms,
            process_history,
            sessions: ViewSessionService::new(effective.viewer.max_sessions),
            presentation: PresentationService::new(
                ports.ledger.clone(),
                effective.runtime.max_projection_bytes,
            ),
            archive: ArchiveTracker::default(),
            overload: OverloadController::new(&effective.runtime, now),
            results: ResultCache::new(
                usize::try_from(effective.runtime.result_cache_entries).unwrap_or(128),
                ms_to_ns(u64::from(effective.runtime.result_cache_ttl_ms)),
            ),
            slice: Duration::from_millis(u64::from(effective.runtime.control_slice_ms)),
            clock: ports.clock,
            logger: ports.logger,
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
        for result in fx.results {
            self.finish(result);
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
        let Some(session_id) = self.routes.remove(&request_id) else {
            self.logger.log(
                LogLevel::Warn,
                "manager.commands",
                &format!("result for unknown request {request_id} dropped"),
            );
            return;
        };
        let now = self.clock.now_boot_ns();
        self.results.complete(request_id, &result, now);
        self.deliver(&session_id, result);
    }

    /// Hands a result to a session mailbox (or releases its slot when the
    /// session is already gone, so the outstanding bound never leaks).
    fn deliver(&self, session_id: &SessionId, result: CommandResult) {
        match self.sessions.shared(session_id) {
            Some(shared) => shared.deliver(result),
            None => self.inbox.release_orphan_slot(),
        }
    }

    /// Records a visible gap in the store.
    fn record_gap(&mut self, gap: GapRecord) {
        if self.ingestion.record_gap(gap).is_err() {
            self.logger.log(
                LogLevel::Warn,
                "manager.gaps",
                "gap not recorded: the store queue is full",
            );
        }
    }

    /// Re-publishes the policy after period or quality changes; new periods
    /// also set how long a process table's values hold in the process
    /// memory record (each table hands its period to the history).
    fn refresh_policy(&mut self) {
        let periods = self.scheduler.effective_periods();
        if let Some(process_ms) = process_list_period_ms(&periods) {
            self.process_period_ms = process_ms;
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

#[cfg(test)]
mod tests;
