//! Process-tree and managed-memory sampling at ≤ 1 s intervals
//! (`PERFORMANCE.md` §2): parent + every adapter host (`ProcessTreeStats`
//! over `[own pid] + SupervisorHandle::managed_pids()`), the byte ledger,
//! and the CPU the harness threads spent (to be excluded and reported).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dgxtop::perf::threads::{HarnessCpu, ThreadTicks, process_thread_ticks};
use dgxtop::perf::tree_stats::ProcessTreeStats;
use dgxtop_core::budget::ByteLedger;
use dgxtop_runtime::supervisor::SupervisorHandle;
use serde::Serialize;

use super::shadow::history_bytes;
use crate::common::BenchResult;

/// One sample of the measured process tree.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceSample {
    pub at_s: f64,
    /// utime+stime(+cutime+cstime of the parent) summed over the tree.
    pub cpu_ticks: u64,
    /// CPU ticks of the harness threads so far (excluded from dgxtop CPU).
    pub harness_ticks: u64,
    pub rss_bytes: u64,
    pub threads: u64,
    pub open_fds: u64,
    pub pids: usize,
    pub complete: bool,
    pub managed_bytes: u64,
    pub shadow_history_bytes: u64,
    pub per_pid: Vec<PidUsage>,
}

/// RSS and CPU ticks of one process of the tree.
#[derive(Debug, Clone, Serialize)]
pub struct PidUsage {
    pub pid: i32,
    pub rss_bytes: Option<u64>,
    pub cpu_ticks: Option<u64>,
}

/// What the sampler thread needs.
pub struct SamplerInputs {
    pub supervisor: SupervisorHandle,
    pub ledger: ByteLedger,
    /// Ledger of the preloaded history store, when one serves the queries.
    pub shadow_ledger: Option<ByteLedger>,
    pub harness: HarnessCpu,
    pub interval: Duration,
}

/// Samples of a window plus the parent's threads at its start and end.
#[derive(Debug, Default)]
pub struct SampledWindow {
    pub samples: Vec<ResourceSample>,
    pub threads_at_start: Vec<ThreadTicks>,
    pub threads_at_end: Vec<ThreadTicks>,
}

pub struct Sampler {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<SampledWindow>>,
}

impl Sampler {
    /// Samples at once, then every `interval`, and once more when stopped.
    pub fn start(inputs: SamplerInputs) -> BenchResult<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("bench-sampler".to_owned())
            .spawn(move || sample_until(&inputs, &flag))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub fn finish(mut self) -> SampledWindow {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .and_then(|thread| thread.join().ok())
            .unwrap_or_default()
    }
}

fn sample_until(inputs: &SamplerInputs, stop: &AtomicBool) -> SampledWindow {
    let slot = inputs.harness.register();
    let origin = Instant::now();
    let mut window = SampledWindow {
        threads_at_start: process_thread_ticks(),
        ..SampledWindow::default()
    };
    let mut next = origin;
    loop {
        let stopping = stop.load(Ordering::Acquire);
        let now = Instant::now();
        if now >= next || stopping {
            window.samples.push(take_sample(inputs, origin));
            slot.refresh();
            next += inputs.interval;
        }
        if stopping {
            window.threads_at_end = process_thread_ticks();
            return window;
        }
        std::thread::sleep(
            next.saturating_duration_since(Instant::now())
                .min(Duration::from_millis(20)),
        );
    }
}

fn take_sample(inputs: &SamplerInputs, origin: Instant) -> ResourceSample {
    let own = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    let mut pids = vec![own];
    pids.extend(inputs.supervisor.managed_pids());
    let tree = ProcessTreeStats::sample(&pids);
    ResourceSample {
        at_s: origin.elapsed().as_secs_f64(),
        cpu_ticks: tree.cpu_ticks,
        harness_ticks: inputs.harness.total_ticks(),
        rss_bytes: tree.rss_bytes,
        threads: tree.threads,
        open_fds: tree.open_fds,
        pids: pids.len(),
        complete: tree.complete,
        managed_bytes: inputs.ledger.snapshot().used,
        shadow_history_bytes: inputs.shadow_ledger.as_ref().map_or(0, history_bytes),
        per_pid: tree
            .per_pid
            .iter()
            .map(|pid| PidUsage {
                pid: pid.pid,
                rss_bytes: pid.rss_bytes,
                cpu_ticks: pid.cpu_ticks,
            })
            .collect(),
    }
}
