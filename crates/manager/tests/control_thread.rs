//! Public-API tests with a real control thread (`Manager::run`): bounded
//! shutdown in real time (AT39) and a cross-thread command flood (AT23).
//! Every port is a fake from `dgxtop_core::testing`; no real I/O happens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use dgxtop_core::action::{ActionOp, AuditCompletion, AuditOp, PersistenceKind, PersistenceStatus};
use dgxtop_core::budget::{ByteLedger, Partition, QueueGate};
use dgxtop_core::canonical::{CollectionBatch, CollectionPlan, ImmutableSnapshot};
use dgxtop_core::commands::{CommandBody, CommandEnvelope, SortKey, ViewKind};
use dgxtop_core::config::Config;
use dgxtop_core::diagnostics::Diagnostics;
use dgxtop_core::errors::{ErrorCode, ResultCode};
use dgxtop_core::identity::{AdapterId, CollectorId, PidNamespace};
use dgxtop_core::observation::{AdapterManifest, AdapterOperation, Isolation};
use dgxtop_core::ports::{
    AdapterJob, Collector, ConfigJob, ControlCompletion, Dispatch, DispatchError, HistoryJob,
    IngestionCompletion, SessionPort, SnapshotReader, StoreJob,
};
use dgxtop_core::quality::Namespace;
use dgxtop_core::testing::{
    CapturingDispatch, FakeClock, MemoryLogger, adapter_id, boot_id, host_id,
};
use dgxtop_core::view::{HostState, HostStatus};
use dgxtop_manager::presentation::HostFacts;
use dgxtop_manager::{Manager, ManagerInboxes, ManagerPorts, ManagerSetup};

#[derive(Default)]
struct Snapshots(Mutex<Arc<ImmutableSnapshot>>);

