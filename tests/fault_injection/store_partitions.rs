//! AT06 (store partitions and partial persistence faults) and AT31 (SQLite
//! disk low-water) across component boundaries:
//!
//! * the HotStore actor feeding a real `SqliteArchive` actor whose storage
//!   first stops answering and then runs out of space;
//! * the Manager's ProcessService talking to a real `SqliteAuditRepository`
//!   through the audit actor while the disk crosses the 64 MiB low-water mark
//!   or the audit storage hangs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use dgxtop::live_store::LiveStore;
use dgxtop_core::action::{ActionState, PersistenceState};
use dgxtop_core::budget::{ByteLedger, Partition};
use dgxtop_core::canonical::{
    AckStatus, ArchiveState, CanonicalValue, CollectionBatch, CollectorState, CommitGuard,
};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::commands::{CommandBody, CommandStatus, ViewKind};
use dgxtop_core::config::Config;
use dgxtop_core::errors::{ErrorCode, InfraError, ResultCode};
use dgxtop_core::identity::{CollectorId, MetricKey};
use dgxtop_core::metric::{CanonicalUnit, Scalar, ids};
use dgxtop_core::ports::{Dispatch, IngestionCompletion, SnapshotReader, StoreJob};
use dgxtop_core::quality::{Freshness, Namespace, Quality, SampleKind, SelectionState};
use dgxtop_core::testing::{FakeClock, metric_key, source_id};
use dgxtop_manager::INGESTION_INBOX_CAPACITY;
use dgxtop_runtime::queues::PoolHandle;
use dgxtop_state::SharedSnapshot;
use dgxtop_store::archive::{ArchiveLimits, SqliteArchive};
use uuid::Uuid;

use crate::support::action_rig::{ActionRig, action_config};
use crate::support::manager_rig::RESULT_WAIT;
use crate::support::probes::{AdjustableFreeSpace, Gate, TempDir, serial, wait_until};
use crate::support::workers::{
    ActionCall, ArchiveFeed, ArchiveProbe, Completions, WorkerSet, spawn_archive_worker,
    spawn_store_worker,
};

const SECOND: u64 = 1_000_000_000;
/// Fake boot time at which the LiveState scenario starts.
const START_NS: u64 = 1_000 * SECOND;
/// A commit must be acknowledged this fast even while the archive is stuck.
const ACK_BOUND: Duration = Duration::from_millis(500);

// ----- LiveState + archive --------------------------------------------------

/// The HotStore actor with the archive actor behind it, on a fake clock.
struct ArchivedStore {
    clock: Arc<FakeClock>,
    store: PoolHandle<StoreJob>,
    acks: Receiver<IngestionCompletion>,
    snapshots: SharedSnapshot,
    ledger: ByteLedger,
    feed: ArchiveFeed,
    probe: Arc<ArchiveProbe>,
    key: MetricKey,
    collector: CollectorId,
    cycle: u64,
    slowest_ack: Duration,
    _workers: WorkerSet,
}

impl ArchivedStore {
    fn start(dir: &TempDir, free_space: Arc<AdjustableFreeSpace>, archive_gate: Gate) -> Self {
        let mut config = Config::defaults().expect("embedded defaults");
        config.store.archive_enabled = true;
        let clock = FakeClock::new(START_NS);
        let time: Arc<dyn TimeProvider> = clock.clone();
        let ledger = ByteLedger::for_memory_limit_mib(u64::from(config.store.memory_limit_mib));
        let (writer, snapshots, _history) = LiveStore::new(
            &config.store,
            &config.sampling,
            ledger.clone(),
            Namespace::Live,
            true,
        )
        .into_parts();
        let archive = SqliteArchive::open(
            &dir.join("history.sqlite3"),
            ArchiveLimits::from_config(&config),
            Arc::clone(&time),
            free_space,
        )
        .expect("open the archive");
        let probe = Arc::new(ArchiveProbe::default());
        let mut workers = WorkerSet::default();
        let archive_worker = spawn_archive_worker(archive, archive_gate, Arc::clone(&probe));
        workers.track(&archive_worker);
        let feed = ArchiveFeed::new(archive_worker);
        let (ingestion, acks) = bounded(INGESTION_INBOX_CAPACITY);
        let (control, _unused_control) = bounded(1);
        let store = spawn_store_worker(
            writer,
            Some(feed.clone()),
            time,
            Completions::new(ingestion, control),
            u64::from(config.queues.archive_bytes),
        );
        workers.track(&store);
        Self {
            clock,
            store,
            acks,
            snapshots,
            ledger,
            feed,
            probe,
            key: metric_key("gpu:GPU-0", ids::GPU_TEMPERATURE, "gpu"),
            collector: CollectorId::new("gpu").expect("collector id"),
            cycle: 0,
            slowest_ack: Duration::ZERO,
            _workers: workers,
        }
    }

