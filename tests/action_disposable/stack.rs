//! The process-action stack of `dgxtop`, assembled the way the composition
//! root (`crates/dgxtop/src/monitor/`) assembles it: the real Manager on its
//! own control thread, the real SQLite audit repository in its single-writer
//! actor, the real pidfd action adapter in its actor and the real file config
//! repository in its actor, all talking through the Manager's bounded
//! inboxes; the test drives a `SessionHandle` exactly like a frontend.
//!
//! Telemetry (adapter hosts, collectors, state, history) is replaced by
//! inert ports: the process table the Manager validates targets against is
//! published directly from procfs facts of the disposable children. The
//! workers run the production `dgxtop::control_ops` functions on the
//! program's queue sizes (`dgxtop::worker_queues`); only the completion
//! forwarding of the monitor is replicated at the bottom of this file.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use dgxtop::audit_unavailable::UnavailableAudit;
use dgxtop::control_ops::{run_action_op, run_audit_op, run_config_job};
use dgxtop::worker_queues::{ACTION_QUEUE, AUDIT_QUEUE, CONFIG_QUEUE};
use dgxtop_adapters::process_action::{
    HostProcessFiles, LinuxPidfd, PidfdActionAdapter, PidfdSyscalls,
};
use dgxtop_core::action::{
    ActionOp, ActionState, AuditOp, ConfirmationToken, PersistenceState, Signal,
};
use dgxtop_core::budget::{ByteLedger, MIB, Partition, QueueGate};
use dgxtop_core::canonical::{ImmutableSnapshot, ProcessRow};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::commands::{
    CommandBody, CommandEnvelope, CommandResult, CommandStatus, SortKey, ViewKind,
};
use dgxtop_core::config::Config;
use dgxtop_core::identity::{BootId, HostId, Pid, PidNamespace, ProcessKey};
use dgxtop_core::ports::{
    AuditRepository, ConfigJob, ControlCompletion, SessionPort, SnapshotReader,
};
use dgxtop_core::quality::{Namespace, SampleKind};
use dgxtop_core::view::{ActionsView, ViewModel};
use dgxtop_manager::presentation::HostFacts;
use dgxtop_manager::testing::inert_ports;
use dgxtop_manager::{
    Manager, ManagerChannels, ManagerPorts, ManagerSetup, SessionHandle, ShutdownSummary,
    ShutdownTrigger,
};
use dgxtop_runtime::clock::{current_euid, host_identity, pid_namespace_of};
use dgxtop_runtime::queues::{PoolHandle, spawn_actor};
use dgxtop_store::audit::{AuditLimits, SqliteAuditRepository};
use dgxtop_store::config::FileConfigRepository;
use dgxtop_store::fs::RealFileSystem;
use dgxtop_store::sqlite::FreeSpaceProbe;
use uuid::Uuid;

use crate::audit_trail::AuditTrail;
use crate::child::{DisposableChild, ProcessFacts, pid_namespace};
use crate::clock::TestClock;
use crate::journal::{Journal, Steps, WorkCounter};
use crate::spies::{
    ActionPortSpy, AuditFaults, AuditSpy, DiskSpace, KernelWithoutPidfd, RecordingDispatch,
    SyscallSpy,
};

/// Bound of every wait for a command result.
pub const RESULT_WAIT: Duration = Duration::from_secs(5);
/// Bound of every wait for a projected actions view.
pub const VIEW_WAIT: Duration = Duration::from_secs(5);
/// Bound of every wait for the audit store's side of an action.
pub const AUDIT_WAIT: Duration = Duration::from_secs(5);
/// Bound of every wait for a signalled child to exit.
pub const EXIT_WAIT: Duration = Duration::from_secs(5);
/// Same bound as the composition root's completion forwarding.
const COMPLETION_SEND_TIMEOUT: Duration = Duration::from_secs(1);
/// The Manager's own shutdown is bounded by 3 s.
const MANAGER_STOP_WAIT: Duration = Duration::from_secs(6);
const WORKER_STOP_WAIT: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(5);

