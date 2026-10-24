//! Shared fakes for the Manager's unit tests. Nothing here touches real
//! drivers, files, processes or the network.

use crate::manager::{Iteration, Manager};
use crate::presentation::HostFacts;
use crate::session::SessionHandle;
use crate::setup::{ManagerInboxes, ManagerPorts, ManagerSetup, ProcessHistoryHandles};
use crate::testing::inert_ports;
use dgxtop_core::action::{ActionOp, AuditCompletion, AuditOp, PersistenceKind, PersistenceStatus};
use dgxtop_core::budget::{ByteLedger, Partition, QueueGate};
use dgxtop_core::canonical::{CollectionBatch, CollectionPlan, CollectorState, ImmutableSnapshot};
use dgxtop_core::commands::{CommandBody, CommandEnvelope, CommandRejection, CommandResult};
use dgxtop_core::config::{Config, ValueOrigin};
use dgxtop_core::identity::PidNamespace;
use dgxtop_core::identity::{AdapterId, CollectorId};
use dgxtop_core::identity::{MetricKey, OriginIdentity, ProfileId};
use dgxtop_core::metric::NativeUnit;
use dgxtop_core::naming::is_replay_adapter;
use dgxtop_core::observation::CatalogDelta;
use dgxtop_core::observation::{AdapterManifest, AdapterOperation, Isolation, SourceDescriptor};
use dgxtop_core::ports::{
    AdapterJob, ConfigJob, ControlCompletion, HistoryJob, IngestionCompletion, ProcessHistoryInput,
    ProcessHistoryReader, SessionPort, StateJob,
};
use dgxtop_core::ports::{Collector, IngressBatch, SnapshotReader};
use dgxtop_core::quality::Availability;
use dgxtop_core::quality::{Namespace, RawKind, SampleKind};
pub(crate) use dgxtop_core::testing::entity_id;
use dgxtop_core::testing::{
    CapturingDispatch, FakeClock, MemoryLogger, StaticProcessHistory, boot_id, host_id,
};
use dgxtop_core::testing::{ObservationBuilder, batch_of, metric_key};
use dgxtop_core::testing::{adapter_id, read_group, source_id};
use dgxtop_core::view::{HostState, HostStatus};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use uuid::Uuid;

/// A valid manifest for `adapter` with the given minimum sample period.
pub(crate) fn manifest(adapter: &str, minimum_ms: u32) -> AdapterManifest {
    let replay = is_replay_adapter(adapter);
    AdapterManifest {
        api_version: 1,
        adapter_id: adapter_id(adapter),
        implementation: format!("{adapter}-fake"),
        isolation: if replay {
            Isolation::InProcessReplay
        } else {
            Isolation::PersistentProcess
        },
        operations: vec![
            AdapterOperation::Discover,
            AdapterOperation::Read,
            AdapterOperation::Shutdown,
        ],
        max_inflight: 1,
        runtime_library: None,
        minimum_sample_ms: minimum_ms,
        source_limit: 2048,
        sample_namespace: if replay {
            Namespace::Replay
        } else {
            Namespace::Live
        },
    }
}

/// A source descriptor of `adapter` in read group `group`.
pub(crate) fn source_descriptor(
    adapter: &str,
    source: &str,
    slot: u32,
    group: &str,
    metric: Option<MetricKey>,
) -> SourceDescriptor {
    let entity_id = metric
        .as_ref()
        .map_or_else(|| entity_id("sensor:test"), |key| key.entity_id.clone());
    SourceDescriptor {
        source_id: source_id(source),
        adapter_id: adapter_id(adapter),
        slot,
        generation: 1,
        entity_id,
        device_id: "device".into(),
        channel: "channel".into(),
        metric,
        origin: OriginIdentity::unknown(),
        driver: "fake".into(),
        api_version: None,
        device_path: "test:///device".into(),
        native_unit: NativeUnit::new("degC").unwrap(),
        raw_kind: RawKind::Gauge,
        sample_kind: SampleKind::Measured,
        read_group: read_group(group),
        update_interval_ms: None,
        profile_id: ProfileId::new("test-profile").unwrap(),
        profile_revision: 1,
        profile_rank: 0,
        label: None,
        counter_modulus: None,
    }
}

