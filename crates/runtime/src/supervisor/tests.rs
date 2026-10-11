//! Supervisor tests with in-process hosts: the same framing, pipes and state
//! machine as child processes, with the adapter running on a host thread.
//! Real child-process behaviour (signals, reaping) is covered by
//! `tests/supervisor_hosts.rs`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dgxtop_core::budget::{ByteLedger, MIB, Partition, QueueGate};
use dgxtop_core::clock::{TimeProvider, ms_to_ns};
use dgxtop_core::observation::{DiscoverRequest, Isolation, ReadPlan};
use dgxtop_core::ports::{
    AdapterFailureKind, AdapterJob, Dispatch, DispatchError, IngestionCompletion, IngressBatch,
};
use dgxtop_core::testing::{MemoryLogger, adapter_id, read_group};
use dgxtop_core::view::HostState;
use uuid::Uuid;

use super::*;
use crate::clock::BootClock;
use crate::host::test_adapter::{Calls, FakeAdapter, ReadBehavior};

const WAIT: Duration = Duration::from_secs(10);

struct Harness {
    handle: SupervisorHandle,
    completions: Receiver<IngestionCompletion>,
    ledger: ByteLedger,
    gate: QueueGate,
    clock: Arc<dyn TimeProvider>,
}

fn test_config() -> SupervisorConfig {
    SupervisorConfig {
        child_grace_ms: 50,
        child_term_ms: 50,
        child_reap_ms: 150,
        backoff_base_ms: 50,
        max_backoff_ms: 200,
        hello_timeout_ms: 5_000,
        probe_deadline_ms: 2_000,
        ..SupervisorConfig::default()
    }
}

fn in_process(adapter: &str, behavior: ReadBehavior, calls: Arc<Calls>) -> HostSpec {
    let name = adapter.to_owned();
    HostSpec {
        adapter_id: adapter_id(adapter),
        launch: HostLaunch::InProcess(Arc::new(move || {
            Box::new(
                FakeAdapter::new(&name, Isolation::InProcessReplay, behavior)
                    .with_calls(Arc::clone(&calls)),
            )
        })),
    }
}

fn start(config: SupervisorConfig, specs: Vec<HostSpec>, ingress_bytes: u64) -> Harness {
    let ledger = ByteLedger::for_memory_limit_mib(256);
    let gate = QueueGate::new(
        "ingress",
        32,
        ingress_bytes,
        ledger.clone(),
        Partition::Ingress,
    );
    let (sender, completions) = crossbeam_channel::bounded(256);
    let clock: Arc<dyn TimeProvider> = Arc::new(BootClock::new().unwrap());
    let handle = Supervisor::start(
        config,
        specs,
        ledger.clone(),
        gate.clone(),
        sender,
        Arc::clone(&clock),
        Arc::new(MemoryLogger::default()),
    )
    .unwrap();
    Harness {
        handle,
        completions,
        ledger,
        gate,
        clock,
    }
}

impl Harness {
    fn wait_for(
        &self,
        mut matches: impl FnMut(&IngestionCompletion) -> bool,
    ) -> IngestionCompletion {
        let deadline = Instant::now() + WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let completion = self
                .completions
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("expected completion not received"));
            if matches(&completion) {
                return completion;
            }
        }
    }

    fn wait_state(&self, adapter: &str, state: HostState) -> dgxtop_core::view::HostStatus {
        let completion = self.wait_for(|completion| {
            matches!(completion, IngestionCompletion::HostStatus(status)
                if status.adapter_id == adapter && status.state == state)
        });
        match completion {
            IngestionCompletion::HostStatus(status) => status,
            _ => unreachable!(),
        }
    }

    fn plan(&self, adapter: &str, generation: u32, cycle: u64, deadline_ms: u64) -> ReadPlan {
        ReadPlan {
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id(adapter),
            host_generation: generation,
            catalog_revision: 1,
            config_revision: 1,
            cycle_id: cycle,
            read_group: read_group("fast"),
            source_slots: vec![1, 2],
            deadline_boot_ns: self.clock.now_boot_ns() + ms_to_ns(deadline_ms),
            epoch: 1,
            page: 0,
        }
    }

    fn batch_for(
        &self,
        request_id: Uuid,
    ) -> Result<Arc<IngressBatch>, dgxtop_core::ports::AdapterFailure> {
        match self.wait_for(|completion| {
            matches!(completion, IngestionCompletion::Batch { request_id: id, .. } if *id == request_id)
        }) {
            IngestionCompletion::Batch { result, .. } => result,
            _ => unreachable!(),
        }
    }

    fn dispatch_when_ready(&self, job: AdapterJob) {
        let deadline = Instant::now() + WAIT;
        loop {
            match self.handle.try_dispatch(job.clone()) {
                Ok(()) => return,
                Err(DispatchError::Busy) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("dispatch failed: {error:?}"),
            }
        }
    }
}

