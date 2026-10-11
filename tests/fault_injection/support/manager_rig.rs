//! A real `Manager` on its own control thread, driven only through its
//! public surface: the `SessionHandle` (commands, results, projections), the
//! shutdown trigger and the event feed. Ports default to inert capturing
//! fakes; each scenario swaps in the real workers it exercises.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dgxtop_core::action::{ActionOp, AuditOp};
use dgxtop_core::budget::{ByteLedger, Partition, QueueGate};
use dgxtop_core::canonical::{CollectionPlan, ImmutableSnapshot, ProcessRow, ProcessTable};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::commands::{
    CommandBody, CommandEnvelope, CommandRejection, CommandResult, SortKey, ViewKind,
};
use dgxtop_core::config::Config;
use dgxtop_core::diagnostics::Diagnostics;
use dgxtop_core::event::EventKind;
use dgxtop_core::identity::{BootId, HostId, Pid, PidNamespace, ProcessKey};
use dgxtop_core::observation::AdapterManifest;
use dgxtop_core::ports::{
    AdapterJob, ConfigJob, HistoryJob, LoadOrigin, SessionPort, SnapshotReader, StoreJob,
};
use dgxtop_core::quality::{Namespace, SampleKind};
use dgxtop_core::testing::{CapturingDispatch, MemoryLogger};
use dgxtop_core::view::ViewModel;
use dgxtop_manager::presentation::HostFacts;
use dgxtop_manager::{
    EventFeed, Manager, ManagerChannels, ManagerPorts, ManagerSetup, SessionHandle,
    ShutdownSummary, ShutdownTrigger,
};
use uuid::Uuid;

use super::probes::wait_until;

/// Effective uid the Manager believes it runs as (the action port is fake,
/// so this never has to match the real uid).
pub const TEST_EUID: u32 = 1000;
const PID_NAMESPACE: &str = "pid:[4026531836]";
const POLL_STEP: Duration = Duration::from_millis(5);
/// Bound for one command's result in real time.
pub const RESULT_WAIT: Duration = Duration::from_secs(5);

/// Who this dgxtop instance is (the caller context of process actions).
#[derive(Debug, Clone)]
pub struct Identity {
    pub host_id: HostId,
    pub boot_id: BootId,
    pub pid_namespace: PidNamespace,
}

impl Identity {
    /// Identity on `host_id` in the clock's boot.
    pub fn new(host_id: HostId, clock: &dyn TimeProvider) -> Self {
        Self {
            host_id,
            boot_id: clock.boot_id(),
            pid_namespace: PidNamespace::new(PID_NAMESPACE).expect("pid namespace"),
        }
    }

    pub fn fault_host(clock: &dyn TimeProvider) -> Self {
        Self::new(HostId::new("fault-host").expect("host id"), clock)
    }

    /// A user process of this host/boot/namespace (never a real process:
    /// the recording action port only pretends to signal it).
    pub fn process(&self, pid: i64) -> ProcessKey {
        ProcessKey {
            host_id: self.host_id.clone(),
            boot_id: self.boot_id.clone(),
            pid_namespace: self.pid_namespace.clone(),
            pid: Pid::new(pid).expect("pid"),
            start_time_ticks: 777,
        }
    }
}

pub fn manager_setup(
    config: &Config,
    identity: &Identity,
    adapters: Vec<AdapterManifest>,
    namespace: Namespace,
) -> ManagerSetup {
    ManagerSetup {
        saved_config: config.clone(),
        effective_config: config.clone(),
        value_origins: dgxtop::startup::value_origins(config, &LoadOrigin::Defaults, &[]),
        host_id: identity.host_id.clone(),
        boot_id: identity.boot_id.clone(),
        pid_namespace: identity.pid_namespace.clone(),
        euid: TEST_EUID,
        self_pid: super::probes::own_pid(),
        adapters,
        facts: HostFacts {
            host_name: identity.host_id.to_string(),
            version: dgxtop_core::VERSION.to_owned(),
            ..HostFacts::default()
        },
        namespace,
    }
}

