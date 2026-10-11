//! The telemetry path of the composition root: adapter-host supervisor →
//! Manager (scheduler, ingestion) → real collectors → real HotStore, with
//! replay hosts as the only data sources.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dgxtop::live_store::LiveStore;
use dgxtop_collectors::registry::default_collectors;
use dgxtop_core::budget::ByteLedger;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::config::Config;
use dgxtop_core::diagnostics::Diagnostics;
use dgxtop_core::identity::{CollectorId, HostId};
use dgxtop_core::observation::AdapterManifest;
use dgxtop_core::ports::SnapshotReader;
use dgxtop_core::quality::Namespace;
use dgxtop_core::testing::MemoryLogger;
use dgxtop_core::view::HostStatus;
use dgxtop_manager::{ManagerChannels, ManagerPorts, ShutdownSummary};
use dgxtop_runtime::queues::StopReport;
use dgxtop_runtime::supervisor::{
    HostLauncher, HostSpec, ShutdownReport, StandardLauncher, Supervisor, SupervisorConfig,
    SupervisorHandle,
};
use dgxtop_store::hot::SharedSnapshot;

use super::manager_rig::{Identity, RunningManager, inert_ports, ingress_gate, manager_setup};
use super::workers::{
    Completions, WorkerSet, spawn_collector_pool, spawn_history_pool, spawn_store_worker,
};

/// Bound of the Manager's own shutdown wait (RUNTIME.md §8) plus slack.
const MANAGER_SHUTDOWN_BOUND: Duration = Duration::from_millis(3_500);
/// Per-worker stop bound used by the composition root.
const WORKER_STOP: Duration = Duration::from_millis(500);
/// A fresh debug binary on a cold page cache may need longer than the
/// production 2 s for its hello; this only widens a startup timeout.
const TEST_HELLO_TIMEOUT_MS: u64 = 10_000;

/// Fast sampling so faults show within a few seconds; all values are inside
/// the configuration schema.
pub fn fast_sampling_config() -> Config {
    let mut config = Config::defaults().expect("embedded defaults");
    config.sampling.interval_ms = 200;
    config.sampling.read_deadline_ms = 300;
    config.sampling.failure_threshold = 1;
    config.sampling.max_backoff_ms = 1_000;
    config.runtime.child_grace_ms = 50;
    config.runtime.child_term_ms = 50;
    config.runtime.child_reap_ms = 300;
    config
        .validate()
        .expect("fast sampling configuration is valid");
    config
}

pub struct TelemetryPlan {
    pub config: Config,
    pub clock: Arc<dyn TimeProvider>,
    /// Must equal the host id the adapter hosts stamp on their records.
    pub host_id: HostId,
    pub specs: Vec<HostSpec>,
    pub manifests: Vec<AdapterManifest>,
    pub launcher: Arc<dyn HostLauncher>,
}

impl TelemetryPlan {
    pub fn new(
        config: Config,
        clock: Arc<dyn TimeProvider>,
        host_id: HostId,
        hosts: Vec<(HostSpec, AdapterManifest)>,
    ) -> Self {
        let (specs, manifests) = hosts.into_iter().unzip();
        Self {
            config,
            clock,
            host_id,
            specs,
            manifests,
            launcher: Arc::new(StandardLauncher),
        }
    }

    pub fn with_launcher(mut self, launcher: Arc<dyn HostLauncher>) -> Self {
        self.launcher = launcher;
        self
    }
}

/// What a bounded composition-style teardown left behind.
#[derive(Debug)]
pub struct Teardown {
    pub manager: ShutdownSummary,
    pub manager_elapsed: Duration,
    pub workers: Vec<(String, StopReport)>,
    pub hosts: ShutdownReport,
    pub hosts_elapsed: Duration,
}

pub struct TelemetryRig {
    // Field order is drop order: control thread, workers, then the hosts.
    pub manager: RunningManager,
    pub workers: WorkerSet,
    pub supervisor: SupervisorHandle,
    pub snapshots: SharedSnapshot,
    pub diagnostics: Diagnostics,
}