/// Private state directory (audit database, config file) shared by
/// consecutive stacks of one test; removed when the last user drops it.
#[derive(Debug)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    pub fn create(label: &str) -> Arc<Self> {
        let root = std::env::temp_dir().join(format!(
            "dgxtop-action-{label}-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&root).expect("create the private state directory");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("make the state directory private");
        Arc::new(Self { root })
    }

    pub fn audit_database(&self) -> PathBuf {
        self.root.join("audit.sqlite3")
    }

    fn config_file(&self) -> PathBuf {
        self.root.join("config").join("config.json")
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Which audit repository the audit worker owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuditBackend {
    /// `SqliteAuditRepository` (WAL, `synchronous=FULL`) in the state dir.
    #[default]
    Sqlite,
    /// The fail-closed stand-in the composition root uses when the SQLite
    /// runtime gate fails.
    Unavailable,
}

/// The kernel the pidfd adapter talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kernel {
    #[default]
    Pidfd,
    WithoutPidfd,
}

#[derive(Debug, Clone, Default)]
pub struct StackOptions {
    /// Caller euid handed to the Manager; `None` is the real euid of this
    /// test process (the composition root passes `current_euid()`).
    pub caller_euid: Option<u32>,
    pub read_only: bool,
    pub actions_disabled: bool,
    pub audit: AuditBackend,
    pub kernel: Kernel,
    /// Also open a second, independent view session.
    pub bystander_session: bool,
}

impl StackOptions {
    /// The shipped defaults with actions read-only or disabled as asked.
    fn config(&self) -> Config {
        let mut config = Config::defaults().expect("embedded defaults");
        config.actions.read_only = self.read_only;
        config.actions.enabled = !self.actions_disabled;
        config
    }
}

/// Fault switches a test may flip while the stack runs.
#[derive(Debug, Default)]
pub struct Faults {
    pub audit: Arc<AuditFaults>,
    pub disk: Arc<DiskSpace>,
}

/// A confirmation the Manager issued.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// Request id of the `PrepareSignal`.
    pub request_id: Uuid,
    pub token: ConfirmationToken,
    pub expires_boot_ns: u64,
}

/// A frontend's use of one view session.
pub struct SessionDriver {
    handle: SessionHandle,
    results: Mutex<Vec<CommandResult>>,
}

impl SessionDriver {
    fn new(handle: SessionHandle) -> Self {
        Self {
            handle,
            results: Mutex::new(Vec::new()),
        }
    }

    /// Another handle on the same session (for submitting from threads).
    pub fn handle(&self) -> SessionHandle {
        self.handle.clone()
    }

    pub fn envelope(&self, body: CommandBody) -> CommandEnvelope {
        CommandEnvelope::new(self.handle.session_id().clone(), body)
    }

    pub fn submit(&self, body: CommandBody) -> Uuid {
        let envelope = self.envelope(body);
        self.submit_envelope(&envelope);
        envelope.request_id
    }

    pub fn submit_envelope(&self, envelope: &CommandEnvelope) {
        self.handle
            .submit(envelope.clone())
            .unwrap_or_else(|rejection| panic!("command refused at submission: {rejection}"));
    }

    /// The first result delivered for `request_id`.
    pub fn result(&self, request_id: Uuid) -> CommandResult {
        self.nth_result(request_id, 0)
    }

