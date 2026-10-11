//! AT30 (bounded overload state machine) on the real Manager control loop
//! with a fake clock, so 90 s of recovery take well under a second of real
//! time. The overload controller only sees what the composition root hands
//! it — the shared `ByteLedger` and ingress `QueueGate` — and control work
//! goes through the real config actor while the system is throttled and
//! its telemetry inbox is flooded.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Sender, TrySendError};
use dgxtop::live_store::LiveStore;
use dgxtop_core::budget::{ByteLedger, BytePermit, MIB, Partition, QueueGate, QueueTicket};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::commands::CommandBody;
use dgxtop_core::config::Config;
use dgxtop_core::errors::{ErrorCode, ErrorDetail};
use dgxtop_core::identity::SessionId;
use dgxtop_core::ports::{ConfigRepository, ControlCompletion, IngestionCompletion, Logger};
use dgxtop_core::quality::Namespace;
use dgxtop_core::testing::FakeClock;
use dgxtop_core::testing::MemoryLogger;
use dgxtop_core::view::{HostState, HostStatus, OverloadState, ViewModel};
use dgxtop_manager::{INGESTION_INBOX_CAPACITY, ManagerChannels, ManagerInboxes, ManagerPorts};
use dgxtop_store::config::FileConfigRepository;
use dgxtop_store::fs::RealFileSystem;
use uuid::Uuid;

use crate::support::manager_rig::{
    Identity, RunningManager, inert_ports, ingress_gate, manager_setup,
};
use crate::support::probes::{Gate, TempDir, serial, wait_until};
use crate::support::workers::{
    Completions, WorkerSet, spawn_audit_worker, spawn_config_worker, spawn_history_pool,
    spawn_store_worker,
};

const SECOND_NS: u64 = 1_000_000_000;
const START_NS: u64 = 10_000 * SECOND_NS;
/// Real time allowed for the control loop to publish the next frame.
const FRAME_WAIT: Duration = Duration::from_secs(2);

/// One overload level as the header projects it.
type Level = (OverloadState, u32);

struct OverloadRig {
    manager: RunningManager,
    clock: Arc<FakeClock>,
    ledger: ByteLedger,
    ingress: QueueGate,
    ingress_budget: u64,
    inboxes: ManagerInboxes,
    logger: Arc<MemoryLogger>,
    _workers: WorkerSet,
    _dir: TempDir,
}

impl OverloadRig {
    fn start() -> Self {
        let dir = TempDir::new("overload");
        let mut repository =
            FileConfigRepository::new(dir.join("config.json"), Arc::new(RealFileSystem));
        let config: Config = repository.load().expect("defaults").config;
        let clock = FakeClock::new(START_NS);
        let time: Arc<dyn TimeProvider> = clock.clone();
        let identity = Identity::fault_host(time.as_ref());
        let ledger = ByteLedger::for_memory_limit_mib(u64::from(config.store.memory_limit_mib));
        let ingress = ingress_gate(&config, &ledger);
        let logger = Arc::new(MemoryLogger::default());
        let channels = ManagerChannels::create();
        let inboxes = channels.inboxes();
        let completions = Completions::from_inboxes(&inboxes);
        let (writer, snapshots, history) = LiveStore::new(
            &config.store,
            &config.sampling,
            &config.runtime,
            ledger.clone(),
            Namespace::Live,
            false,
        )
        .into_parts();
        let mut workers = WorkerSet::default();
        let store = spawn_store_worker(
            writer,
            None,
            Arc::clone(&time),
            completions.clone(),
            u64::from(config.queues.archive_bytes),
        );
        workers.track(&store);
        let config_worker = spawn_config_worker(repository, completions.clone());
        workers.track(&config_worker);
        // The product's fail-closed audit stand-in (SQLite gate failed).
        let audit = spawn_audit_worker(
            Box::new(dgxtop::audit_unavailable::UnavailableAudit::new(
                "not under test",
            )),
            Gate::open(),
            completions.clone(),
        );
        workers.track(&audit);
        let history_pool =
            spawn_history_pool(&config, Arc::new(history), Arc::clone(&time), completions);
        workers.track(&history_pool);
        let ports = ManagerPorts {
            store: Box::new(store),
            config: Box::new(config_worker),
            audit: Box::new(audit),
            history: Box::new(history_pool),
            logger: Arc::clone(&logger) as Arc<dyn Logger>,
            ..inert_ports(time, Arc::new(snapshots), &ledger, ingress.clone())
        };
        // A real live manifest gives the scheduler its fast read groups (the
        // header projects their effective period); behind the inert adapter
        // port the host never becomes dispatchable, so nothing is read.
        let procfs = dgxtop_adapters::registry::live_manifest("procfs").expect("procfs manifest");
        let setup = manager_setup(&config, &identity, vec![procfs], Namespace::Live);
        let manager = RunningManager::start(setup, ports, channels);
        Self {
            manager,
            clock,
            ledger,
            ingress,
            ingress_budget: u64::from(config.queues.ingress_bytes),
            inboxes,
            logger,
            _workers: workers,
            _dir: dir,
        }
    }

