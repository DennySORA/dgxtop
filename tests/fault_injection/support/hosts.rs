//! Adapter hosts for the scenarios. Nothing reads real hardware: every host
//! is a child process of the real `dgxtop` binary in `--adapter-host` mode
//! serving the shipped replay fixture (split per backend, with injected
//! `hang` faults).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use dgxtop_adapters::replay::{FaultKind, FixtureFault, FixtureSource, ReplayFixture};
use dgxtop_core::errors::InfraError;
use dgxtop_core::identity::AdapterId;
use dgxtop_core::observation::AdapterManifest;
use dgxtop_runtime::supervisor::{
    ChildControl, ExitSummary, HostLaunch, HostLauncher, HostSpec, LaunchRequest, LaunchedHost,
    SignalDelivery, StandardLauncher,
};

use super::probes::TempDir;

/// The adapter host binary built for this test run.
const DGXTOP_BINARY: &str = env!("CARGO_BIN_EXE_dgxtop");
/// Read groups served by the GPU backend.
const GPU_GROUP_PREFIX: &str = "nvml.";
const GPU_ENTITY_PREFIX: &str = "gpu:";

fn demo_fixture() -> ReplayFixture {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../adapters/fixtures/replay/demo.json");
    ReplayFixture::load(&path).expect("the shipped demo fixture is valid")
}

/// The demo fixture split into two independent backends.
pub struct BackendFixtures {
    /// Every `nvml.*` read group: the GPU backend (an "NVML host").
    pub gpu: ReplayFixture,
    /// CPU, memory, disk, network, thermal, ...: everything else.
    pub host: ReplayFixture,
}

pub fn split_demo_by_backend() -> BackendFixtures {
    let demo = demo_fixture();
    let is_gpu = |source: &FixtureSource| source.read_group.starts_with(GPU_GROUP_PREFIX);
    BackendFixtures {
        gpu: restrict(&demo, &is_gpu, true),
        host: restrict(&demo, &|source: &FixtureSource| !is_gpu(source), false),
    }
}

fn restrict(
    fixture: &ReplayFixture,
    keep: &dyn Fn(&FixtureSource) -> bool,
    gpu_entities: bool,
) -> ReplayFixture {
    let sources: Vec<FixtureSource> = fixture
        .sources
        .iter()
        .filter(|source| keep(source))
        .cloned()
        .collect();
    let kept: BTreeSet<_> = sources
        .iter()
        .map(|source| source.source_id.clone())
        .collect();
    let cycles = fixture
        .cycles
        .iter()
        .map(|cycle| {
            cycle
                .iter()
                .filter(|sample| kept.contains(&sample.source_id))
                .cloned()
                .collect()
        })
        .collect();
    let entities = fixture
        .entities
        .iter()
        .filter(|entity| entity.entity_id.as_str().starts_with(GPU_ENTITY_PREFIX) == gpu_entities)
        .cloned()
        .collect();
    let restricted = ReplayFixture {
        sources,
        cycles,
        entities,
        faults: Vec::new(),
        ..fixture.clone()
    };
    restricted.validate().expect("restricted fixture is valid");
    restricted
}

/// Every read of a fixture cycle `>= first_cycle` sleeps `hang` inside the
/// adapter call: the whole (single-threaded) host stops answering, like an
/// NVML call stuck in the driver. Cycle ids start at 1 per host, so the
/// first `first_cycle - 1` reads still succeed.
pub fn hang_from_cycle(
    mut fixture: ReplayFixture,
    first_cycle: u32,
    hang: Duration,
) -> ReplayFixture {
    let cycles = u32::try_from(fixture.cycles.len()).expect("cycle count fits u32");
    let ms = u32::try_from(hang.as_millis()).expect("hang fits u32 ms");
    fixture.faults = (first_cycle..cycles)
        .map(|cycle| FixtureFault {
            cycle,
            read_group: None,
            kind: FaultKind::Hang { ms },
        })
        .collect();
    fixture.validate().expect("fault fixture is valid");
    fixture
}

pub fn write_fixture(dir: &TempDir, name: &str, fixture: &ReplayFixture) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, fixture.to_json_bytes().expect("encode fixture")).expect("write fixture");
    path
}

pub fn adapter_id(name: &str) -> AdapterId {
    AdapterId::new(name).expect("adapter id")
}

/// Manifest of a replay host running as a persistent child process.
pub fn child_manifest(adapter: &str) -> AdapterManifest {
    dgxtop_adapters::replay::manifest(&adapter_id(adapter), false)
}

/// The real `dgxtop` binary as a managed adapter host serving `fixture`.
pub fn child_replay_host(adapter: &str, fixture: &Path) -> HostSpec {
    HostSpec {
        adapter_id: adapter_id(adapter),
        launch: HostLaunch::ChildProcess {
            program: PathBuf::from(DGXTOP_BINARY),
            args: vec![
                "--adapter-host".to_owned(),
                adapter.to_owned(),
                "--replay-fixture".to_owned(),
                fixture.display().to_string(),
            ],
        },
    }
}

/// Launcher whose control of one adapter never confirms a reap: from the
/// supervisor's point of view the child is stuck in uninterruptible sleep
/// (D-state cannot be provoked without root). Signals still reach the real
/// child, which the test reaps itself afterwards.
pub struct UnreapableLauncher {
    adapter: AdapterId,
    launches: Arc<AtomicU32>,
    pids: Arc<Mutex<Vec<i32>>>,
}

impl UnreapableLauncher {
    pub fn new(adapter: &str) -> Arc<Self> {
        Arc::new(Self {
            adapter: adapter_id(adapter),
            launches: Arc::new(AtomicU32::new(0)),
            pids: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Incarnations started for the unreapable adapter.
    pub fn launches(&self) -> u32 {
        self.launches.load(Ordering::SeqCst)
    }

    pub fn pids(&self) -> Vec<i32> {
        self.pids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Reaps the children whose reap the fake control withheld. They are
    /// children of this test (never anyone else's process).
    pub fn reap_withheld_children(&self) {
        for pid in self.pids() {
            reap_own_child(pid);
        }
    }
}

impl HostLauncher for UnreapableLauncher {
    fn launch(&self, request: &LaunchRequest<'_>) -> Result<LaunchedHost, InfraError> {
        let mut launched = StandardLauncher.launch(request)?;
        if request.adapter_id == &self.adapter {
            self.launches.fetch_add(1, Ordering::SeqCst);
            if let Some(pid) = launched.control.pid() {
                self.pids
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(pid);
            }
            launched.control = Box::new(NeverReaped {
                inner: launched.control,
            });
        }
        Ok(launched)
    }
}

struct NeverReaped {
    inner: Box<dyn ChildControl>,
}

impl ChildControl for NeverReaped {
    fn pid(&self) -> Option<i32> {
        self.inner.pid()
    }

    fn terminate(&mut self) -> Result<SignalDelivery, InfraError> {
        self.inner.terminate()
    }

    fn kill(&mut self) -> Result<SignalDelivery, InfraError> {
        self.inner.kill()
    }

    fn try_reap(&mut self) -> Result<Option<ExitSummary>, InfraError> {
        Ok(None)
    }
}

/// Reaps `pid`, a child this test started (bounded; kills it first if it
/// is somehow still running so the wait cannot hang).
fn reap_own_child(pid: i32) {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    let child = nix::unistd::Pid::from_raw(pid);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(WaitStatus::StillAlive) => {
                let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
                let _ = waitpid(child, None);
                return;
            }
            _ => return,
        }
    }
}
