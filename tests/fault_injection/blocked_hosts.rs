//! AT24 (blocked host and quarantine bounds) with real child processes:
//! two `dgxtop --adapter-host` replay children — a GPU backend (the demo
//! fixture's `nvml.*` groups) and a host backend (CPU, memory, network, ...)
//! — under the real supervisor, Manager, collectors and HotStore. The GPU
//! host's reads hang inside the adapter call (replay `hang` fault), i.e. the
//! whole host stops answering like an NVML call stuck in the driver.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::commands::ViewKind;
use dgxtop_core::diagnostics::names;
use dgxtop_core::view::{CellState, HostState};
use dgxtop_runtime::clock::{BootClock, host_identity};
use dgxtop_runtime::supervisor::StopOutcome;

use crate::support::hosts::{
    UnreapableLauncher, adapter_id, child_manifest, child_replay_host, hang_from_cycle,
    split_demo_by_backend, write_fixture,
};
use crate::support::probes::{
    TempDir, is_unreaped_child, open_descriptor_count, rig_threads, serial, settles_to, wait_until,
};
use crate::support::telemetry_rig::{TelemetryPlan, TelemetryRig, fast_sampling_config};

const GPU_HOST: &str = "replay-1";
const SYSTEM_HOST: &str = "replay-2";
/// Reads with cycle id 1..=9 succeed, every later read hangs.
const FIRST_HUNG_CYCLE: u32 = 10;
/// Longer than any scenario: the host never answers on its own.
const HANG: Duration = Duration::from_secs(30);
const STARTUP_WAIT: Duration = Duration::from_secs(10);
/// Production circuit backoff base (1 s) minus its ±10 % jitter.
const MIN_BACKOFF: Duration = Duration::from_millis(900);

/// The two replay children, the GPU one with hanging reads.
fn hung_gpu_plan(dir: &TempDir) -> TelemetryPlan {
    let fixtures = split_demo_by_backend();
    let gpu = write_fixture(
        dir,
        "gpu.json",
        &hang_from_cycle(fixtures.gpu, FIRST_HUNG_CYCLE, HANG),
    );
    let system = write_fixture(dir, "system.json", &fixtures.host);
    let clock: Arc<dyn TimeProvider> = Arc::new(BootClock::new().expect("boot clock"));
    // The children stamp their records with this host's identity.
    let host_id = host_identity(Path::new("/")).expect("host identity");
    TelemetryPlan::new(
        fast_sampling_config(),
        clock,
        host_id,
        vec![
            (child_replay_host(GPU_HOST, &gpu), child_manifest(GPU_HOST)),
            (
                child_replay_host(SYSTEM_HOST, &system),
                child_manifest(SYSTEM_HOST),
            ),
        ],
    )
}

fn wait_for_both_backends(rig: &TelemetryRig) {
    wait_until("GPU and CPU data to be committed", STARTUP_WAIT, || {
        rig.domain_committed_ns("gpu").is_some() && rig.domain_committed_ns("cpu").is_some()
    });
}

/// What one observation window saw.
#[derive(Debug, Default)]
struct Window {
    /// `(generation, pid)` of every GPU-host incarnation seen, in order.
    incarnations: Vec<(u32, Option<i32>)>,
    /// A new generation appeared while its predecessor was still unreaped.
    successor_before_reap: Vec<(u32, i32)>,
    cpu_commits: Vec<u64>,
    gpu_commit_changes: u32,
    system_host_problems: Vec<String>,
}