/// Snapshot reader whose content tests replace at will.
#[derive(Debug, Default)]
pub(crate) struct FakeSnapshots {
    current: Mutex<Arc<ImmutableSnapshot>>,
}

impl FakeSnapshots {
    pub(crate) fn set(&self, snapshot: ImmutableSnapshot) {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(snapshot);
    }

    pub(crate) fn update(&self, change: impl FnOnce(&mut ImmutableSnapshot)) {
        let mut guard = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = (**guard).clone();
        change(&mut next);
        *guard = Arc::new(next);
    }

    /// Simulates the state adopting `next_state` of a collector.
    pub(crate) fn set_collector_state(&self, collector: &str, revision: u64) {
        self.update(|snapshot| {
            snapshot.collector_states.insert(
                CollectorId::new(collector).unwrap(),
                (
                    revision,
                    Arc::new(CollectorState {
                        revision,
                        ..CollectorState::default()
                    }),
                ),
            );
        });
    }
}

impl SnapshotReader for FakeSnapshots {
    fn latest(&self) -> Arc<ImmutableSnapshot> {
        Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Collector consuming a fixed set of `(adapter, group)` pairs.
#[derive(Debug)]
pub(crate) struct FakeCollector {
    id: CollectorId,
    consumes: Vec<(String, String)>,
}

impl FakeCollector {
    pub(crate) fn new(id: &str, consumes: &[(&str, &str)]) -> Self {
        Self {
            id: CollectorId::new(id).unwrap(),
            consumes: consumes
                .iter()
                .map(|(adapter, group)| ((*adapter).to_owned(), (*group).to_owned()))
                .collect(),
        }
    }
}

impl Collector for FakeCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn consumes(&self, adapter_id: &AdapterId, read_group: &str) -> bool {
        self.consumes
            .iter()
            .any(|(adapter, group)| adapter == adapter_id.as_str() && group == read_group)
    }

    fn collect(&self, plan: &CollectionPlan) -> CollectionBatch {
        CollectionBatch {
            batch_id: plan.batch_id,
            collector_id: plan.collector_id.clone(),
            cycle_id: plan.cycle_id,
            epoch: plan.epoch,
            config_revision: plan.config_revision,
            catalog_revision: plan.catalog.revision,
            policy_revision: plan.policy.revision,
            state_base_revision: plan.state_revision,
            canonical: Vec::new(),
            explanations: Vec::new(),
            counter_intervals: Vec::new(),
            inputs: plan.inputs.clone(),
            process_table: None,
            next_state: (*plan.state).clone(),
            failures: Vec::new(),
        }
    }
}

/// One admitted page with `records` observations finished at `finished_ns`.
pub(crate) fn ingress_page(
    adapter: &str,
    group: &str,
    cycle_id: u64,
    records: usize,
    finished_ns: u64,
) -> Arc<IngressBatch> {
    let key = metric_key("host", "cpu.load.1m", "host");
    let source = format!("{adapter}:test:load");
    let observations = (0..records)
        .map(|_| {
            ObservationBuilder::new(key.clone(), adapter, &source)
                .cycle(cycle_id)
                .read_at(finished_ns)
                .build()
        })
        .collect();
    Arc::new(IngressBatch::new(
        batch_of(adapter, group, cycle_id, observations),
        None,
    ))
}

/// A committed sample of `key` (no value means "unavailable") and a
/// snapshot of such samples, shared with the state crate's tests.
pub(crate) use dgxtop_core::testing::{
    snapshot_entry as sample_entry, snapshot_of as committed_snapshot,
};

/// Boot time at which every harness starts.
pub(crate) const HARNESS_START_NS: u64 = 1_000_000_000_000;
pub(crate) const HARNESS_EUID: u32 = 1000;
pub(crate) const HARNESS_SELF_PID: i32 = 4000;

/// Captured ports of a harness Manager.
pub(crate) struct Ports {
    pub(crate) clock: Arc<FakeClock>,
    pub(crate) adapters: CapturingDispatch<AdapterJob>,
    pub(crate) collectors: CapturingDispatch<CollectionPlan>,
    pub(crate) state: CapturingDispatch<StateJob>,
    pub(crate) history: CapturingDispatch<HistoryJob>,
    pub(crate) config: CapturingDispatch<ConfigJob>,
    pub(crate) audit: CapturingDispatch<AuditOp>,
    pub(crate) actions: CapturingDispatch<ActionOp>,
    /// The history's process actor, wired with
    /// [`HarnessOptions::process_history`].
    pub(crate) process_inputs: CapturingDispatch<ProcessHistoryInput>,
    pub(crate) process_history: Arc<StaticProcessHistory>,
    pub(crate) snapshots: Arc<FakeSnapshots>,
    pub(crate) logger: Arc<MemoryLogger>,
    pub(crate) ledger: ByteLedger,
    pub(crate) ingress: QueueGate,
}

/// Harness configuration.
pub(crate) struct HarnessOptions {
    pub(crate) saved: Config,
    pub(crate) effective: Config,
    pub(crate) origins: BTreeMap<String, ValueOrigin>,
    pub(crate) adapters: Vec<AdapterManifest>,
    pub(crate) collectors: Vec<Arc<dyn Collector>>,
    pub(crate) euid: u32,
    pub(crate) audit_ready: bool,
    /// Wires a fake process actor (D-79); off, the Manager has none.
    pub(crate) process_history: bool,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        let config = Config::defaults().unwrap();
        Self {
            saved: config.clone(),
            effective: config,
            origins: BTreeMap::new(),
            adapters: vec![manifest("procfs", 100)],
            collectors: vec![
                Arc::new(FakeCollector::new("cpu", &[("procfs", "cpu")])),
                Arc::new(FakeCollector::new("memory", &[("procfs", "memory")])),
                Arc::new(FakeCollector::new("process", &[("procfs", "process")])),
            ],
            euid: HARNESS_EUID,
            audit_ready: true,
            process_history: false,
        }
    }
}