/// Ingress gate sized like the composition root's.
pub fn ingress_gate(config: &Config, ledger: &ByteLedger) -> QueueGate {
    QueueGate::new(
        "ingress",
        usize::try_from(config.queues.collection_batches).unwrap_or(32),
        u64::from(config.queues.ingress_bytes),
        ledger.clone(),
        Partition::Ingress,
    )
}

/// Ports where nothing happens: no adapters, collectors, store, history,
/// config, audit or action traffic reaches a real worker.
pub fn inert_ports(
    clock: Arc<dyn TimeProvider>,
    snapshots: Arc<dyn SnapshotReader>,
    ledger: &ByteLedger,
    ingress: QueueGate,
) -> ManagerPorts {
    ManagerPorts {
        clock,
        adapters: Box::new(CapturingDispatch::<AdapterJob>::new()),
        collectors: Box::new(CapturingDispatch::<CollectionPlan>::new()),
        collector_registry: Vec::new(),
        store: Box::new(CapturingDispatch::<StoreJob>::new()),
        snapshots,
        history: Box::new(CapturingDispatch::<HistoryJob>::new()),
        config: Box::new(CapturingDispatch::<ConfigJob>::new()),
        audit: Box::new(CapturingDispatch::<AuditOp>::new()),
        actions: Box::new(CapturingDispatch::<ActionOp>::new()),
        process_history: None,
        logger: Arc::new(MemoryLogger::default()),
        diagnostics: Diagnostics::new(),
        ledger: ledger.clone(),
        ingress,
    }
}

/// Snapshot reader holding a fixed process table. It can be told to fail
/// its next read, which happens on the Manager's control thread: the way a
/// black-box test provokes a Manager invariant failure.
#[derive(Debug)]
pub struct ProcessTableSnapshots {
    current: Mutex<Arc<ImmutableSnapshot>>,
    fail_next_read: AtomicBool,
}

impl ProcessTableSnapshots {
    pub fn with_processes(rows: Vec<ProcessRow>) -> Arc<Self> {
        let snapshot = ImmutableSnapshot {
            revision: 1,
            process_table: Arc::new(ProcessTable {
                rows,
                ..ProcessTable::default()
            }),
            ..ImmutableSnapshot::default()
        };
        Arc::new(Self {
            current: Mutex::new(Arc::new(snapshot)),
            fail_next_read: AtomicBool::new(false),
        })
    }

    pub fn inject_invariant_failure(&self) {
        self.fail_next_read.store(true, Ordering::SeqCst);
    }
}