    /// The `index`-th result delivered for `request_id` (replays of one
    /// request id get one result per submission).
    pub fn nth_result(&self, request_id: Uuid, index: usize) -> CommandResult {
        let deadline = Instant::now() + RESULT_WAIT;
        loop {
            {
                let mut results = self.results.lock().unwrap_or_else(PoisonError::into_inner);
                results.extend(self.handle.drain_results(usize::MAX));
                if let Some(found) = results
                    .iter()
                    .filter(|result| result.request_id == request_id)
                    .nth(index)
                {
                    return found.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "no result #{index} for request {request_id} within {RESULT_WAIT:?}"
            );
            thread::sleep(POLL);
        }
    }

    /// Waits for a projected actions view accepted by `accept`.
    pub fn await_actions(&self, what: &str, accept: impl Fn(&ActionsView) -> bool) -> ActionsView {
        let deadline = Instant::now() + VIEW_WAIT;
        let mut last_seen = None;
        loop {
            if let Some(view) = self
                .handle
                .take_projection()
                .and_then(|model| model.modules.actions.as_deref().cloned())
            {
                if accept(&view) {
                    return view;
                }
                last_seen = Some(view);
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; last actions view: {last_seen:#?}"
            );
            thread::sleep(POLL);
        }
    }

    fn await_projection(&self) -> Arc<ViewModel> {
        let deadline = Instant::now() + VIEW_WAIT;
        loop {
            if let Some(model) = self.handle.take_projection() {
                return model;
            }
            assert!(
                Instant::now() < deadline,
                "no projection within {VIEW_WAIT:?}"
            );
            thread::sleep(POLL);
        }
    }

    /// Navigates to the Overview, the view of the process actions (every
    /// projection carries their state, D-91).
    fn show_actions_view(&self) {
        let frame_revision = self.await_projection().frame_revision;
        let request_id = self.submit(CommandBody::UpdateViewSession {
            view: ViewKind::Overview,
            selected_entity_id: None,
            selected_process: None,
            filter: String::new(),
            sort: SortKey::Pid,
            ascending: true,
            frame_revision,
            overview_bar: None,
            process_bar: None,
        });
        let result = self.result(request_id);
        assert_eq!(
            result.status,
            CommandStatus::Completed,
            "navigate to the actions view: {result:?}"
        );
    }
}

/// The identity of this dgxtop instance, as the composition root builds it.
#[derive(Debug, Clone)]
struct Identity {
    host_id: HostId,
    boot_id: BootId,
    pid_namespace: PidNamespace,
    euid: u32,
    self_pid: i32,
}

impl Identity {
    fn of_this_process(clock: &TestClock, caller_euid: Option<u32>) -> Self {
        let self_pid = i32::try_from(std::process::id()).expect("pid fits in i32");
        Self {
            host_id: host_identity(Path::new("/")).expect("host identity"),
            boot_id: clock.boot_id(),
            pid_namespace: pid_namespace_of(self_pid).expect("own pid namespace"),
            euid: caller_euid.unwrap_or_else(current_euid),
            self_pid,
        }
    }
}

/// The real workers behind the Manager's audit, action and config ports.
struct Workers {
    audit: PoolHandle<AuditOp>,
    audit_work: Arc<WorkCounter>,
    action: PoolHandle<ActionOp>,
    action_work: Arc<WorkCounter>,
    config: PoolHandle<ConfigJob>,
    control_inbox: Sender<ControlCompletion>,
}

/// Everything the workers are built from.
struct WorkerPlan<'a> {
    options: &'a StackOptions,
    state: &'a StateDir,
    config: &'a Config,
    clock: &'a Arc<dyn TimeProvider>,
    journal: &'a Journal,
    faults: &'a Faults,
    control_inbox: Sender<ControlCompletion>,
}

impl Workers {
    fn spawn(plan: &WorkerPlan<'_>) -> Self {
        let completions = Completions {
            control: plan.control_inbox.clone(),
        };
        let audit_work = Arc::new(WorkCounter::default());
        let action_work = Arc::new(WorkCounter::default());
        let repository = AuditSpy::new(
            open_audit_repository(plan),
            plan.journal.clone(),
            Arc::clone(&plan.faults.audit),
        );
        let audit = spawn_audit_worker(repository, completions.clone(), Arc::clone(&audit_work));
        let port = ActionPortSpy::new(pidfd_adapter(plan), plan.journal.clone());
        let action = spawn_action_worker(
            port,
            Arc::clone(plan.clock),
            completions.clone(),
            Arc::clone(&action_work),
        );
        let config = spawn_config_worker(plan.state.config_file(), completions);
        Self {
            audit,
            audit_work,
            action,
            action_work,
            config,
            control_inbox: plan.control_inbox.clone(),
        }
    }

    /// No job in flight in the audit or action worker and every completion
    /// taken by the control thread.
    fn quiet(&self) -> bool {
        self.audit_work.is_idle() && self.action_work.is_idle() && self.control_inbox.is_empty()
    }