#[test]
fn in_process_round_trip_accounts_and_releases_ingress_bytes() {
    let calls = Arc::new(Calls::default());
    let harness = start(
        test_config(),
        vec![in_process(
            "replay",
            ReadBehavior::Healthy,
            Arc::clone(&calls),
        )],
        8 * MIB,
    );
    harness.wait_state("replay", HostState::Healthy);
    let discover = DiscoverRequest {
        request_id: Uuid::new_v4(),
        adapter_id: adapter_id("replay"),
        host_generation: 1,
        catalog_revision: 1,
        config_revision: 1,
        deadline_boot_ns: harness.clock.now_boot_ns() + ms_to_ns(2_000),
        previous_generation: 0,
        page: 0,
    };
    harness.dispatch_when_ready(AdapterJob::Discover(discover.clone()));
    let catalog = harness.wait_for(|completion| {
        matches!(completion, IngestionCompletion::Catalog { request_id, .. } if *request_id == discover.request_id)
    });
    assert!(matches!(
        catalog,
        IngestionCompletion::Catalog { result: Ok(ref delta), .. } if delta.complete
    ));
    let plan = harness.plan("replay", 1, 1, 300);
    harness.dispatch_when_ready(AdapterJob::Read(plan.clone()));
    let batch = harness.batch_for(plan.request_id).unwrap();
    assert_eq!(batch.records.len(), 2);
    assert_eq!(batch.request_id, plan.request_id);
    assert!(batch.accounted_bytes() > 0);
    assert_eq!(
        harness.ledger.used(Partition::Ingress),
        batch.accounted_bytes()
    );
    assert_eq!(harness.gate.len(), 0, "queue slot released at hand-off");
    drop(batch);
    // The broker's recycle area keeps the shared result (and its bytes)
    // until the plan deadline; then everything is released.
    let released_by = Instant::now() + WAIT;
    while harness.ledger.used(Partition::Ingress) != 0 {
        assert!(Instant::now() < released_by, "ingress bytes never released");
        std::thread::sleep(Duration::from_millis(5));
    }
    let report = harness.handle.shutdown(Duration::from_secs(2));
    assert!(report.completed_within_deadline);
    assert!(report.unreaped().is_empty());
    assert_eq!(calls.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(
        harness
            .handle
            .try_dispatch(AdapterJob::Read(harness.plan("replay", 1, 2, 1_000))),
        Err(DispatchError::Closed)
    );
}

#[test]
fn busy_while_in_flight_and_capacity_when_credit_is_missing() {
    let calls = Arc::new(Calls::default());
    let harness = start(
        test_config(),
        vec![in_process(
            "replay",
            ReadBehavior::Slow(Duration::from_millis(300)),
            Arc::clone(&calls),
        )],
        8 * MIB,
    );
    harness.wait_state("replay", HostState::Healthy);
    let first = harness.plan("replay", 1, 1, 2_000);
    harness.dispatch_when_ready(AdapterJob::Read(first.clone()));
    let second = harness.plan("replay", 1, 2, 2_000);
    assert_eq!(
        harness.handle.try_dispatch(AdapterJob::Read(second)),
        Err(DispatchError::Busy)
    );
    assert!(harness.batch_for(first.request_id).is_ok());
    harness.handle.shutdown(Duration::from_secs(2));

    let tiny = start(
        test_config(),
        vec![in_process(
            "replay",
            ReadBehavior::Healthy,
            Arc::new(Calls::default()),
        )],
        16 * 1024,
    );
    tiny.wait_state("replay", HostState::Healthy);
    let plan = tiny.plan("replay", 1, 1, 2_000);
    assert!(matches!(
        tiny.handle.try_dispatch(AdapterJob::Read(plan)),
        Err(DispatchError::Capacity(_))
    ));
    tiny.handle.shutdown(Duration::from_secs(2));
}

#[test]
fn invalid_stale_and_expired_jobs_are_answered_with_failures() {
    let harness = start(
        test_config(),
        vec![in_process(
            "replay",
            ReadBehavior::Healthy,
            Arc::new(Calls::default()),
        )],
        8 * MIB,
    );
    harness.wait_state("replay", HostState::Healthy);
    assert_eq!(
        harness
            .handle
            .try_dispatch(AdapterJob::Read(harness.plan("ghost", 1, 1, 1_000))),
        Err(DispatchError::Closed)
    );
    let stale = harness.plan("replay", 7, 1, 1_000);
    harness
        .handle
        .try_dispatch(AdapterJob::Read(stale.clone()))
        .unwrap();
    let failure = harness.batch_for(stale.request_id).unwrap_err();
    assert_eq!(failure.kind, AdapterFailureKind::HostUnavailable);
    let mut expired = harness.plan("replay", 1, 2, 1_000);
    expired.deadline_boot_ns = harness.clock.now_boot_ns().saturating_sub(1);
    harness
        .handle
        .try_dispatch(AdapterJob::Read(expired.clone()))
        .unwrap();
    assert_eq!(
        harness.batch_for(expired.request_id).unwrap_err().kind,
        AdapterFailureKind::TimedOut
    );
    let mut mismatched = harness.plan("replay", 1, 3, 1_000);
    mismatched.page = 2;
    harness
        .handle
        .try_dispatch(AdapterJob::Read(mismatched.clone()))
        .unwrap();
    assert_eq!(
        harness.batch_for(mismatched.request_id).unwrap_err().kind,
        AdapterFailureKind::Protocol
    );
    harness.handle.shutdown(Duration::from_secs(2));
}

#[test]
fn identical_reads_are_joined_and_served_from_the_recycle_area() {
    let calls = Arc::new(Calls::default());
    let harness = start(
        test_config(),
        vec![in_process(
            "replay",
            ReadBehavior::Slow(Duration::from_millis(150)),
            Arc::clone(&calls),
        )],
        8 * MIB,
    );
    harness.wait_state("replay", HostState::Healthy);
    let first = harness.plan("replay", 1, 5, 3_000);
    let mut duplicate = first.clone();
    duplicate.request_id = Uuid::new_v4();
    harness.dispatch_when_ready(AdapterJob::Read(first.clone()));
    harness
        .handle
        .try_dispatch(AdapterJob::Read(duplicate.clone()))
        .unwrap();
    let original = harness.batch_for(first.request_id).unwrap();
    let joined = harness.batch_for(duplicate.request_id).unwrap();
    assert!(Arc::ptr_eq(&original, &joined));
    assert_eq!(calls.reads.load(Ordering::SeqCst), 1);
    let mut late_duplicate = first.clone();
    late_duplicate.request_id = Uuid::new_v4();
    harness
        .handle
        .try_dispatch(AdapterJob::Read(late_duplicate.clone()))
        .unwrap();
    let cached = harness.batch_for(late_duplicate.request_id).unwrap();
    assert!(Arc::ptr_eq(&original, &cached));
    assert_eq!(
        calls.reads.load(Ordering::SeqCst),
        1,
        "no second host request"
    );
    let next_cycle = harness.plan("replay", 1, 6, 3_000);
    harness.dispatch_when_ready(AdapterJob::Read(next_cycle.clone()));
    let fresh = harness.batch_for(next_cycle.request_id).unwrap();
    assert!(!Arc::ptr_eq(&original, &fresh));
    assert_eq!(calls.reads.load(Ordering::SeqCst), 2);
    harness.handle.shutdown(Duration::from_secs(2));
}

#[test]
fn timeout_reports_failure_and_discards_the_late_reply() {
    // Silence after a timeout keeps counting as failures (one per deadline
    // length); threshold 5 keeps the restart (1 s) after the late reply.
    let config = SupervisorConfig {
        failure_threshold: 5,
        ..test_config()
    };
    let calls = Arc::new(Calls::default());
    let harness = start(
        config,
        vec![in_process(
            "replay",
            ReadBehavior::SlowOnce(Duration::from_millis(600)),
            Arc::clone(&calls),
        )],
        8 * MIB,
    );
    harness.wait_state("replay", HostState::Healthy);
    let slow = harness.plan("replay", 1, 1, 200);
    harness.dispatch_when_ready(AdapterJob::Read(slow.clone()));
    let failure = harness.batch_for(slow.request_id).unwrap_err();
    // The failure is emitted at the deadline, before the late reply arrives.
    assert_eq!(failure.kind, AdapterFailureKind::TimedOut);
    let degraded = harness.wait_state("replay", HostState::Degraded);
    assert_eq!(degraded.timeouts_total, 1);
    // The late reply is still outstanding inside the host: no second request.
    assert_eq!(
        harness
            .handle
            .try_dispatch(AdapterJob::Read(harness.plan("replay", 1, 2, 1_000))),
        Err(DispatchError::Busy)
    );
    let recovered = harness.plan("replay", 1, 3, 2_000);
    harness.dispatch_when_ready(AdapterJob::Read(recovered.clone()));
    assert!(harness.batch_for(recovered.request_id).is_ok());
    harness.wait_state("replay", HostState::Healthy);
    harness.handle.shutdown(Duration::from_secs(2));
}

#[test]
fn hung_in_process_host_can_only_be_quarantined() {
    let config = SupervisorConfig {
        failure_threshold: 1,
        ..test_config()
    };
    let calls = Arc::new(Calls::default());
    let harness = start(
        config,
        vec![in_process(
            "replay",
            ReadBehavior::Slow(Duration::from_secs(3)),
            Arc::clone(&calls),
        )],
        8 * MIB,
    );
    harness.wait_state("replay", HostState::Healthy);
    let plan = harness.plan("replay", 1, 1, 100);
    harness.dispatch_when_ready(AdapterJob::Read(plan.clone()));
    assert_eq!(
        harness.batch_for(plan.request_id).unwrap_err().kind,
        AdapterFailureKind::TimedOut
    );
    let quarantined = harness.wait_state("replay", HostState::Quarantined);
    assert_eq!(quarantined.generation, 1);
    assert_eq!(quarantined.pid, None);
    // Never respawned, even well after any backoff would have elapsed.
    std::thread::sleep(Duration::from_millis(400));
    let status = &harness.handle.statuses()[0];
    assert_eq!(status.state, HostState::Quarantined);
    assert_eq!(status.generation, 1);
    assert_eq!(status.restarts_total, 0);
    assert_eq!(
        harness
            .handle
            .try_dispatch(AdapterJob::Read(harness.plan("replay", 1, 2, 1_000))),
        Err(DispatchError::Busy)
    );
    let begin = Instant::now();
    let report = harness.handle.shutdown(Duration::from_millis(300));
    assert!(begin.elapsed() < Duration::from_millis(600));
    assert_eq!(report.unreaped().len(), 1);
}

#[test]
fn crashed_host_is_restarted_with_a_new_generation() {
    let calls = Arc::new(Calls::default());
    let harness = start(
        test_config(),
        vec![in_process(
            "replay",
            ReadBehavior::Panic,
            Arc::clone(&calls),
        )],
        8 * MIB,
    );
    harness.wait_state("replay", HostState::Healthy);
    let plan = harness.plan("replay", 1, 1, 2_000);
    harness.dispatch_when_ready(AdapterJob::Read(plan.clone()));
    // The host replies with a failed batch, then exits: the batch is
    // forwarded (it is a valid reply) and the crash triggers a restart.
    let reply = harness.batch_for(plan.request_id).unwrap();
    assert_eq!(
        reply.error_code.as_deref(),
        Some(crate::host::ADAPTER_PANIC)
    );
    let restarted = harness.wait_for(|completion| {
        matches!(completion, IngestionCompletion::HostStatus(status)
            if status.generation == 2 && status.state != HostState::Starting)
    });
    let IngestionCompletion::HostStatus(status) = restarted else {
        unreachable!()
    };
    assert_eq!(status.restarts_total, 1);
    assert!(status.consecutive_failures >= 1);
    harness.handle.shutdown(Duration::from_secs(2));
}

#[test]
fn start_rejects_too_many_or_duplicate_hosts() {
    let ledger = ByteLedger::for_memory_limit_mib(256);
    let gate = QueueGate::new("ingress", 32, 8 * MIB, ledger.clone(), Partition::Ingress);
    let clock: Arc<dyn TimeProvider> = Arc::new(BootClock::new().unwrap());
    let attempt = |config: SupervisorConfig, specs: Vec<HostSpec>| {
        let (sender, _receiver) = crossbeam_channel::bounded(8);
        Supervisor::start(
            config,
            specs,
            ledger.clone(),
            gate.clone(),
            sender,
            Arc::clone(&clock),
            Arc::new(MemoryLogger::default()),
        )
    };
    let calls = Arc::new(Calls::default());
    let three = vec![
        in_process("a", ReadBehavior::Healthy, Arc::clone(&calls)),
        in_process("b", ReadBehavior::Healthy, Arc::clone(&calls)),
        in_process("c", ReadBehavior::Healthy, Arc::clone(&calls)),
    ];
    let two_hosts = SupervisorConfig {
        max_hosts: 2,
        ..test_config()
    };
    assert!(attempt(two_hosts, three).is_err());
    let duplicate = vec![
        in_process("a", ReadBehavior::Healthy, Arc::clone(&calls)),
        in_process("a", ReadBehavior::Healthy, Arc::clone(&calls)),
    ];
    assert!(attempt(test_config(), duplicate).is_err());
    let invalid = SupervisorConfig {
        max_hosts: 9,
        ..test_config()
    };
    assert!(attempt(invalid, Vec::new()).is_err());
    assert_eq!(ledger.used(Partition::Reserve), 0, "no working set leaked");
}
