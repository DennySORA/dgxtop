//! The order everything is built in: the live state and its history, the
//! stores, the control workers, the process history, the adapter hosts and
//! the Manager on its own thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dgxtop_collectors::registry::default_collectors;
use dgxtop_core::budget::{ByteLedger, Partition, QueueGate};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::config::Config;
use dgxtop_core::ports::{ConfigRepository, Logger};
use dgxtop_core::quality::Namespace;
use dgxtop_manager::presentation::HostFacts;
use dgxtop_manager::{Manager, ManagerChannels, ManagerPorts, ManagerSetup};
use dgxtop_runtime::clock::{
    current_euid, host_identity, host_name, kernel_release, pid_namespace_of,
};
use dgxtop_runtime::logger::FileLogger;
use dgxtop_runtime::supervisor::{Supervisor, SupervisorConfig};
use dgxtop_runtime::timezone::local_utc_offset_s;
use dgxtop_store::config::FileConfigRepository;
use dgxtop_store::lock::InstanceLock;
use dgxtop_store::paths::StatePaths;

use super::hosts::host_specs;
use super::signals::SignalWatch;
use super::workers::{Completions, Workers};
use super::{Application, StartError};
use crate::cli::Cli;
use crate::fail_stop::FailStop;
use crate::startup::LoadedConfiguration;
use crate::version::BuildInfo;

/// What the start hands over to the assembly.
pub(super) struct AssembleInputs<'a> {
    pub(super) cli: &'a Cli,
    pub(super) paths: &'a StatePaths,
    pub(super) loaded: &'a LoadedConfiguration,
    pub(super) config_repository: FileConfigRepository,
    pub(super) lock: InstanceLock,
    pub(super) logger: Arc<FileLogger>,
    pub(super) replay_root: Option<PathBuf>,
}

/// Shared pieces built before the workers.
pub(super) struct Wiring {
    pub(super) clock: Arc<dyn TimeProvider>,
    pub(super) logger: Arc<dyn Logger>,
    pub(super) ledger: ByteLedger,
    pub(super) ingress: QueueGate,
    pub(super) namespace: Namespace,
    pub(super) fail_stop: FailStop,
    /// The SQLite gate passed; without it nothing is written to SQLite.
    pub(super) sqlite_ok: bool,
}

impl Wiring {
    pub(super) fn new(
        loaded: &LoadedConfiguration,
        (clock, logger): (Arc<dyn TimeProvider>, Arc<dyn Logger>),
        namespace: Namespace,
        sqlite_ok: bool,
    ) -> Self {
        let effective = &loaded.effective;
        let ledger = ByteLedger::new(effective.partition_limits());
        let ingress = QueueGate::new(
            "ingress",
            usize::try_from(effective.queues.collection_batches).unwrap_or(32),
            u64::from(effective.queues.ingress_bytes),
            ledger.clone(),
            Partition::Ingress,
        );
        Self {
            clock,
            fail_stop: FailStop::new(Arc::clone(&logger)),
            logger,
            ledger,
            ingress,
            namespace,
            sqlite_ok,
        }
    }

    /// The archive (and the Overview's sizes kept beside it) is on.
    pub(super) fn archive_on(&self, effective: &Config) -> bool {
        effective.store.archive_enabled && self.sqlite_ok
    }

