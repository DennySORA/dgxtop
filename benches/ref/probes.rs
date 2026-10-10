//! Measurement sinks shared by the instrumented composition, the headless
//! frontend and the workload drivers. Product threads only append to them
//! under a short lock after doing their real work; nothing here changes
//! what the product does or decides.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dgxtop::perf::coverage::{COMMIT_GRACE_NS, CoverageLedger, CoverageReport, ScheduledGroup};
use dgxtop::perf::latency::{PendingRequests, ProjectionLatencyTracker, duration_us};
use dgxtop_core::canonical::{CollectionBatch, StoreAck};
use dgxtop_core::errors::{ErrorCode, ErrorDetail};
use dgxtop_core::history::HistoryResult;
use dgxtop_core::observation::BatchStatus;
use serde::Serialize;
use uuid::Uuid;

/// Revisions waiting for a projection / history jobs waiting for a result.
const PENDING_CAPACITY: usize = 65_536;
/// Poll step while the coverage of a finished window settles.
const SETTLE_POLL: Duration = Duration::from_millis(50);

/// The adapter read behind one input of a collection batch.
#[derive(Debug, Clone)]
pub struct ReadSummary {
    adapter_id: String,
    read_group: String,
    cycle_id: u64,
    finished_ns: u64,
    delivered: bool,
    status: String,
    records: usize,
}

/// Reads consumed by a batch, captured before the batch moves into the store.
pub fn reads_of(batch: &CollectionBatch) -> Vec<ReadSummary> {
    batch
        .inputs
        .iter()
        .map(|input| ReadSummary {
            adapter_id: input.adapter_id.to_string(),
            read_group: input.read_group.as_str().to_owned(),
            cycle_id: input.cycle_id,
            finished_ns: input.finished_boot_ns,
            delivered: input.status != BatchStatus::Failed,
            status: format!("{:?}", input.status),
            records: input.records.len(),
        })
        .collect()
}

#[derive(Debug)]
struct CommitProbe {
    tracker: ProjectionLatencyTracker,
    coverage: CoverageLedger,
    committed: u64,
    rejected: u64,
    /// Committed inputs per `adapter group status` during the window.
    read_census: BTreeMap<String, ReadCensus>,
}

/// Committed inputs of one adapter group and batch status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReadCensus {
    pub batches: u64,
    pub records: u64,
}

#[derive(Debug)]
struct HistoryProbe {
    pending: PendingRequests<Uuid>,
    latencies_us: Vec<u64>,
    outcomes: BTreeMap<String, u64>,
    max_points: usize,
}

/// History query outcomes of a window.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HistoryWindow {
    pub latencies_us: Vec<u64>,
    pub outcomes: BTreeMap<String, u64>,
    pub max_points: usize,
    pub unanswered: usize,
}

/// Projection-latency samples of a window.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectionWindow {
    pub latencies_us: Vec<u64>,
    pub unresolved_revisions: usize,
    pub evicted_revisions: u64,
    pub clock_skew: u64,
    pub committed_batches: u64,
    pub rejected_batches: u64,
    pub read_census: BTreeMap<String, ReadCensus>,
}