impl SnapshotReader for ProcessTableSnapshots {
    fn latest(&self) -> Arc<ImmutableSnapshot> {
        if self.fail_next_read.swap(false, Ordering::SeqCst) {
            panic!("injected Manager invariant failure: snapshot state is inconsistent");
        }
        Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// A process row owned by [`TEST_EUID`].
pub fn process_row(key: ProcessKey) -> ProcessRow {
    ProcessRow {
        key,
        name: "train.py".to_owned(),
        command: String::new(),
        uid: Some(TEST_EUID),
        state: Some('S'),
        descendants: None,
        cpu_percent: Some(1.0),
        rss_bytes: Some(1 << 20),
        missing_reason: None,
        gpu: Vec::new(),
        sample_kind: SampleKind::Measured,
        read_finished_boot_ns: 0,
    }
}

pub fn navigation(view: ViewKind) -> CommandBody {
    CommandBody::UpdateViewSession {
        view,
        selected_entity_id: None,
        selected_process: None,
        filter: String::new(),
        sort: SortKey::Pid,
        ascending: true,
        frame_revision: 0,
        overview_bar: None,
        process_bar: None,
    }
}

/// The Manager running `Manager::run` on its own thread.
pub struct RunningManager {
    pub session: SessionHandle,
    pub trigger: ShutdownTrigger,
    pub events: EventFeed,
    thread: Option<JoinHandle<ShutdownSummary>>,
    results: Mutex<Vec<CommandResult>>,
}

impl RunningManager {
    pub fn start(setup: ManagerSetup, ports: ManagerPorts, channels: ManagerChannels) -> Self {
        let mut manager =
            Manager::with_channels(setup, ports, channels).expect("valid manager setup");
        let session = manager.open_session().expect("open a view session");
        let trigger = manager.shutdown_trigger();
        let events = manager.event_feed();
        let thread = std::thread::Builder::new()
            .name("fault-manager".to_owned())
            .spawn(move || manager.run())
            .expect("spawn the manager thread");
        Self {
            session,
            trigger,
            events,
            thread: Some(thread),
            results: Mutex::new(Vec::new()),
        }
    }

    pub fn envelope(&self, body: CommandBody) -> CommandEnvelope {
        CommandEnvelope::new(self.session.session_id().clone(), body)
    }

    pub fn try_submit(&self, body: CommandBody) -> Result<Uuid, CommandRejection> {
        let envelope = self.envelope(body);
        let request_id = envelope.request_id;
        self.session.submit(envelope).map(|()| request_id)
    }

    pub fn submit(&self, body: CommandBody) -> Uuid {
        self.try_submit(body)
            .unwrap_or_else(|rejection| panic!("command refused at submission: {rejection}"))
    }

    /// Waits for the single result of `request_id`.
    pub fn result_for(&self, request_id: Uuid, timeout: Duration) -> CommandResult {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut book = self.results.lock().unwrap_or_else(PoisonError::into_inner);
                book.extend(self.session.drain_results(usize::MAX));
                if let Some(position) = book
                    .iter()
                    .position(|result| result.request_id == request_id)
                {
                    return book.remove(position);
                }
            }
            assert!(
                Instant::now() < deadline,
                "no result for {request_id} within {timeout:?}"
            );
            std::thread::sleep(POLL_STEP);
        }
    }

    /// Submits and waits for the result.
    pub fn execute(&self, body: CommandBody) -> CommandResult {
        let request_id = self.submit(body);
        self.result_for(request_id, RESULT_WAIT)
    }

    /// Switches the session's view (decides which module views are filled).
    pub fn show(&self, view: ViewKind) {
        let result = self.execute(navigation(view));
        assert!(result.code.is_ok(), "navigation refused: {result:?}");
    }

    /// Newest projection accepted by `accept`, waiting for fresh frames.
    pub fn projection_where(
        &self,
        what: &str,
        timeout: Duration,
        mut accept: impl FnMut(&ViewModel) -> bool,
    ) -> Arc<ViewModel> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(projection) = self.session.take_projection()
                && accept(&projection)
            {
                return projection;
            }
            assert!(
                Instant::now() < deadline,
                "no projection with {what} within {timeout:?}"
            );
            std::thread::sleep(POLL_STEP);
        }
    }

    /// Kinds of the events still held by the bounded event log.
    pub fn event_kinds(&self) -> Vec<EventKind> {
        self.events
            .recent(512)
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    /// Waits (bounded) for the control thread and returns its outcome
    /// (`Err` when it panicked).
    pub fn join_within(&mut self, bound: Duration) -> std::thread::Result<ShutdownSummary> {
        let thread = self
            .thread
            .take()
            .expect("the manager thread is joined once");
        wait_until("the manager thread to end", bound, || thread.is_finished());
        thread.join()
    }

    /// Requests shutdown out of band and returns the summary plus how long
    /// the bounded shutdown took.
    pub fn shut_down(&mut self, reason: &str, bound: Duration) -> (ShutdownSummary, Duration) {
        let started = Instant::now();
        self.trigger.trigger(reason);
        let summary = self
            .join_within(bound)
            .unwrap_or_else(|_| panic!("the manager thread panicked during shutdown"));
        (summary, started.elapsed())
    }
}

impl Drop for RunningManager {
    fn drop(&mut self) {
        // Never leave a control thread behind a failed assertion.
        if let Some(thread) = self.thread.take() {
            self.trigger.trigger("test teardown");
            let deadline = Instant::now() + Duration::from_secs(5);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(POLL_STEP);
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}