    pub(super) fn assemble(self, inputs: AssembleInputs<'_>) -> Result<Application, StartError> {
        let AssembleInputs {
            cli,
            paths,
            loaded,
            config_repository,
            lock,
            logger,
            replay_root,
        } = inputs;
        let effective = &loaded.effective;
        let host_id = host_identity(Path::new("/"))?;
        let boot_id = self.clock.boot_id();
        let self_pid = i32::try_from(std::process::id())
            .map_err(|_| StartError::failure("pid out of range"))?;
        let pid_namespace = pid_namespace_of(self_pid)?;
        let channels = ManagerChannels::create();
        let inboxes = channels.inboxes();
        let completions = Completions {
            ingestion: inboxes.ingestion.clone(),
            control: inboxes.control.clone(),
            logger: Arc::clone(&self.logger),
        };
        let mut workers = Workers::default();

        let (writer, snapshots, history_index) = crate::live_store::LiveStore::new(
            &effective.store,
            &effective.sampling,
            self.ledger.clone(),
            self.namespace,
            self.archive_on(effective),
        )
        .into_parts();
        // The state fills the trends at commit; the Manager only reads them
        // (D-99).
        let trends = Arc::new(writer.trends());
        // The Overview's sizes follow the local clock and are kept with the
        // archive (D-82); both before the hot store feeds them.
        history_index
            .overview()
            .set_utc_offset(local_utc_offset_s());
        // The archive opens first: a file it recreates (D-85) is gone before
        // the tiers' reader of it opens.
        let archive = self.spawn_archive(effective, paths, completions.clone(), &mut workers)?;
        self.spawn_history_tiers(
            effective,
            paths,
            (history_index.overview().clone(), host_id.clone()),
            &mut workers,
        )?;
        let store = self.spawn_store(
            writer,
            archive,
            (
                completions.clone(),
                u64::from(effective.queues.archive_bytes),
            ),
            &mut workers,
        )?;
        let collector_registry = default_collectors();
        let collectors = self.spawn_collectors(
            effective,
            collector_registry.clone(),
            completions.clone(),
            &mut workers,
        )?;
        let config_file = config_repository.status();
        let config = self.spawn_config(config_repository, completions.clone(), &mut workers)?;
        let audit = self.spawn_audit(effective, paths, completions.clone(), &mut workers)?;
        let actions = self.spawn_actions(completions.clone(), &mut workers)?;
        let facts = host_facts(self.namespace);
        let process_history = self.spawn_process_history_parts(
            effective,
            paths,
            (boot_id.clone(), facts.utc_offset_s),
            &mut workers,
        )?;
        // The history's readers join the memory charts' bars with the
        // process actor's memory record (D-99).
        let history = self.spawn_history(
            effective,
            Arc::new(history_index.with_process_memory(process_history.memory)),
            completions,
            &mut workers,
        )?;

        let (specs, manifests) = host_specs(cli, effective, self.namespace, &self.clock, &host_id)?;
        let supervisor = Supervisor::start(
            SupervisorConfig::from_config(effective),
            specs,
            self.ledger.clone(),
            self.ingress.clone(),
            inboxes.ingestion.clone(),
            Arc::clone(&self.clock),
            Arc::clone(&self.logger),
        )?;

        let setup = ManagerSetup {
            saved_config: loaded.saved.clone(),
            effective_config: effective.clone(),
            value_origins: loaded.origins.clone(),
            config_file,
            host_id,
            boot_id,
            pid_namespace,
            euid: current_euid(),
            self_pid,
            adapters: manifests,
            facts,
            namespace: self.namespace,
        };
        let ports = ManagerPorts {
            clock: Arc::clone(&self.clock),
            adapters: Box::new(supervisor.clone()),
            collectors: Box::new(collectors),
            collector_registry,
            store: Box::new(store),
            snapshots: Arc::new(snapshots),
            trends,
            history: Box::new(history),
            config: Box::new(config),
            audit: Box::new(audit),
            actions: Box::new(actions),
            process_history: Some(process_history.handles),
            logger: Arc::clone(&self.logger),
            ledger: self.ledger.clone(),
            ingress: self.ingress.clone(),
        };
        let mut manager = Manager::with_channels(setup, ports, channels)?;
        let session = manager.open_session()?;
        let trigger = manager.shutdown_trigger();
        let shutdown = trigger.clone();
        self.fail_stop
            .connect(move |reason| shutdown.trigger(reason));
        let frontend_finished = Arc::new(AtomicBool::new(false));
        SignalWatch {
            trigger: trigger.clone(),
            session: session.clone(),
            frontend_finished: Arc::clone(&frontend_finished),
            logger: Arc::clone(&logger),
        }
        .spawn()?;
        let guard = self.fail_stop.clone();
        let manager_thread = std::thread::Builder::new()
            .name("dgxtop-manager".to_owned())
            .spawn(move || guard.guard("manager", move || manager.run()))?;
        Ok(Application {
            session,
            trigger,
            frontend_finished,
            fail_stop: self.fail_stop,
            manager_thread: Some(manager_thread),
            supervisor,
            workers,
            logger,
            fps: effective.viewer.fps,
            replay_root,
            _lock: lock,
        })
    }
}

fn host_facts(namespace: Namespace) -> HostFacts {
    HostFacts {
        host_name: host_name().unwrap_or_else(|_| "unknown host".to_owned()),
        kernel: kernel_release().ok(),
        cpu_model: None,
        version: BuildInfo::current().version.to_owned(),
        namespace_label: (namespace == Namespace::Replay)
            .then(|| "replay (simulated data)".to_owned()),
        utc_offset_s: local_utc_offset_s(),
    }
}