impl TelemetryRig {
    pub fn start(plan: TelemetryPlan) -> Self {
        let config = &plan.config;
        let clock = plan.clock;
        let ledger = ByteLedger::for_memory_limit_mib(u64::from(config.store.memory_limit_mib));
        let ingress = ingress_gate(config, &ledger);
        let diagnostics = Diagnostics::new();
        let logger = Arc::new(MemoryLogger::default());
        let channels = ManagerChannels::create();
        let inboxes = channels.inboxes();
        let completions = Completions::from_inboxes(&inboxes);
        let (writer, snapshots, history) = LiveStore::new(
            &config.store,
            &config.sampling,
            &config.runtime,
            ledger.clone(),
            Namespace::Replay,
            false,
        )
        .into_parts();
        let mut workers = WorkerSet::default();
        let store = spawn_store_worker(
            writer,
            None,
            Arc::clone(&clock),
            completions.clone(),
            u64::from(config.queues.archive_bytes),
        );
        workers.track(&store);
        let registry = default_collectors();
        let collectors = spawn_collector_pool(config, registry.clone(), completions.clone());
        workers.track(&collectors);
        let history_pool =
            spawn_history_pool(config, Arc::new(history), Arc::clone(&clock), completions);
        workers.track(&history_pool);
        let supervisor_config = SupervisorConfig {
            hello_timeout_ms: TEST_HELLO_TIMEOUT_MS,
            ..SupervisorConfig::from_config(config)
        };
        let supervisor = Supervisor::start_with_launcher(
            supervisor_config,
            plan.specs,
            ledger.clone(),
            ingress.clone(),
            inboxes.ingestion.clone(),
            Arc::clone(&clock),
            logger.clone(),
            diagnostics.clone(),
            plan.launcher,
        )
        .expect("start the adapter-host supervisor");
        let identity = Identity::new(plan.host_id, clock.as_ref());
        let setup = manager_setup(config, &identity, plan.manifests, Namespace::Replay);
        let ports = ManagerPorts {
            adapters: Box::new(supervisor.clone()),
            collectors: Box::new(collectors),
            collector_registry: registry,
            store: Box::new(store),
            history: Box::new(history_pool),
            logger,
            diagnostics: diagnostics.clone(),
            ..inert_ports(
                Arc::clone(&clock),
                Arc::new(snapshots.clone()),
                &ledger,
                ingress,
            )
        };
        let manager = RunningManager::start(setup, ports, channels);
        Self {
            manager,
            workers,
            supervisor,
            snapshots,
            diagnostics,
        }
    }

    /// Current supervisor status of `adapter`.
    pub fn host(&self, adapter: &str) -> HostStatus {
        self.supervisor
            .statuses()
            .into_iter()
            .find(|status| status.adapter_id == adapter)
            .unwrap_or_else(|| panic!("no host {adapter}"))
    }

    /// Commit time of a collector's domain in the latest snapshot.
    pub fn domain_committed_ns(&self, collector: &str) -> Option<u64> {
        let collector = CollectorId::new(collector).expect("collector id");
        self.snapshots
            .latest()
            .domains
            .get(&collector)
            .map(|domain| domain.committed_boot_ns)
    }

    /// Bounded teardown in composition-root order: Manager (≤ 3 s wait),
    /// workers (500 ms each), then the adapter hosts within `hosts_bound`.
    pub fn tear_down(&mut self, hosts_bound: Duration) -> Teardown {
        let (manager, manager_elapsed) = self
            .manager
            .shut_down("fault test teardown", MANAGER_SHUTDOWN_BOUND);
        let workers = self.workers.stop_all(WORKER_STOP);
        let started = Instant::now();
        let hosts = self.supervisor.shutdown(hosts_bound);
        Teardown {
            manager,
            manager_elapsed,
            workers,
            hosts,
            hosts_elapsed: started.elapsed(),
        }
    }
}