impl SnapshotReader for Snapshots {
    fn latest(&self) -> Arc<ImmutableSnapshot> {
        Arc::clone(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

struct NoopCollector(CollectorId);

impl Collector for NoopCollector {
    fn id(&self) -> &CollectorId {
        &self.0
    }

    fn consumes(&self, adapter_id: &AdapterId, read_group: &str) -> bool {
        adapter_id.as_str() == "procfs" && read_group == "cpu"
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
            events: Vec::new(),
        }
    }
}

struct Fixture {
    manager: Manager,
    inboxes: ManagerInboxes,
    config_jobs: CapturingDispatch<ConfigJob>,
    adapters: CapturingDispatch<AdapterJob>,
    ledger: ByteLedger,
}

fn manifest() -> AdapterManifest {
    AdapterManifest {
        api_version: 1,
        adapter_id: adapter_id("procfs"),
        implementation: "procfs-fake".into(),
        isolation: Isolation::PersistentProcess,
        operations: vec![AdapterOperation::Discover, AdapterOperation::Read],
        max_inflight: 1,
        runtime_library: None,
        minimum_sample_ms: 100,
        source_limit: 2048,
        sample_namespace: Namespace::Live,
    }
}

/// An audit port whose first use panics: an invariant failure injected into
/// the control thread (the Manager asks the audit for recovery at start).
struct PanickingAudit;

impl Dispatch<AuditOp> for PanickingAudit {
    fn try_dispatch(&self, _job: AuditOp) -> Result<(), DispatchError> {
        panic!("injected invariant failure in the control thread");
    }
}

fn fixture() -> Fixture {
    fixture_with_audit(Box::new(CapturingDispatch::<AuditOp>::new()))
}

fn fixture_with_audit(audit: Box<dyn Dispatch<AuditOp>>) -> Fixture {
    let config = Config::defaults().unwrap();
    let ledger = ByteLedger::for_memory_limit_mib(256);
    let config_jobs = CapturingDispatch::new();
    let adapters = CapturingDispatch::new();
    let ports = ManagerPorts {
        clock: FakeClock::new(1_000_000_000_000),
        adapters: Box::new(adapters.clone()),
        collectors: Box::new(CapturingDispatch::<CollectionPlan>::new()),
        collector_registry: vec![Arc::new(NoopCollector(CollectorId::new("cpu").unwrap()))],
        store: Box::new(CapturingDispatch::<StoreJob>::new()),
        snapshots: Arc::new(Snapshots::default()),
        history: Box::new(CapturingDispatch::<HistoryJob>::new()),
        archive_reads: None,
        config: Box::new(config_jobs.clone()),
        audit,
        actions: Box::new(CapturingDispatch::<ActionOp>::new()),
        process_probe: None,
        probe_deriver: Arc::new(dgxtop_core::testing::IdleProbeDeriver),
        process_history: None,
        logger: Arc::new(MemoryLogger::default()),
        diagnostics: Diagnostics::new(),
        ledger: ledger.clone(),
        ingress: QueueGate::new("ingress", 32, 8 << 20, ledger.clone(), Partition::Ingress),
    };
    let setup = ManagerSetup {
        saved_config: config.clone(),
        effective_config: config,
        value_origins: BTreeMap::new(),
        host_id: host_id("test-host"),
        boot_id: boot_id("fake-boot"),
        pid_namespace: PidNamespace::new("pid:[1]").unwrap(),
        euid: 1000,
        self_pid: 4000,
        adapters: vec![manifest()],
        facts: HostFacts::default(),
        namespace: Namespace::Live,
    };
    let (manager, inboxes) = Manager::new(setup, ports).unwrap();
    Fixture {
        manager,
        inboxes,
        config_jobs,
        adapters,
        ledger,
    }
}

fn navigation(session: &dyn SessionPort) -> CommandEnvelope {
    CommandEnvelope::new(
        session.session_id().clone(),
        CommandBody::UpdateViewSession {
            view: ViewKind::Overview,
            selected_entity_id: None,
            selected_process: None,
            filter: String::new(),
            sort: SortKey::Pid,
            ascending: true,
            frame_revision: 0,
        },
    )
}

#[test]
fn at39_a_control_thread_that_panics_still_tells_every_session_to_stop() {
    let mut fixture = fixture_with_audit(Box::new(PanickingAudit));
    let session = fixture.manager.open_session().unwrap();
    assert!(!session.shutdown_requested());
    let manager = fixture.manager;
    let outcome = thread::spawn(move || manager.run()).join();
    assert!(
        outcome.is_err(),
        "the injected failure panics the control thread"
    );
    // Frontends must stop (and restore the terminal) instead of showing a
    // frozen view of a dead Manager.
    assert!(session.shutdown_requested());
}

#[test]
fn at39_run_shuts_down_within_three_seconds_despite_a_hung_config_worker() {
    let mut fixture = fixture();
    let session = fixture.manager.open_session().unwrap();
    let trigger = fixture.manager.shutdown_trigger();
    let control = fixture.inboxes.control.clone();
    control
        .send(ControlCompletion::Audit(AuditCompletion::Status(
            PersistenceStatus::healthy(PersistenceKind::Audit),
        )))
        .unwrap();
    let manager = fixture.manager;
    let runner = thread::spawn(move || manager.run());
    // A configuration change whose persist receipt never arrives.
    let envelope = CommandEnvelope::new(
        session.session_id().clone(),
        CommandBody::SetSamplingInterval {
            interval_ms: 2000,
            expected_config_revision: 1,
        },
    );
    let persist_id = envelope.request_id;
    session.submit(envelope).unwrap();
    let waited = Instant::now();
    while fixture.config_jobs.is_empty() {
        assert!(
            waited.elapsed() < Duration::from_secs(5),
            "persist dispatched"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    trigger.trigger("integration test");
    let summary = runner.join().unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(3_800),
        "bounded: {elapsed:?}"
    );
    assert!(summary.timed_out);
    assert!(
        summary
            .unfinished
            .iter()
            .any(|entry| entry.contains("config persist"))
    );
    assert!(session.shutdown_requested());
    let results = session.drain_results(16);
    let persist = results
        .iter()
        .find(|result| result.request_id == persist_id)
        .expect("the accepted command got its one result");
    assert_eq!(persist.code, ResultCode::from(ErrorCode::PersistFailed));
    // After run() returned, submissions fail fast instead of hanging.
    let late = session.submit(navigation(&session)).unwrap_err();
    assert_eq!(late.code, ErrorCode::Unavailable);
}

#[test]
fn at23_flood_from_another_thread_is_busy_and_every_accepted_command_is_answered() {
    let mut fixture = fixture();
    let session = fixture.manager.open_session().unwrap();
    let trigger = fixture.manager.shutdown_trigger();
    fixture
        .inboxes
        .ingestion
        .send(IngestionCompletion::HostStatus(HostStatus {
            adapter_id: "procfs".into(),
            state: HostState::Healthy,
            generation: 1,
            pid: Some(5000),
            last_success_boot_ns: None,
            timeouts_total: 0,
            consecutive_failures: 0,
            restarts_total: 0,
            backoff_until_boot_ns: None,
            detail: None,
        }))
        .unwrap();
    let manager = fixture.manager;
    let runner = thread::spawn(move || manager.run());
    let started = Instant::now();
    let mut accepted = BTreeSet::new();
    let mut answered = BTreeSet::new();
    let mut busy = 0u32;
    for round in 0..100_000u32 {
        let envelope = navigation(&session);
        let request_id = envelope.request_id;
        match session.submit(envelope) {
            Ok(()) => {
                accepted.insert(request_id);
            }
            Err(rejection) => {
                assert_eq!(rejection.code, ErrorCode::Busy);
                busy += 1;
            }
        }
        if round % 64 == 0 {
            for result in session.drain_results(usize::MAX) {
                assert!(accepted.contains(&result.request_id));
                assert!(answered.insert(result.request_id), "one result each");
            }
        }
    }
    let submit_time = started.elapsed();
    assert!(
        submit_time < Duration::from_secs(30),
        "submit never blocks: {submit_time:?}"
    );
    // The control loop kept dispatching work while commands flooded in.
    let waited = Instant::now();
    while fixture.adapters.is_empty() {
        assert!(
            waited.elapsed() < Duration::from_secs(5),
            "discovery dispatched"
        );
        thread::sleep(Duration::from_millis(5));
    }
    // Shutdown is out of band and not starved by the flood.
    trigger.trigger("flood done");
    let summary = runner.join().unwrap();
    assert!(!summary.timed_out);
    for result in session.drain_results(usize::MAX) {
        assert!(answered.insert(result.request_id), "one result each");
    }
    assert_eq!(answered, accepted, "every accepted command answered once");
    assert!(
        busy > 0,
        "a 100 000 command flood must hit the bounded queue"
    );
    assert_eq!(
        fixture.ledger.used(Partition::Reserve),
        0,
        "command payload bytes released"
    );
}