    fn stop(&self) {
        self.audit.stop(WORKER_STOP_WAIT);
        self.action.stop(WORKER_STOP_WAIT);
        self.config.stop(WORKER_STOP_WAIT);
    }
}

fn open_audit_repository(plan: &WorkerPlan<'_>) -> Box<dyn AuditRepository> {
    match plan.options.audit {
        AuditBackend::Sqlite => {
            let free_space: Arc<dyn FreeSpaceProbe> = Arc::clone(&plan.faults.disk) as _;
            Box::new(
                SqliteAuditRepository::open(
                    &plan.state.audit_database(),
                    AuditLimits::from_config(plan.config),
                    Arc::clone(plan.clock),
                    free_space,
                )
                .expect("open the audit database"),
            )
        }
        AuditBackend::Unavailable => Box::new(UnavailableAudit::new("SQLite runtime gate failed")),
    }
}

/// `PidfdActionAdapter::new` is `with_parts(clock, LinuxPidfd,
/// HostProcessFiles)`; the suite only slides the system-call spy in between.
fn pidfd_adapter(plan: &WorkerPlan<'_>) -> PidfdActionAdapter {
    let syscalls: Box<dyn PidfdSyscalls> = match plan.options.kernel {
        Kernel::Pidfd => Box::new(SyscallSpy::new(LinuxPidfd, plan.journal.clone())),
        Kernel::WithoutPidfd => Box::new(SyscallSpy::new(KernelWithoutPidfd, plan.journal.clone())),
    };
    PidfdActionAdapter::with_parts(Arc::clone(plan.clock), syscalls, Box::new(HostProcessFiles))
}

/// The running stack plus the handles a test needs to drive and observe it.
pub struct ActionStack {
    session: SessionDriver,
    bystander: Option<SessionDriver>,
    trigger: ShutdownTrigger,
    manager_thread: Option<JoinHandle<ShutdownSummary>>,
    workers: Workers,
    snapshots: Arc<TableSnapshots>,
    clock: Arc<TestClock>,
    journal: Journal,
    faults: Faults,
    identity: Identity,
    state: Arc<StateDir>,
}

impl ActionStack {
    pub fn start(options: StackOptions) -> Self {
        Self::start_in(StateDir::create("stack"), options)
    }

    /// Starts on an existing state directory (a restart of a previous stack).
    pub fn start_in(state: Arc<StateDir>, options: StackOptions) -> Self {
        let clock = TestClock::following_real_time();
        let time: Arc<dyn TimeProvider> = Arc::clone(&clock) as _;
        let config = options.config();
        let journal = Journal::default();
        let faults = Faults::default();
        let identity = Identity::of_this_process(&clock, options.caller_euid);
        let channels = ManagerChannels::create();
        let workers = Workers::spawn(&WorkerPlan {
            options: &options,
            state: &state,
            config: &config,
            clock: &time,
            journal: &journal,
            faults: &faults,
            control_inbox: channels.inboxes().control,
        });
        let snapshots = Arc::new(TableSnapshots::default());
        let ports = manager_ports(
            &workers,
            &journal,
            Arc::clone(&time),
            Arc::clone(&snapshots) as _,
            &config,
        );
        let mut manager =
            Manager::with_channels(manager_setup(&config, &identity), ports, channels)
                .expect("valid Manager setup");
        let session = SessionDriver::new(manager.open_session().expect("open the session"));
        let bystander = options
            .bystander_session
            .then(|| SessionDriver::new(manager.open_session().expect("open a second session")));
        let trigger = manager.shutdown_trigger();
        let manager_thread = thread::Builder::new()
            .name("suite-manager".to_owned())
            .spawn(move || manager.run())
            .expect("spawn the control thread");
        let stack = Self {
            session,
            bystander,
            trigger,
            manager_thread: Some(manager_thread),
            workers,
            snapshots,
            clock,
            journal,
            faults,
            identity,
            state,
        };
        stack.session.show_actions_view();
        stack
    }

    pub fn clock(&self) -> &TestClock {
        &self.clock
    }

    pub fn faults(&self) -> &Faults {
        &self.faults
    }

    pub fn steps(&self) -> Steps {
        self.journal.steps()
    }

    /// The caller euid the Manager authorizes actions with.
    pub fn caller_euid(&self) -> u32 {
        self.identity.euid
    }

    pub fn session(&self) -> &SessionDriver {
        &self.session
    }

