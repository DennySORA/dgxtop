//! Process-level probes and small fault doubles shared by every scenario:
//! the suite-wide serial lock, private temporary directories, a free-space
//! probe the test moves across the 64 MiB low-water mark, a gate that turns a
//! worker into "storage stopped answering", bounded polling, and `/proc`
//! counters for descriptor/thread leak checks.

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dgxtop_core::errors::InfraError;
use dgxtop_store::sqlite::FreeSpaceProbe;

const MIB: u64 = 1024 * 1024;
/// Comfortably above every low-water mark.
const PLENTY_OF_SPACE: u64 = 100 * 1024 * MIB;
/// Below the 64 MiB low-water mark of the audit and the archive.
const BELOW_LOW_WATER: u64 = 16 * MIB;
/// A gated worker gives up waiting after this long, so a failed test can
/// never leave a thread blocked for the rest of the run.
const GATE_SAFETY_LIMIT: Duration = Duration::from_secs(20);
/// Poll step of [`wait_until`].
const POLL_STEP: Duration = Duration::from_millis(5);

/// Serializes the scenarios: they count process-wide descriptors and
/// threads and spawn child processes, so two of them must never overlap.
pub fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Private (0700) directory under the system temp dir, removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dgxtop-faults-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("restrict temp dir");
        Self { path }
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Free-space probe whose answer the test controls (the product seam for
/// the 64 MiB low-water mark; a real full disk needs root to provoke).
#[derive(Debug)]
pub struct AdjustableFreeSpace(AtomicU64);

impl AdjustableFreeSpace {
    pub fn plenty() -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(PLENTY_OF_SPACE)))
    }

    pub fn below_low_water() -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(BELOW_LOW_WATER)))
    }

    pub fn drop_below_low_water(&self) {
        self.0.store(BELOW_LOW_WATER, Ordering::SeqCst);
    }

    pub fn restore(&self) {
        self.0.store(PLENTY_OF_SPACE, Ordering::SeqCst);
    }
}

impl FreeSpaceProbe for AdjustableFreeSpace {
    fn available_bytes(&self, _path: &Path) -> Result<u64, InfraError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

#[derive(Debug, Default)]
struct GateState {
    closed: bool,
    blocked: usize,
}

/// A latch a worker passes before touching storage. Closed, it models a
/// disk that stopped answering: the worker blocks (never longer than
/// [`GATE_SAFETY_LIMIT`]) until the test releases it.
#[derive(Clone, Default)]
pub struct Gate {
    shared: Arc<(Mutex<GateState>, Condvar)>,
}

impl fmt::Debug for Gate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Gate")
            .field("closed", &self.lock().closed)
            .field("blocked", &self.lock().blocked)
            .finish()
    }
}

impl Gate {
    pub fn open() -> Self {
        Self::default()
    }

    pub fn closed() -> Self {
        let gate = Self::default();
        gate.close();
        gate
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.shared.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn close(&self) {
        self.lock().closed = true;
    }

    pub fn release(&self) {
        self.lock().closed = false;
        self.shared.1.notify_all();
    }

    /// Threads currently stuck at the gate.
    pub fn blocked(&self) -> usize {
        self.lock().blocked
    }

    /// Returns at once while open; otherwise waits for [`Gate::release`].
    pub fn pass(&self) {
        let deadline = Instant::now() + GATE_SAFETY_LIMIT;
        let mut state = self.lock();
        state.blocked += 1;
        while state.closed {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            state = self
                .shared
                .1
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.blocked -= 1;
    }
}

/// Polls `condition` until it holds; panics with `what` after `timeout`.
pub fn wait_until(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}"
        );
        std::thread::sleep(POLL_STEP);
    }
}

/// Open descriptors of this test process.
pub fn open_descriptor_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("read /proc/self/fd")
        .count()
}

/// Thread-name prefixes of everything a rig starts: the Manager thread, the
/// runtime's pools/actors (`<pool>-<index>`) and the supervisor thread.
/// libtest's own threads are named after tests (`module::test`) and are
/// deliberately not counted: they come and go as tests are scheduled.
const RIG_THREAD_PREFIXES: [&str; 9] = [
    "fault-manager",
    "dgxtop-supervis",
    "hot-store-",
    "collector-",
    "history-",
    "config-",
    "audit-",
    "action-",
    "archive-",
];

fn thread_names() -> Vec<String> {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("comm")).ok())
        .map(|comm| comm.trim_end().to_owned())
        .collect()
}

/// Threads started by a rig that are still alive.
pub fn rig_threads() -> Vec<String> {
    thread_names()
        .into_iter()
        .filter(|name| {
            RIG_THREAD_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
        })
        .collect()
}

/// Whether a thread of this process carries `name` (as set by the runtime's
/// pools: `<pool>-<index>`).
pub fn thread_exists(name: &str) -> bool {
    thread_names().iter().any(|thread| thread == name)
}

pub fn own_pid() -> i32 {
    i32::try_from(std::process::id()).expect("pid fits i32")
}

/// Parent of `pid`, or `None` once the process is gone (reaped).
pub fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    stat.get(close + 1..)?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Whether `pid` is still an unreaped child (alive or zombie) of this test.
pub fn is_unreaped_child(pid: i32) -> bool {
    parent_of(pid) == Some(own_pid())
}

/// Waits (bounded) until `current()` is back at or below `baseline`; pipes
/// and threads of a stopped rig close asynchronously.
pub fn settles_to(baseline: usize, timeout: Duration, current: impl Fn() -> usize) -> usize {
    let deadline = Instant::now() + timeout;
    loop {
        let now = current();
        if now <= baseline || Instant::now() >= deadline {
            return now;
        }
        std::thread::sleep(POLL_STEP);
    }
}