    /// Every journal the archive accepted has been persisted or failed.
    fn archive_settled(&self) -> bool {
        self.probe.snapshot().processed() == self.feed.accepted()
    }

    /// A selected, history-eligible GPU temperature read at `at`.
    fn temperature(&self, celsius: f64, at: u64) -> CanonicalValue {
        CanonicalValue {
            key: self.key.clone(),
            cycle_id: self.cycle,
            source_id: Some(source_id("nvml:GPU-0:temp")),
            source_generation: Some(1),
            unit: CanonicalUnit::DegC,
            value: Some(Scalar::from_f64(celsius).expect("finite")),
            sample_kind: SampleKind::Measured,
            selection_state: SelectionState::Selected,
            quality: Quality::Verified,
            freshness: Freshness::Fresh,
            measurement_freshness: Freshness::Unknown,
            observed_boot_ns: None,
            read_finished_boot_ns: at,
            policy_revision: 1,
            candidate_source_ids: Vec::new(),
            reason_codes: Vec::new(),
            eligible_for_history: true,
        }
    }

    /// Commits one reading per fake second for `seconds`, with store
    /// maintenance every 10 s, and checks every acknowledgement.
    fn run_for(&mut self, seconds: u64) {
        for _ in 0..seconds {
            self.clock.advance_ms(1_000);
            self.commit_reading();
            if self.cycle.is_multiple_of(10) {
                self.store
                    .try_dispatch(StoreJob::Maintain)
                    .expect("store accepts maintenance");
            }
        }
    }

    fn commit_reading(&mut self) {
        self.cycle += 1;
        let now = self.clock.now_boot_ns();
        let celsius = 40.0 + f64::from(u32::try_from(self.cycle % 30).unwrap_or(0));
        let base = self.snapshots.latest().collector_state(&self.collector).0;
        let batch = CollectionBatch {
            batch_id: Uuid::new_v4(),
            collector_id: self.collector.clone(),
            cycle_id: self.cycle,
            epoch: 1,
            config_revision: 1,
            catalog_revision: 1,
            policy_revision: 1,
            state_base_revision: base,
            canonical: vec![self.temperature(celsius, now)],
            explanations: Vec::new(),
            counter_intervals: Vec::new(),
            inputs: Vec::new(),
            process_table: None,
            next_state: CollectorState::default(),
            failures: Vec::new(),
        };
        let guard = CommitGuard {
            expected_epoch: 1,
            collector_id: self.collector.clone(),
            state_base_revision: base,
        };
        let sent = Instant::now();
        self.store
            .try_dispatch(StoreJob::Commit {
                batch: Box::new(batch),
                guard,
            })
            .expect("store accepts the commit");
        let completion = self
            .acks
            .recv_timeout(Duration::from_secs(5))
            .expect("the LiveState acknowledges every commit");
        self.slowest_ack = self.slowest_ack.max(sent.elapsed());
        let IngestionCompletion::Committed(ack) = completion else {
            panic!("unexpected completion {completion:?}");
        };
        assert_eq!(ack.status, AckStatus::Committed, "{ack:?}");
        assert_eq!(
            ack.archive,
            ArchiveState::Pending,
            "a memory commit never claims archive durability"
        );
        let current = self
            .snapshots
            .latest()
            .sample(&self.key)
            .and_then(|entry| entry.sample.value)
            .map(Scalar::to_f64_lossy);
        assert_eq!(current, Some(celsius), "the current value refreshed");
    }
}