    pub fn bystander(&self) -> &SessionDriver {
        self.bystander
            .as_ref()
            .expect("stack started with bystander_session")
    }

    // ----- process table -----------------------------------------------------

    /// The stable identity of a process as this instance names it.
    pub fn key_for(&self, facts: &ProcessFacts, pid_namespace: &str) -> ProcessKey {
        ProcessKey {
            host_id: self.identity.host_id.clone(),
            boot_id: self.identity.boot_id.clone(),
            pid_namespace: PidNamespace::new(pid_namespace).expect("valid pid namespace"),
            pid: Pid::new(i64::from(facts.pid)).expect("valid pid"),
            start_time_ticks: facts.start_time_ticks,
        }
    }

    /// This instance's own PID namespace (`pid:[N]`).
    pub fn pid_namespace(&self) -> &str {
        self.identity.pid_namespace.as_str()
    }

    /// Publishes the child in the process table with its real procfs facts
    /// and returns the key a frontend would select.
    pub fn publish(&self, child: &DisposableChild) -> ProcessKey {
        let facts = ProcessFacts::read(child.pid()).expect("read the child's procfs facts");
        let key = self.key_for(&facts, &pid_namespace(child.pid()));
        self.publish_row(&key, Some(facts.real_uid));
        key
    }

    /// Publishes one process-table row (`uid` as the collector saw it).
    pub fn publish_row(&self, key: &ProcessKey, uid: Option<u32>) {
        self.snapshots.upsert(ProcessRow {
            key: key.clone(),
            name: "disposable".to_owned(),
            command: String::new(),
            uid,
            state: Some('S'),
            descendants: None,
            cpu_percent: None,
            rss_bytes: None,
            missing_reason: None,
            gpu: Vec::new(),
            sample_kind: SampleKind::Measured,
            read_finished_boot_ns: self.clock.now_boot_ns(),
        });
    }

    // ----- commands of the main session ------------------------------------------

    pub fn prepare(&self, target: &ProcessKey, signal: Signal) -> Uuid {
        self.session.submit(CommandBody::PrepareSignal {
            target: target.clone(),
            signal,
        })
    }

    /// A SIGTERM confirmation the Manager must issue.
    pub fn prepared(&self, target: &ProcessKey) -> Prepared {
        let request_id = self.prepare(target, Signal::Sigterm);
        let result = self.session.result(request_id);
        assert_eq!(
            result.status,
            CommandStatus::Completed,
            "prepare failed: {result:?}"
        );
        assert_eq!(result.action_state, Some(ActionState::Prepared));
        Prepared {
            request_id,
            token: result.confirmation_token.expect("a confirmation token"),
            expires_boot_ns: result.expires_boot_ns.expect("a confirmation deadline"),
        }
    }

    pub fn confirm(&self, token: &ConfirmationToken) -> Uuid {
        self.session.submit(CommandBody::ConfirmSignal {
            confirmation_token: token.clone(),
        })
    }

    pub fn result(&self, request_id: Uuid) -> CommandResult {
        self.session.result(request_id)
    }

    // ----- what the frontend sees --------------------------------------------------

    // ----- what the audit store saw ----------------------------------------------

    /// Waits until the audit store answered the startup recovery and
    /// reported itself healthy, and the Manager handled both answers: from
    /// then on the audit lets actions through (policy and caller decide).
    pub fn await_audit_ready(&self) {
        self.await_journal("a recovered, healthy audit store", |steps| {
            steps.recovered_intents().is_some()
                && steps
                    .statuses_reported()
                    .contains(&PersistenceState::Healthy)
        });
        self.settle();
    }

    /// Waits until the audit store reported that it refuses new records and
    /// the Manager handled the report: from then on actions fail closed.
    pub fn await_audit_refusing(&self) {
        self.await_journal("an audit store refusing new records", |steps| {
            steps
                .statuses_reported()
                .contains(&PersistenceState::Failed)
        });
        self.settle();
    }

