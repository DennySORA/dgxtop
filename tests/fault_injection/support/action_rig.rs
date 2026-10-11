//! Manager + real SQLite audit (`SqliteAuditRepository` behind the audit
//! actor) + recording action port + a fixed process table. Used by every
//! scenario that has to prove "no durable intent, no signal".

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dgxtop_core::action::{ConfirmationToken, PersistenceState, Signal};
use dgxtop_core::budget::ByteLedger;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::commands::{CommandBody, ViewKind};
use dgxtop_core::config::Config;
use dgxtop_core::identity::ProcessKey;
use dgxtop_core::quality::Namespace;
use dgxtop_core::view::ActionsView;
use dgxtop_manager::{ManagerChannels, ManagerPorts};
use dgxtop_runtime::clock::BootClock;
use dgxtop_runtime::queues::PoolHandle;
use dgxtop_store::audit::{AuditLimits, SqliteAuditRepository};

use super::manager_rig::{
    Identity, ProcessTableSnapshots, RunningManager, TEST_EUID, inert_ports, ingress_gate,
    manager_setup, process_row,
};
use super::probes::{AdjustableFreeSpace, Gate, TempDir};
use super::workers::{
    ActionLog, Completions, RecordingActionPort, WorkerSet, spawn_action_worker, spawn_audit_worker,
};

/// PID of the (fictitious) user process every scenario targets.
const TARGET_PID: i64 = 4242;
/// How long a scenario waits for the audit status to reach a projection.
const STATUS_WAIT: Duration = Duration::from_secs(5);

/// Defaults, with process actions enabled (the shipped default).
pub fn action_config() -> Config {
    Config::defaults().expect("embedded defaults")
}

pub struct ActionRig {
    // Field order is drop order: control thread, then workers, then files.
    pub manager: RunningManager,
    pub workers: WorkerSet,
    pub audit: PoolHandle<dgxtop_core::action::AuditOp>,
    pub actions: ActionLog,
    pub snapshots: Arc<ProcessTableSnapshots>,
    pub identity: Identity,
    pub clock: Arc<dyn TimeProvider>,
    pub free_space: Arc<AdjustableFreeSpace>,
    pub limits: AuditLimits,
    pub audit_path: PathBuf,
    _dir: TempDir,
}

impl ActionRig {
    /// Starts the rig; `intent_gate` sits in front of audit intent writes.
    pub fn start(config: &Config, free_space: Arc<AdjustableFreeSpace>, intent_gate: Gate) -> Self {
        Self::start_with(config, free_space, intent_gate, |_, _, ports| ports)
    }

    /// Like [`ActionRig::start`]; `customize` may spawn more real workers
    /// (tracked in the rig's worker set) and swap them into the ports.
    pub fn start_with(
        config: &Config,
        free_space: Arc<AdjustableFreeSpace>,
        intent_gate: Gate,
        customize: impl FnOnce(&Completions, &mut WorkerSet, ManagerPorts) -> ManagerPorts,
    ) -> Self {
        let dir = TempDir::new("actions");
        let clock: Arc<dyn TimeProvider> = Arc::new(BootClock::new().expect("boot clock"));
        let identity = Identity::fault_host(clock.as_ref());
        let snapshots =
            ProcessTableSnapshots::with_processes(vec![process_row(identity.process(TARGET_PID))]);
        let ledger = ByteLedger::for_memory_limit_mib(u64::from(config.store.memory_limit_mib));
        let channels = ManagerChannels::create();
        let completions = Completions::from_inboxes(&channels.inboxes());
        let audit_path = dir.join("audit.sqlite3");
        let limits = AuditLimits::from_config(config);
        let repository = SqliteAuditRepository::open(
            &audit_path,
            limits,
            Arc::clone(&clock),
            free_space.clone(),
        )
        .expect("open the audit database");
        let mut workers = WorkerSet::default();
        let audit = spawn_audit_worker(Box::new(repository), intent_gate, completions.clone());
        workers.track(&audit);
        let actions = ActionLog::default();
        let action_worker = spawn_action_worker(
            RecordingActionPort::new(actions.clone(), TEST_EUID),
            Arc::clone(&clock),
            completions.clone(),
        );
        workers.track(&action_worker);
        let ports = ManagerPorts {
            audit: Box::new(audit.clone()),
            actions: Box::new(action_worker),
            ..inert_ports(
                Arc::clone(&clock),
                snapshots.clone(),
                &ledger,
                ingress_gate(config, &ledger),
            )
        };
        let ports = customize(&completions, &mut workers, ports);
        let setup = manager_setup(config, &identity, Vec::new(), Namespace::Live);
        let manager = RunningManager::start(setup, ports, channels);
        manager.show(ViewKind::Overview);
        Self {
            manager,
            workers,
            audit,
            actions,
            snapshots,
            identity,
            clock,
            free_space,
            limits,
            audit_path,
            _dir: dir,
        }
    }

    pub fn target(&self) -> ProcessKey {
        self.identity.process(TARGET_PID)
    }

    pub fn prepare_signal(&self) -> CommandBody {
        CommandBody::PrepareSignal {
            target: self.target(),
            signal: Signal::Sigterm,
        }
    }

    /// Prepares SIGTERM for the target and returns its confirmation token.
    pub fn prepare(&self) -> ConfirmationToken {
        let result = self.manager.execute(self.prepare_signal());
        assert!(result.code.is_ok(), "prepare refused: {result:?}");
        result
            .confirmation_token
            .expect("a prepared action carries a confirmation token")
    }

    /// Actions view of the newest projection accepted by `accept`.
    pub fn actions_view(
        &self,
        what: &str,
        mut accept: impl FnMut(&ActionsView) -> bool,
    ) -> Arc<ActionsView> {
        let projection = self.manager.projection_where(what, STATUS_WAIT, |model| {
            model.modules.actions.as_deref().is_some_and(&mut accept)
        });
        projection
            .modules
            .actions
            .clone()
            .expect("checked by the predicate")
    }

    /// Waits until a projection shows the audit store healthy, which is
    /// when the Manager allows actions here (policy and caller allow them).
    /// The Manager asks the audit worker for its recovery before its status
    /// and handles the answers in order, so the recovery was answered too.
    pub fn wait_until_audit_is_healthy(&self) {
        self.actions_view("a healthy audit store", |actions| {
            actions.audit.state == PersistenceState::Healthy
        });
    }

    /// Opens a second handle on the audit file (only after the audit worker
    /// stopped) to read the durable trail.
    pub fn reopen_audit(&self) -> SqliteAuditRepository {
        SqliteAuditRepository::open(
            &self.audit_path,
            self.limits,
            Arc::clone(&self.clock),
            self.free_space.clone(),
        )
        .expect("reopen the audit database")
    }
}