    fn now(&self) -> u64 {
        self.clock.now_boot_ns()
    }

    /// Advances the fake clock one second and returns the frame built at the
    /// new time. Frames are published after the overload sample of the same
    /// iteration, so the header shows the state decided at that instant.
    fn tick(&self) -> Arc<ViewModel> {
        self.clock.advance_ns(SECOND_NS);
        let now = self.now();
        self.wake_control_loop();
        self.manager
            .projection_where("a frame at the new time", FRAME_WAIT, |model| {
                model.generated_boot_ns >= now
            })
    }

    /// A stale control completion wakes the loop at once instead of after
    /// its idle poll (dropped as stale, `history` label).
    fn wake_control_loop(&self) {
        let _ = self.inboxes.control.try_send(ControlCompletion::History {
            request_id: Uuid::new_v4(),
            session_id: SessionId::new("session-wake").expect("session id"),
            session_generation: 0,
            result: Err(ErrorDetail::new(ErrorCode::Cancelled, "wake")),
        });
    }

    /// Holds `percent` of the ingress byte budget, as in-flight adapter
    /// reads do (backpressure from the hosts, as the Manager sees it).
    fn hold_ingress(&self, percent: u64) -> QueueTicket {
        assert_eq!(self.ingress.queued_bytes(), 0, "nothing else holds ingress");
        self.ingress
            .try_admit(self.ingress_budget * percent / 100)
            .expect("the ingress gate admits up to its budget")
    }

    /// Takes every free byte of the data partitions plus half the reserve:
    /// > 90 % of the managed budget, leaving presentation and command bytes.
    fn hog_memory(&self) -> Vec<BytePermit> {
        let limits = self.ledger.limits();
        let mut hogs: Vec<BytePermit> = Partition::ALL
            .iter()
            .filter(|partition| !matches!(partition, Partition::Presentation | Partition::Reserve))
            .filter_map(|partition| {
                let free = limits.limit(*partition) - self.ledger.used(*partition);
                (free > 0).then(|| {
                    self.ledger
                        .try_reserve(*partition, free)
                        .expect("free bytes can be reserved")
                })
            })
            .collect();
        hogs.push(
            self.ledger
                .try_reserve(Partition::Reserve, 10 * MIB)
                .expect("half the reserve"),
        );
        assert!(self.ledger.total_utilization() >= 0.9);
        hogs
    }

    /// Overload transitions the Manager logged that start with `prefix`
    /// (`overload <from> -> <to> (fast multiplier x<n>)`).
    fn transitions(&self, prefix: &str) -> usize {
        self.logger
            .records()
            .iter()
            .filter(|(_, target, message)| {
                *target == "manager.overload" && message.starts_with(prefix)
            })
            .count()
    }
}

fn level(model: &ViewModel) -> Level {
    (model.header.overload, model.header.effective_interval_ms)
}

/// Telemetry completions pushed into the ingestion inbox as fast as it
/// accepts them (host reports of an unknown adapter: dropped cheaply).
struct Flood {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<u64>,
}

impl Flood {
    fn start(inbox: Sender<IngestionCompletion>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut pushed = 0u64;
            while !running.load(Ordering::Relaxed) {
                match inbox.try_send(IngestionCompletion::HostStatus(ghost_status())) {
                    Ok(()) => pushed += 1,
                    Err(TrySendError::Full(_)) => std::thread::yield_now(),
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            pushed
        });
        Self { stop, thread }
    }

    fn stop(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().expect("flood thread")
    }
}

fn ghost_status() -> HostStatus {
    HostStatus {
        adapter_id: "ghost".to_owned(),
        state: HostState::Healthy,
        generation: 1,
        pid: None,
        last_success_boot_ns: None,
        timeouts_total: 0,
        consecutive_failures: 0,
        restarts_total: 0,
        backoff_until_boot_ns: None,
        detail: None,
    }
}