    /// Waits until the audit store wrote the outcome row `state` of the
    /// intent of `confirm_request` and returns the row's detail.
    pub fn await_outcome(&self, confirm_request: Uuid, state: ActionState) -> String {
        let mut detail = None;
        self.await_journal(
            &format!("outcome {} of {confirm_request}", state.name()),
            |steps| {
                detail = steps.outcome_detail(confirm_request, state);
                detail.is_some()
            },
        );
        detail.expect("checked by the wait")
    }

    fn await_journal(&self, what: &str, mut accept: impl FnMut(&Steps) -> bool) {
        let deadline = Instant::now() + AUDIT_WAIT;
        loop {
            let steps = self.steps();
            if accept(&steps) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; journal: {steps:#?}"
            );
            thread::sleep(POLL);
        }
    }

    // ----- synchronization and teardown ------------------------------------------

    /// Returns once every completion produced so far was handled by the
    /// control thread and no worker has a job in flight.
    ///
    /// Why the marker works: a `CancelSignal` for a token that was never
    /// issued is answered without any side effect, and the control thread
    /// handles each completion it took to the end before taking a command,
    /// so the marker's result is ordered after every completion that was
    /// already delivered when it was submitted.
    pub fn settle(&self) {
        let deadline = Instant::now() + RESULT_WAIT;
        loop {
            while !self.workers.quiet() {
                assert!(Instant::now() < deadline, "workers never went idle");
                thread::sleep(POLL);
            }
            let marker = self.session.submit(CommandBody::CancelSignal {
                confirmation_token: never_issued_token(),
            });
            let answer = self.session.result(marker);
            assert_eq!(answer.status, CommandStatus::Completed, "{answer:?}");
            if self.workers.quiet() {
                return;
            }
        }
    }