fn observe(rig: &TelemetryRig, length: Duration) -> Window {
    let mut window = Window::default();
    let mut last_gpu_commit = rig.domain_committed_ns("gpu");
    let deadline = Instant::now() + length;
    while Instant::now() < deadline {
        let gpu = rig.host(GPU_HOST);
        let previous = window.incarnations.last().copied();
        if previous.is_none_or(|(generation, _)| generation != gpu.generation) {
            if let Some((generation, Some(pid))) = previous
                && is_unreaped_child(pid)
            {
                window.successor_before_reap.push((generation, pid));
            }
            window.incarnations.push((gpu.generation, gpu.pid));
        } else if let Some(last) = window.incarnations.last_mut()
            && last.1.is_none()
        {
            last.1 = gpu.pid;
        }
        let system = rig.host(SYSTEM_HOST);
        if system.state != HostState::Healthy || system.generation != 1 {
            window
                .system_host_problems
                .push(format!("{:?} gen {}", system.state, system.generation));
        }
        if let Some(committed) = rig.domain_committed_ns("cpu")
            && window.cpu_commits.last() != Some(&committed)
        {
            window.cpu_commits.push(committed);
        }
        let gpu_commit = rig.domain_committed_ns("gpu");
        if gpu_commit != last_gpu_commit {
            window.gpu_commit_changes += 1;
            last_gpu_commit = gpu_commit;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    window
}

#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at24_hung_gpu_host_is_isolated_restarted_only_after_reap_and_leaks_nothing() {
    let _serial = serial();
    let descriptors_before = open_descriptor_count();
    let rig_threads_before = rig_threads().len();
    {
        let dir = TempDir::new("hung-host");
        let mut rig = TelemetryRig::start(hung_gpu_plan(&dir));
        wait_for_both_backends(&rig);
        // The leak check below counts exactly these threads.
        assert!(
            rig_threads().len() > rig_threads_before,
            "{:?}",
            rig_threads()
        );
        wait_until("the hung GPU host to be restarted", STARTUP_WAIT, || {
            rig.host(GPU_HOST).restarts_total >= 1
        });
        let first_restart = Instant::now();
        let window = observe(&rig, Duration::from_secs(3));

        // Only the hung backend is affected.
        assert!(window.system_host_problems.is_empty(), "{window:?}");
        assert!(
            window.cpu_commits.len() >= 5,
            "CPU data kept updating: {window:?}"
        );
        assert_eq!(
            window.gpu_commit_changes, 0,
            "no GPU data while its host hangs"
        );
        let counter =
            |metric: &'static str, adapter: &str| rig.diagnostics.counter(metric, adapter);
        assert!(counter(names::ADAPTER_TIMEOUT_TOTAL, GPU_HOST) >= 1);
        assert!(counter(names::GAP_RECORDED_TOTAL, GPU_HOST) >= 1);
        assert_eq!(counter(names::ADAPTER_TIMEOUT_TOTAL, SYSTEM_HOST), 0);
        assert_eq!(counter(names::GAP_RECORDED_TOTAL, SYSTEM_HOST), 0);
        let projection = rig.manager.projection_where(
            "the GPU host counted as unhealthy",
            STARTUP_WAIT,
            |model| model.header.hosts_total == 2 && model.header.hosts_healthy == 1,
        );
        assert_eq!(projection.header.hosts_healthy, 1);

        // A new generation only after the previous child was reaped, and
        // restarts are paced by the circuit backoff (no respawn loop).
        assert!(
            window.successor_before_reap.is_empty(),
            "new generation before reap: {window:?}"
        );
        let gpu = rig.host(GPU_HOST);
        assert!(gpu.generation >= 2, "{gpu:?}");
        let paced = first_restart.elapsed().as_millis() / MIN_BACKOFF.as_millis() + 2;
        assert!(
            u128::from(gpu.restarts_total) <= paced,
            "{} restarts in {:?}",
            gpu.restarts_total,
            first_restart.elapsed()
        );

        // Bounded teardown: every child reaped, the hung one by signal.
        let teardown = rig.tear_down(Duration::from_secs(3));
        assert!(teardown.hosts.completed_within_deadline, "{teardown:?}");
        assert!(teardown.hosts.unreaped().is_empty(), "{teardown:?}");
        assert!(teardown.hosts_elapsed < Duration::from_millis(3_500));
        assert!(teardown.manager_elapsed < Duration::from_secs(1));
        assert!(!teardown.manager.timed_out, "{:?}", teardown.manager);
        for (name, report) in &teardown.workers {
            assert_eq!(report.detached, 0, "{name} stopped cleanly");
        }
        for host in &teardown.hosts.hosts {
            assert!(
                matches!(
                    host.outcome,
                    StopOutcome::Reaped { .. } | StopOutcome::NotRunning
                ),
                "{host:?}"
            );
        }
    }
    // Restarts leaked no pipe, descriptor or thread.
    let descriptors_after = settles_to(
        descriptors_before,
        Duration::from_secs(3),
        open_descriptor_count,
    );
    assert!(
        descriptors_after <= descriptors_before,
        "descriptors {descriptors_before} -> {descriptors_after}"
    );
    let rig_threads_after = settles_to(rig_threads_before, Duration::from_secs(3), || {
        rig_threads().len()
    });
    assert!(
        rig_threads_after <= rig_threads_before,
        "threads left behind: {:?}",
        rig_threads()
    );
}

#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at24_at39_unreapable_host_is_quarantined_never_respawned_and_shutdown_stays_bounded() {
    let _serial = serial();
    let dir = TempDir::new("unreapable-host");
    let launcher = UnreapableLauncher::new(GPU_HOST);
    let mut rig = TelemetryRig::start(hung_gpu_plan(&dir).with_launcher(launcher.clone()));
    wait_for_both_backends(&rig);
    wait_until("the GPU host to be quarantined", STARTUP_WAIT, || {
        rig.host(GPU_HOST).state == HostState::Quarantined
    });
    let quarantined = rig.host(GPU_HOST);
    let pid = quarantined.pid.expect("a quarantined child keeps its pid");
    assert!(is_unreaped_child(pid), "the child really is unreaped");
    assert!(
        rig.supervisor.managed_pids().contains(&pid),
        "an unreaped child still counts toward the host limit"
    );

    // For longer than a backoff period: no replacement child, while the
    // other backend keeps delivering.
    let cpu_before = rig.domain_committed_ns("cpu");
    let deadline = Instant::now() + Duration::from_millis(1_500);
    while Instant::now() < deadline {
        let gpu = rig.host(GPU_HOST);
        assert_eq!(gpu.state, HostState::Quarantined, "{gpu:?}");
        assert_eq!(gpu.generation, quarantined.generation, "never respawned");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(launcher.launches(), 1, "exactly one GPU child was started");
    assert!(rig.domain_committed_ns("cpu") > cpu_before);
    assert_eq!(rig.host(SYSTEM_HOST).state, HostState::Healthy);
    rig.manager.show(ViewKind::Settings);
    rig.manager.projection_where(
        "the quarantine in the settings' data sources",
        STARTUP_WAIT,
        |model| {
            model.header.hosts_healthy == 1
                && model.modules.settings.as_deref().is_some_and(|settings| {
                    settings.hosts.iter().any(|host| {
                        host.adapter_id == GPU_HOST && host.state == HostState::Quarantined
                    })
                })
        },
    );

    // Shutdown reports the unreaped child (the composition root turns this
    // into "adapter host may still be running") instead of waiting forever.
    let teardown = rig.tear_down(Duration::from_secs(1));
    assert!(
        teardown.hosts_elapsed < Duration::from_millis(1_500),
        "{teardown:?}"
    );
    assert_eq!(
        teardown.hosts.unreaped(),
        vec![(adapter_id(GPU_HOST), Some(pid))]
    );
    let system = teardown
        .hosts
        .hosts
        .iter()
        .find(|host| host.adapter_id.as_str() == SYSTEM_HOST)
        .expect("system host reported");
    assert!(
        matches!(system.outcome, StopOutcome::Reaped { .. }),
        "{system:?}"
    );
    drop(rig);
    launcher.reap_withheld_children();
    assert!(!is_unreaped_child(pid));
}

/// A hung backend commits nothing, so its last values must age out: the
/// hot store stamps each value with its stale deadline (`now >= deadline`,
/// STORAGE.md §3) and the projection shows it as stale afterwards (AT24).
#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at24_values_of_a_hung_backend_stop_being_shown_as_current() {
    let _serial = serial();
    let dir = TempDir::new("stale-cells");
    let mut rig = TelemetryRig::start(hung_gpu_plan(&dir));
    wait_for_both_backends(&rig);
    wait_until("the hung GPU host to be restarted", STARTUP_WAIT, || {
        rig.host(GPU_HOST).restarts_total >= 1
    });
    rig.manager.show(ViewKind::Overview);
    // 3 x 200 ms stale window plus the 300 ms read deadline, doubled.
    let stale_after_ms = 2 * (3 * 200 + 300);
    let model = rig.manager.projection_where(
        "GPU cards older than the stale window",
        STARTUP_WAIT,
        |model| {
            model.modules.gpu.as_deref().is_some_and(|gpu| {
                gpu.cards.iter().any(|card| {
                    card.temperature
                        .age_ms
                        .is_some_and(|age| age > stale_after_ms)
                })
            })
        },
    );
    let cards = &model.modules.gpu.as_deref().expect("checked").cards;
    for card in cards {
        assert_ne!(
            card.temperature.state,
            CellState::Ok,
            "{} shown as current {} ms after its host stopped answering",
            card.label,
            card.temperature.age_ms.unwrap_or_default()
        );
    }
    let _ = rig.tear_down(Duration::from_secs(3));
}