#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at06_stuck_then_full_archive_never_stalls_hot_store_commits() {
    let _serial = serial();
    let dir = TempDir::new("archive");
    let free_space = AdjustableFreeSpace::below_low_water();
    let archive_gate = Gate::closed();
    let mut store = ArchivedStore::start(&dir, free_space.clone(), archive_gate.clone());

    // 1. The archive storage stops answering: one journal blocks the archive
    //    worker, its mailbox fills, later journals are refused — commits go on.
    let phase = Instant::now();
    store.run_for(14 * 60);
    assert!(
        store.slowest_ack < ACK_BOUND,
        "slowest ack {:?}",
        store.slowest_ack
    );
    assert!(phase.elapsed() < Duration::from_secs(20));
    wait_until(
        "the archive worker stuck on a journal",
        Duration::from_secs(5),
        || archive_gate.blocked() == 1,
    );
    let refused_while_stuck = store.feed.refused();
    assert!(
        refused_while_stuck >= 1,
        "journals beyond the archive mailbox are refused, never waited on"
    );
    assert!(store.probe.snapshot().receipts.is_empty());
    let reserve_with_stuck_journals = store.ledger.used(Partition::Reserve);

    // 2. The storage answers again but the disk is below the low-water mark:
    //    every journal fails, nothing is persisted, commits go on.
    archive_gate.release();
    wait_until(
        "the queued journals to fail",
        Duration::from_secs(10),
        || store.archive_settled(),
    );
    // No commit happened meanwhile: the difference is the journals' bytes,
    // released although none of them could be persisted.
    assert!(
        store.ledger.used(Partition::Reserve) < reserve_with_stuck_journals,
        "failed journals give their reserve bytes back"
    );
    store.run_for(2 * 60);
    wait_until(
        "journals of the full disk to drain",
        Duration::from_secs(10),
        || store.archive_settled(),
    );
    let full = store.probe.snapshot();
    assert!(full.failures.len() >= 2);
    assert!(full.receipts.is_empty(), "nothing persisted on a full disk");
    assert!(
        full.failures.iter().all(|failure| matches!(
            failure,
            InfraError::DiskFull {
                operation: "archive.free_space"
            }
        )),
        "{:?}",
        full.failures
    );
    assert_eq!(full.stats.rows_written, 0);
    assert_eq!(
        full.status.as_ref().map(|status| status.state),
        Some(PersistenceState::Failed)
    );

    // 3. Space is back: later journals are archived; the LiveState never
    //    waited for any of it.
    free_space.restore();
    store.run_for(3 * 60);
    wait_until(
        "journals to be archived again",
        Duration::from_secs(10),
        || store.archive_settled() && !store.probe.snapshot().receipts.is_empty(),
    );
    let recovered = store.probe.snapshot();
    assert!(recovered.stats.rows_written > 0);
    assert_eq!(
        recovered.status.as_ref().map(|status| status.state),
        Some(PersistenceState::Healthy)
    );
    assert!(store.slowest_ack < ACK_BOUND);
}

// ----- Manager + audit -------------------------------------------------------

#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at06_at31_audit_below_low_water_refuses_actions_before_the_action_worker() {
    let _serial = serial();
    let rig = ActionRig::start(
        &action_config(),
        AdjustableFreeSpace::below_low_water(),
        Gate::open(),
    );
    let view = rig.actions_view("the audit refusing new records", |actions| {
        actions.audit.state == PersistenceState::Failed
    });
    assert!(!view.actions_available);
    assert!(
        view.unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("audit store unavailable")),
        "{view:?}"
    );
    let prepare = rig.manager.execute(rig.prepare_signal());
    assert_eq!(prepare.status, CommandStatus::Rejected);
    assert_eq!(prepare.code, ResultCode::from(ErrorCode::Unavailable));
    assert!(prepare.confirmation_token.is_none());
    assert!(
        rig.actions.calls().is_empty(),
        "the action worker never saw the target"
    );
}