#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at30_overload_escalates_recovers_one_level_per_30s_and_keeps_control_flowing() {
    let _serial = serial();
    let rig = OverloadRig::start();
    assert_eq!(level(&rig.tick()), (OverloadState::Normal, 1_000));

    // 1. A full ingress gate every other second is never three samples in a
    //    row: no state change at all (no flapping).
    for second in 0..12u32 {
        let held = (second % 2 == 0).then(|| rig.hold_ingress(100));
        assert_eq!(
            level(&rig.tick()),
            (OverloadState::Normal, 1_000),
            "{second}"
        );
        drop(held);
    }

    // 2. Managed memory >= 90 %: throttled (x2) at the next sample, x4 once
    //    the overload persisted for >= 3 samples and >= 3 s.
    let hogs = rig.hog_memory();
    assert_eq!(level(&rig.tick()), (OverloadState::Throttled, 2_000));
    let escalated_after = (1..=6u32)
        .find(|_| level(&rig.tick()) == (OverloadState::Throttled, 4_000))
        .expect("escalated to x4");
    assert!(escalated_after >= 3, "x4 after {escalated_after} s");

    // 3. While throttled and with the telemetry inbox saturated, a control
    //    command still completes through the real config actor.
    let flood = Flood::start(rig.inboxes.ingestion.clone());
    wait_until("the ingestion inbox to saturate", FRAME_WAIT, || {
        rig.inboxes.ingestion.len() * 4 >= INGESTION_INBOX_CAPACITY * 3
    });
    let applied = rig.manager.execute(CommandBody::SetSamplingInterval {
        interval_ms: 2_000,
        expected_config_revision: 1,
    });
    let pushed = flood.stop();
    assert!(applied.code.is_ok(), "{applied:?}");
    assert_eq!(applied.config_revision, Some(2));
    assert!(
        pushed > u64::try_from(INGESTION_INBOX_CAPACITY).expect("fits"),
        "only {pushed} telemetry completions"
    );
    let model = rig.tick();
    assert_eq!(model.header.configured_interval_ms, 2_000);
    assert_eq!(level(&model), (OverloadState::Throttled, 8_000));

    // 4. Healthy again: one level per 30 s of healthy samples, never faster.
    drop(hogs);
    let healthy_from = rig.now();
    let mut last = level(&model);
    let mut steps: Vec<(u64, Level)> = Vec::new();
    for _ in 0..120 {
        let current = level(&rig.tick());
        if current != last {
            steps.push(((rig.now() - healthy_from) / SECOND_NS, current));
            last = current;
        }
        if current.0 == OverloadState::Normal {
            break;
        }
    }
    let levels: Vec<Level> = steps.iter().map(|(_, level)| *level).collect();
    assert_eq!(
        levels,
        vec![
            (OverloadState::Throttled, 4_000),
            (OverloadState::Recovering, 2_000),
            (OverloadState::Normal, 2_000),
        ],
        "{steps:?}"
    );
    let at: Vec<u64> = steps.iter().map(|(seconds, _)| *seconds).collect();
    assert!(
        at[0] >= 30 && at[1] - at[0] >= 30 && at[2] - at[1] >= 30,
        "{steps:?}"
    );
    for transition in [
        "overload normal -> throttled",
        "overload throttled -> throttled (fast multiplier x4)",
        "overload throttled -> throttled (fast multiplier x2)",
        "overload throttled -> recovering",
        "overload recovering -> normal",
    ] {
        assert_eq!(rig.transitions(transition), 1, "{transition}");
    }
    assert_eq!(rig.transitions("overload normal -> pressured"), 0);
}

/// RUNTIME.md §7 enters `pressured` when any queue's bytes stay >= 75 % for
/// 3 samples / 3 s; the byte-accounted ingress gate is one of those queues
/// (it used to count only at >= 95 %, on the throttle path).
#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at30_ingress_bytes_at_75_percent_for_three_samples_enter_pressure() {
    let _serial = serial();
    let rig = OverloadRig::start();
    assert_eq!(level(&rig.tick()), (OverloadState::Normal, 1_000));
    let held = rig.hold_ingress(80);
    let entered_after = (1..=6u32).find(|_| rig.tick().header.overload == OverloadState::Pressured);
    drop(held);
    assert!(
        entered_after.is_some_and(|seconds| seconds >= 3),
        "pressure after {entered_after:?} s of 80 % ingress bytes"
    );
}