pub(crate) fn harness_setup(options: &HarnessOptions) -> ManagerSetup {
    ManagerSetup {
        saved_config: options.saved.clone(),
        effective_config: options.effective.clone(),
        value_origins: options.origins.clone(),
        config_file: dgxtop_core::action::PersistenceStatus::healthy(
            dgxtop_core::action::PersistenceKind::Config,
        ),
        host_id: host_id("test-host"),
        boot_id: boot_id("fake-boot"),
        pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
        euid: options.euid,
        sudo_uid: None,
        self_pid: HARNESS_SELF_PID,
        adapters: options.adapters.clone(),
        facts: HostFacts {
            host_name: "test-host".into(),
            version: "0.2.0".into(),
            ..HostFacts::default()
        },
        namespace: Namespace::Live,
    }
}

pub(crate) fn harness_ports(options: &HarnessOptions) -> (ManagerPorts, Ports) {
    let ledger =
        ByteLedger::for_memory_limit_mib(u64::from(options.effective.store.memory_limit_mib));
    let ports = Ports {
        clock: FakeClock::new(HARNESS_START_NS),
        adapters: CapturingDispatch::new(),
        collectors: CapturingDispatch::new(),
        state: CapturingDispatch::new(),
        history: CapturingDispatch::new(),
        config: CapturingDispatch::new(),
        audit: CapturingDispatch::new(),
        actions: CapturingDispatch::new(),
        process_inputs: CapturingDispatch::new(),
        process_history: StaticProcessHistory::new(Default::default()),
        snapshots: Arc::new(FakeSnapshots::default()),
        logger: Arc::new(MemoryLogger::default()),
        ingress: QueueGate::new(
            "ingress",
            32,
            u64::from(options.effective.queues.ingress_bytes),
            ledger.clone(),
            Partition::Ingress,
        ),
        ledger,
    };
    let manager_ports = ManagerPorts {
        adapters: Box::new(ports.adapters.clone()),
        collectors: Box::new(ports.collectors.clone()),
        collector_registry: options.collectors.clone(),
        state: Box::new(ports.state.clone()),
        history: Box::new(ports.history.clone()),
        config: Box::new(ports.config.clone()),
        audit: Box::new(ports.audit.clone()),
        actions: Box::new(ports.actions.clone()),
        process_history: options.process_history.then(|| ProcessHistoryHandles {
            inputs: Box::new(ports.process_inputs.clone()),
            runs: Arc::clone(&ports.process_history) as Arc<dyn ProcessHistoryReader>,
        }),
        logger: ports.logger.clone(),
        ..inert_ports(
            ports.clock.clone(),
            ports.snapshots.clone(),
            &ports.ledger,
            ports.ingress.clone(),
        )
    };
    (manager_ports, ports)
}