#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at06_at31_low_water_after_prepare_refuses_the_intent_and_recovers() {
    let _serial = serial();
    let free_space = AdjustableFreeSpace::plenty();
    let mut rig = ActionRig::start(&action_config(), free_space.clone(), Gate::open());
    rig.wait_until_actions_available();
    let token = rig.prepare();

    // The disk crosses the low-water mark between prepare and confirm: the
    // real repository refuses the intent, so the signal is never sent.
    free_space.drop_below_low_water();
    let refused = rig.manager.submit(CommandBody::ConfirmSignal {
        confirmation_token: token,
    });
    let result = rig.manager.result_for(refused, RESULT_WAIT);
    assert_eq!(result.status, CommandStatus::Rejected);
    assert_eq!(result.code, ResultCode::from(ErrorCode::PersistFailed));
    assert!(
        result.message.contains("audit intent not durable"),
        "{result:?}"
    );
    assert!(result.message.contains("signal not sent"));
    wait_until("the held descriptor to be released", RESULT_WAIT, || {
        rig.actions
            .calls()
            .iter()
            .any(|call| matches!(call, ActionCall::Release(_)))
    });
    assert_eq!(rig.actions.sends(), 0);

    // Space is back: the Manager re-checks the audit and actions work again.
    free_space.restore();
    rig.wait_until_actions_available();
    let token = rig.prepare();
    let sent = rig.manager.submit(CommandBody::ConfirmSignal {
        confirmation_token: token,
    });
    let result = rig.manager.result_for(sent, RESULT_WAIT);
    assert_eq!(result.status, CommandStatus::Completed, "{result:?}");
    assert_eq!(result.action_state, Some(ActionState::SignalSent));
    assert_eq!(rig.actions.sends(), 1);

    // The durable trail: no intent for the refused confirmation, a complete
    // one for the sent signal.
    let _ = rig.manager.shut_down("test done", Duration::from_secs(4));
    rig.workers.stop_all(Duration::from_secs(2));
    let mut audit = rig.reopen_audit();
    assert_eq!(audit.lookup(refused).expect("lookup"), None);
    let summary = audit
        .lookup(sent)
        .expect("lookup")
        .expect("intent recorded");
    // The bounded shutdown may end the exit watch first (`still_running`).
    assert!(
        matches!(
            summary.state,
            ActionState::SignalSent | ActionState::ExitObserved | ActionState::StillRunning
        ),
        "{summary:?}"
    );
}

#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at06_at39_stuck_audit_times_out_the_intent_and_its_late_receipt_never_sends() {
    let _serial = serial();
    let mut config = action_config();
    config.runtime.audit_intent_deadline_ms = 300;
    let intent_gate = Gate::open();
    let mut rig = ActionRig::start(&config, AdjustableFreeSpace::plenty(), intent_gate.clone());
    rig.wait_until_actions_available();
    let token = rig.prepare();

    intent_gate.close();
    let confirm = rig.manager.submit(CommandBody::ConfirmSignal {
        confirmation_token: token,
    });
    wait_until("the audit worker to hang in storage", RESULT_WAIT, || {
        intent_gate.blocked() == 1
    });
    // The control loop keeps serving while the audit worker hangs.
    let responsive = Instant::now();
    for view in [ViewKind::Overview, ViewKind::GpuDetail, ViewKind::Settings] {
        rig.manager.show(view);
    }
    assert!(responsive.elapsed() < Duration::from_secs(1));
    let result = rig.manager.result_for(confirm, RESULT_WAIT);
    assert_eq!(result.status, CommandStatus::Rejected);
    assert_eq!(
        result.code,
        ResultCode::from(ErrorCode::TimedOut),
        "{result:?}"
    );
    assert!(result.message.contains("signal not sent"));

    // The storage answers again: the intent commits late and its durable
    // receipt reaches the Manager — which must not send any more.
    intent_gate.release();
    wait_until("the audit worker to drain", RESULT_WAIT, || {
        intent_gate.blocked() == 0 && rig.audit.queued() == 0
    });
    // Control completions are handled before commands, so once this
    // navigation is answered the late receipt has been processed.
    rig.manager.show(ViewKind::Overview);
    assert_eq!(rig.actions.sends(), 0, "a late receipt never sends");
    assert!(
        rig.actions
            .calls()
            .iter()
            .any(|call| matches!(call, ActionCall::Release(_)))
    );

    let _ = rig.manager.shut_down("test done", Duration::from_secs(4));
    rig.workers.stop_all(Duration::from_secs(2));
    let summary = rig
        .reopen_audit()
        .lookup(confirm)
        .expect("lookup")
        .expect("the late intent is on disk");
    // `not_sent` outcomes are stored under the terminal `rejected` status.
    assert_eq!(
        summary.state,
        ActionState::Rejected,
        "the durable trail closes the late intent without a send: {summary:?}"
    );
    assert_eq!(summary.result_code, ErrorCode::TimedOut.as_str());
}