#[derive(Debug)]
pub struct Probes {
    measuring: AtomicBool,
    commits: Mutex<CommitProbe>,
    history: Mutex<HistoryProbe>,
    /// Completions the composition could not hand to the Manager in time.
    pub completion_drops: AtomicU64,
    pub collector_panics: AtomicU64,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Probes {
    pub fn new() -> Self {
        Self {
            measuring: AtomicBool::new(false),
            commits: Mutex::new(CommitProbe {
                tracker: ProjectionLatencyTracker::with_capacity(PENDING_CAPACITY),
                coverage: CoverageLedger::default(),
                committed: 0,
                rejected: 0,
                read_census: BTreeMap::new(),
            }),
            history: Mutex::new(HistoryProbe {
                pending: PendingRequests::with_capacity(PENDING_CAPACITY),
                latencies_us: Vec::new(),
                outcomes: BTreeMap::new(),
                max_points: 0,
            }),
            completion_drops: AtomicU64::new(0),
            collector_panics: AtomicU64::new(0),
        }
    }

    pub fn is_measuring(&self) -> bool {
        self.measuring.load(Ordering::Acquire)
    }

    /// Store actor: a collection batch was committed or rejected.
    pub fn on_commit(&self, reads: &[ReadSummary], ack: &StoreAck) {
        let mut probe = locked(&self.commits);
        if !ack.status.is_committed() {
            probe.rejected += 1;
            return;
        }
        probe.committed += 1;
        if self.is_measuring() {
            for read in reads {
                let base = dgxtop_core::observation::split_page(&read.read_group).0;
                let census = probe
                    .read_census
                    .entry(format!("{} {base} {}", read.adapter_id, read.status))
                    .or_default();
                census.batches += 1;
                census.records += u64::try_from(read.records).unwrap_or(u64::MAX);
            }
        }
        if let Some(newest) = reads.iter().map(|read| read.finished_ns).max() {
            probe.tracker.committed(ack.committed_revision, newest);
        }
        for read in reads.iter().filter(|read| read.delivered) {
            probe.coverage.record(
                &read.adapter_id,
                &read.read_group,
                read.cycle_id,
                read.finished_ns,
            );
        }
    }

    /// Frontend: the Manager published a projection of `snapshot_revision`.
    pub fn on_projection(&self, snapshot_revision: u64, generated_boot_ns: u64) {
        locked(&self.commits)
            .tracker
            .published(snapshot_revision, generated_boot_ns);
    }

    /// Manager → history pool: a job was handed over at `at`.
    pub fn on_history_dispatch(&self, request_id: Uuid, at: Instant) {
        locked(&self.history).pending.start(request_id, at);
    }

    /// The pool refused a job; it will never produce a result.
    pub fn on_history_refused(&self, request_id: &Uuid) {
        locked(&self.history)
            .pending
            .finish(request_id, Instant::now());
    }

    /// History worker: the query finished. Completed results and deadline
    /// timeouts are latency samples; cancellations (the session left the
    /// view) and other failures are counted, not sampled.
    pub fn on_history_result(
        &self,
        request_id: &Uuid,
        result: &Result<HistoryResult, ErrorDetail>,
    ) {
        let finished = Instant::now();
        let measuring = self.is_measuring();
        let mut probe = locked(&self.history);
        let Some(elapsed) = probe.pending.finish(request_id, finished) else {
            return;
        };
        if !measuring {
            return;
        }
        let (label, sampled) = match result {
            Ok(result) => {
                probe.max_points = probe.max_points.max(result.points.len());
                ("ok".to_owned(), true)
            }
            Err(error) => (
                format!("{:?}", error.code),
                error.code == ErrorCode::TimedOut,
            ),
        };
        *probe.outcomes.entry(label).or_insert(0) += 1;
        if sampled {
            probe.latencies_us.push(duration_us(elapsed));
        }
    }

    /// Starts the window: earlier samples and revisions are discarded so
    /// warm-up never leaks into the measurement.
    pub fn begin_window(&self, start_ns: u64, revision_at_start: u64) {
        {
            let mut probe = locked(&self.commits);
            probe.tracker.forget_through(revision_at_start);
            let _warmup = probe.tracker.take_samples();
            probe.coverage.forget_before(start_ns);
            probe.committed = 0;
            probe.rejected = 0;
            probe.read_census.clear();
        }
        {
            let mut probe = locked(&self.history);
            probe.latencies_us.clear();
            probe.outcomes.clear();
            probe.max_points = 0;
        }
        self.measuring.store(true, Ordering::Release);
    }

    /// Ends the window and returns its samples.
    pub fn end_window(&self) -> (ProjectionWindow, HistoryWindow) {
        self.measuring.store(false, Ordering::Release);
        let projection = {
            let mut probe = locked(&self.commits);
            ProjectionWindow {
                latencies_us: probe.tracker.take_samples(),
                unresolved_revisions: probe.tracker.unresolved(),
                evicted_revisions: probe.tracker.evicted(),
                clock_skew: probe.tracker.clock_skew(),
                committed_batches: probe.committed,
                rejected_batches: probe.rejected,
                read_census: std::mem::take(&mut probe.read_census),
            }
        };
        let history = {
            let mut probe = locked(&self.history);
            HistoryWindow {
                latencies_us: std::mem::take(&mut probe.latencies_us),
                outcomes: std::mem::take(&mut probe.outcomes),
                max_points: probe.max_points,
                unanswered: probe.pending.len(),
            }
        };
        (projection, history)
    }

    /// Final coverage of the window `[start_ns, end_ns)`: waits until the
    /// reads that finished inside it had time to commit
    /// (`CoverageLedger::final_report`); the product keeps running meanwhile.
    pub fn final_coverage(
        &self,
        groups: &[ScheduledGroup],
        start_ns: u64,
        end_ns: u64,
        clock_now: impl Fn() -> u64,
    ) -> CoverageReport {
        loop {
            let now_ns = clock_now();
            if let Some(report) = locked(&self.commits)
                .coverage
                .final_report(groups, start_ns, end_ns, now_ns)
            {
                return report;
            }
            let remaining_ns = end_ns
                .saturating_add(COMMIT_GRACE_NS)
                .saturating_sub(now_ns);
            std::thread::sleep(Duration::from_nanos(remaining_ns).min(SETTLE_POLL));
        }
    }

    pub fn coverage(
        &self,
        groups: &[ScheduledGroup],
        start_ns: u64,
        end_ns: u64,
    ) -> CoverageReport {
        locked(&self.commits)
            .coverage
            .report(groups, start_ns, end_ns)
    }

    /// Adapters with a committed read in `[start_ns, end_ns)` (readiness check).
    pub fn adapters_seen(
        &self,
        groups: &[ScheduledGroup],
        start_ns: u64,
        end_ns: u64,
    ) -> BTreeSet<String> {
        self.coverage(groups, start_ns, end_ns)
            .groups
            .into_iter()
            .filter(|group| group.covered_slots > 0)
            .map(|group| group.adapter_id)
            .collect()
    }
}