/// A Manager with every port faked, driven one iteration at a time.
pub(crate) struct Harness {
    pub(crate) manager: Manager,
    pub(crate) inboxes: ManagerInboxes,
    pub(crate) ports: Ports,
    pub(crate) session: SessionHandle,
}

impl Harness {
    pub(crate) fn new(options: HarnessOptions) -> Self {
        let (manager_ports, ports) = harness_ports(&options);
        let (mut manager, inboxes) = Manager::new(harness_setup(&options), manager_ports).unwrap();
        let session = manager.open_session().unwrap();
        let mut harness = Self {
            manager,
            inboxes,
            ports,
            session,
        };
        harness.step();
        if options.audit_ready {
            harness.ports.audit.take();
            harness.control(ControlCompletion::Audit(AuditCompletion::Recovered {
                result: Ok(Vec::new()),
            }));
            harness.control(ControlCompletion::Audit(AuditCompletion::Status(
                PersistenceStatus::healthy(PersistenceKind::Audit),
            )));
            harness.step();
        }
        harness
    }

    pub(crate) fn step(&mut self) -> Iteration {
        self.manager.iterate()
    }

    /// Iterates until nothing is ready (bounded).
    pub(crate) fn settle(&mut self) {
        for _ in 0..256 {
            if !self.step().more_ready {
                return;
            }
        }
        panic!("manager never settled");
    }

    pub(crate) fn now_ns(&self) -> u64 {
        dgxtop_core::clock::TimeProvider::now_boot_ns(&*self.ports.clock)
    }

    pub(crate) fn envelope(&self, body: CommandBody) -> CommandEnvelope {
        CommandEnvelope::new(self.session.session_id().clone(), body)
    }

    pub(crate) fn submit(&self, body: CommandBody) -> Result<Uuid, CommandRejection> {
        let envelope = self.envelope(body);
        let request_id = envelope.request_id;
        self.session.submit(envelope).map(|()| request_id)
    }

    pub(crate) fn results(&self) -> Vec<CommandResult> {
        self.session.drain_results(usize::MAX)
    }

    pub(crate) fn result_for(&self, request_id: Uuid) -> CommandResult {
        let results = self.results();
        results
            .iter()
            .find(|result| result.request_id == request_id)
            .cloned()
            .unwrap_or_else(|| panic!("no result for {request_id}: {results:?}"))
    }

    pub(crate) fn ingest(&self, completion: IngestionCompletion) {
        self.inboxes.ingestion.try_send(completion).unwrap();
    }

    pub(crate) fn control(&self, completion: ControlCompletion) {
        self.inboxes.control.try_send(completion).unwrap();
    }

    pub(crate) fn host_up(&mut self, adapter: &str, generation: u32, pid: Option<i32>) {
        self.ingest(IngestionCompletion::HostStatus(HostStatus {
            adapter_id: adapter.into(),
            state: HostState::Healthy,
            generation,
            pid,
            last_success_boot_ns: None,
            timeouts_total: 0,
            consecutive_failures: 0,
            restarts_total: 0,
            backoff_until_boot_ns: None,
            detail: None,
        }));
        self.settle();
    }

    /// Answers every captured discovery request with a complete catalog
    /// page; returns the read jobs captured meanwhile.
    pub(crate) fn answer_discovery(&mut self, sources: Vec<SourceDescriptor>) -> Vec<AdapterJob> {
        let mut others = Vec::new();
        for job in self.ports.adapters.take() {
            match job {
                AdapterJob::Discover(request) => {
                    let delta = CatalogDelta {
                        request_id: Some(request.request_id),
                        adapter_id: Some(request.adapter_id.clone()),
                        generation: 1,
                        availability: Some(Availability::Available),
                        sources: sources
                            .iter()
                            .filter(|source| source.adapter_id == request.adapter_id)
                            .cloned()
                            .collect(),
                        complete: true,
                        ..CatalogDelta::default()
                    };
                    self.ingest(IngestionCompletion::Catalog {
                        adapter_id: request.adapter_id,
                        request_id: request.request_id,
                        host_generation: request.host_generation,
                        result: Ok(delta),
                    });
                }
                read @ AdapterJob::Read(_) => others.push(read),
            }
        }
        self.settle();
        others
    }
}