    /// Bounded shutdown; asserts that it left nothing unfinished.
    pub fn stop(mut self) -> StoppedStack {
        self.trigger.trigger("action suite: test finished");
        let thread = self.manager_thread.take().expect("stack is running");
        let summary = join_within(thread, MANAGER_STOP_WAIT);
        self.workers.stop();
        assert!(
            !summary.timed_out && summary.unfinished.is_empty(),
            "shutdown left work unfinished: {:?}",
            summary.unfinished
        );
        StoppedStack {
            state: Arc::clone(&self.state),
            clock: Arc::clone(&self.clock),
        }
    }
}

impl Drop for ActionStack {
    fn drop(&mut self) {
        // With a running Manager only when a test failed before `stop`:
        // never leave threads behind.
        self.trigger.trigger("action suite: cleanup");
        if let Some(thread) = self.manager_thread.take() {
            let deadline = Instant::now() + MANAGER_STOP_WAIT;
            while !thread.is_finished() && Instant::now() < deadline {
                thread::sleep(POLL);
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
        self.workers.stop();
    }
}

/// What remains after a stack stopped: its state directory.
pub struct StoppedStack {
    state: Arc<StateDir>,
    clock: Arc<TestClock>,
}

impl StoppedStack {
    /// Opens the audit database the stack wrote.
    pub fn audit_trail(&self) -> AuditTrail {
        AuditTrail::open(&self.state.audit_database(), Arc::clone(&self.clock) as _)
    }

    /// The state directory, for a restart on the same audit database.
    pub fn state(&self) -> Arc<StateDir> {
        Arc::clone(&self.state)
    }
}

fn join_within(thread: JoinHandle<ShutdownSummary>, limit: Duration) -> ShutdownSummary {
    let deadline = Instant::now() + limit;
    while !thread.is_finished() {
        assert!(
            Instant::now() < deadline,
            "the Manager did not stop within {limit:?}"
        );
        thread::sleep(POLL);
    }
    thread.join().expect("the control thread did not panic")
}

fn never_issued_token() -> ConfirmationToken {
    ConfirmationToken::new(format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    ))
    .expect("64 hex characters form a valid token")
}

fn manager_setup(config: &Config, identity: &Identity) -> ManagerSetup {
    ManagerSetup {
        saved_config: config.clone(),
        effective_config: config.clone(),
        value_origins: BTreeMap::new(),
        config_file: dgxtop_core::action::PersistenceStatus::healthy(
            dgxtop_core::action::PersistenceKind::Config,
        ),
        host_id: identity.host_id.clone(),
        boot_id: identity.boot_id.clone(),
        pid_namespace: identity.pid_namespace.clone(),
        euid: identity.euid,
        self_pid: identity.self_pid,
        // No adapter hosts: nothing to schedule and no managed host to protect.
        adapters: Vec::new(),
        facts: HostFacts::default(),
        namespace: Namespace::Live,
    }
}

fn manager_ports(
    workers: &Workers,
    journal: &Journal,
    clock: Arc<dyn TimeProvider>,
    snapshots: Arc<dyn SnapshotReader>,
    config: &Config,
) -> ManagerPorts {
    let ledger = ByteLedger::new(config.partition_limits());
    let ingress = QueueGate::new("ingress", 32, 8 * MIB, ledger.clone(), Partition::Ingress);
    ManagerPorts {
        config: Box::new(workers.config.clone()),
        audit: Box::new(RecordingDispatch::new(
            workers.audit.clone(),
            journal.clone(),
            Arc::clone(&workers.audit_work),
        )),
        actions: Box::new(RecordingDispatch::new(
            workers.action.clone(),
            journal.clone(),
            Arc::clone(&workers.action_work),
        )),
        ..inert_ports(clock, snapshots, &ledger, ingress)
    }
}

/// The process table the Manager validates `PrepareSignal` targets against.
#[derive(Default)]
struct TableSnapshots {
    latest: Mutex<Arc<ImmutableSnapshot>>,
}

impl TableSnapshots {
    /// Adds `row`, replacing whatever row held its PID before.
    fn upsert(&self, row: ProcessRow) {
        let mut latest = self.latest.lock().unwrap_or_else(PoisonError::into_inner);
        let mut table = (*latest.process_table).clone();
        table
            .rows
            .retain(|existing| existing.key.pid != row.key.pid);
        table.rows.push(row);
        table.total_seen = u32::try_from(table.rows.len()).unwrap_or(u32::MAX);
        let mut next = (**latest).clone();
        next.revision = next.revision.saturating_add(1);
        next.process_table = Arc::new(table);
        *latest = Arc::new(next);
    }
}

impl SnapshotReader for TableSnapshots {
    fn latest(&self) -> Arc<ImmutableSnapshot> {
        Arc::clone(&self.latest.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

// ----- composition glue as in crates/dgxtop/src/monitor/ ----------------

/// Forwards worker completions to the Manager's control inbox (bounded wait).
#[derive(Clone)]
struct Completions {
    control: Sender<ControlCompletion>,
}

impl Completions {
    fn deliver(&self, completion: ControlCompletion) {
        // A closed inbox means the Manager already stopped.
        let _ = self
            .control
            .send_timeout(completion, COMPLETION_SEND_TIMEOUT);
    }
}

fn spawn_audit_worker(
    repository: AuditSpy,
    completions: Completions,
    work: Arc<WorkCounter>,
) -> PoolHandle<AuditOp> {
    spawn_actor(
        "audit",
        AUDIT_QUEUE,
        repository,
        move |repository: &mut AuditSpy, op: AuditOp| {
            let completion = run_audit_op(repository, op);
            completions.deliver(ControlCompletion::Audit(completion));
            work.finished();
        },
    )
    .expect("spawn the audit worker")
}

fn spawn_action_worker(
    port: ActionPortSpy,
    clock: Arc<dyn TimeProvider>,
    completions: Completions,
    work: Arc<WorkCounter>,
) -> PoolHandle<ActionOp> {
    spawn_actor(
        "action",
        ACTION_QUEUE,
        port,
        move |port: &mut ActionPortSpy, op: ActionOp| {
            let completion = run_action_op(port, op, clock.as_ref(), None);
            completions.deliver(ControlCompletion::Action(completion));
            work.finished();
        },
    )
    .expect("spawn the action worker")
}

fn spawn_config_worker(path: PathBuf, completions: Completions) -> PoolHandle<ConfigJob> {
    let repository = FileConfigRepository::new(path, Arc::new(RealFileSystem));
    spawn_actor(
        "config",
        CONFIG_QUEUE,
        repository,
        move |repository: &mut FileConfigRepository, job: ConfigJob| {
            completions.deliver(run_config_job(repository, job));
        },
    )
    .expect("spawn the config worker")
}
