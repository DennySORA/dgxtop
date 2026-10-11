//! The process history (D-76): every GPU process dgxtop saw, as a *run*
//! (one [`ProcessKey`]: a PID with its start time on one boot), with what it
//! ran, where, on which GPUs, when it started and ended and why, and the
//! averages and peaks of what it used over its life.
//!
//! What it used is kept as [`RunBucket`]s of the shared bar sizes
//! ([`crate::tiers`], D-77): each size keeps its latest
//! [`TIER_BUCKETS`](crate::tiers::TIER_BUCKETS) and older time lives in
//! the coarser sizes, so a run costs the same however long it lasts, and
//! its whole life stays at the coarse sizes. The 5 s buckets of the last
//! 10 minutes are kept in a short-term file.

use serde::{Deserialize, Serialize};

use crate::action::Signal;
use crate::identity::{BootId, ProcessKey};
use crate::tiers::Tier;

/// Runs followed at once (live, probed every cycle).
pub const MAX_LIVE_RUNS: usize = crate::process_probe::MAX_PROBED_PROCESSES;
/// Runs the history lists; the oldest ended ones give way.
pub const MAX_LISTED_RUNS: usize = 512;

/// What a run used, in the order of [`RunBucket::values`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RunMetric {
    /// CPU time over wall time, percent of one core.
    Cpu,
    /// Resident memory, bytes.
    Memory,
    /// Utilization summed over its GPUs, percent.
    GpuUtilization,
    /// GPU memory summed over its GPUs, bytes.
    GpuMemory,
    /// Bytes per second it made the storage layer read.
    DiskRead,
    /// Bytes per second it made the storage layer write.
    DiskWrite,
    /// Bytes per second the host received while it ran (not per process:
    /// the kernel does not count traffic per process).
    HostNetworkReceive,
    /// Bytes per second the host sent while it ran.
    HostNetworkTransmit,
    /// Its threads (D-77).
    Threads,
    /// Its descendant processes, at any depth.
    Children,
    /// CPU time of its descendants over wall time (those still running and
    /// those reaped since), percent of one core.
    ChildrenCpu,
    /// Resident memory of its running descendants, bytes.
    ChildrenMemory,
    /// Its memory swapped out, bytes.
    Swap,
    /// Its open file descriptors (readable for one's own processes only).
    OpenFiles,
}

impl RunMetric {
    pub const ALL: [Self; RUN_METRICS] = [
        Self::Cpu,
        Self::Memory,
        Self::GpuUtilization,
        Self::GpuMemory,
        Self::DiskRead,
        Self::DiskWrite,
        Self::HostNetworkReceive,
        Self::HostNetworkTransmit,
        Self::Threads,
        Self::Children,
        Self::ChildrenCpu,
        Self::ChildrenMemory,
        Self::Swap,
        Self::OpenFiles,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    /// Stable column stem in the database.
    pub const fn column(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::GpuUtilization => "gpu_util",
            Self::GpuMemory => "gpu_memory",
            Self::DiskRead => "disk_read",
            Self::DiskWrite => "disk_write",
            Self::HostNetworkReceive => "net_rx",
            Self::HostNetworkTransmit => "net_tx",
            Self::Threads => "threads",
            Self::Children => "children",
            Self::ChildrenCpu => "children_cpu",
            Self::ChildrenMemory => "children_memory",
            Self::Swap => "swap",
            Self::OpenFiles => "open_files",
        }
    }
}

/// Metrics a bucket holds. Stored buckets keep them in [`RunMetric::ALL`]
/// order, so a bucket written before a metric was added reads with that
/// metric empty.
pub const RUN_METRICS: usize = 14;

/// The mean and peak of the samples a bucket merged. Single precision
/// keeps a bucket small (seven digits are plenty for a chart), so a long
/// run's buckets stay a few hundred kilobytes.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Aggregate {
    pub count: u32,
    pub sum: f32,
    pub max: f32,
}

impl Aggregate {
    #[allow(clippy::cast_possible_truncation)]
    pub fn add(&mut self, value: f64) {
        let value = value as f32;
        if !value.is_finite() {
            return;
        }
        self.max = if self.count == 0 {
            value
        } else {
            self.max.max(value)
        };
        self.count = self.count.saturating_add(1);
        self.sum += value;
    }

    pub fn merge(&mut self, other: &Self) {
        if other.count == 0 {
            return;
        }
        self.max = if self.count == 0 {
            other.max
        } else {
            self.max.max(other.max)
        };
        self.count = self.count.saturating_add(other.count);
        self.sum += other.sum;
    }

    /// `None` without a sample.
    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| f64::from(self.sum) / f64::from(self.count))
    }

    pub fn peak(&self) -> Option<f64> {
        (self.count > 0).then_some(f64::from(self.max))
    }

    /// Rebuilt from a stored mean, peak and count.
    #[allow(clippy::cast_possible_truncation)]
    pub fn from_parts(count: u32, mean: f64, max: f64) -> Self {
        Self {
            count,
            sum: (mean * f64::from(count)) as f32,
            max: max as f32,
        }
    }
}

/// What a run used over one bucket of time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RunBucket {
    /// UTC start of the bucket, aligned to its tier's step.
    pub start_utc_ns: i64,
    #[serde(deserialize_with = "values_of_any_version")]
    pub values: [Aggregate; RUN_METRICS],
}

/// The values of a bucket saved by any version: metrics it did not know
/// are empty, metrics it knew beyond these are left out.
fn values_of_any_version<'de, D>(deserializer: D) -> Result<[Aggregate; RUN_METRICS], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let saved = Vec::<Aggregate>::deserialize(deserializer)?;
    let mut values = [Aggregate::default(); RUN_METRICS];
    for (value, saved) in values.iter_mut().zip(saved) {
        *value = saved;
    }
    Ok(values)
}

impl RunBucket {
    pub fn new(start_utc_ns: i64) -> Self {
        Self {
            start_utc_ns,
            values: [Aggregate::default(); RUN_METRICS],
        }
    }

    pub fn value(&self, metric: RunMetric) -> &Aggregate {
        &self.values[metric.index()]
    }

    pub fn add(&mut self, metric: RunMetric, value: Option<f64>) {
        if let Some(value) = value {
            self.values[metric.index()].add(value);
        }
    }

    pub fn merge(&mut self, other: &Self) {
        for (mine, theirs) in self.values.iter_mut().zip(&other.values) {
            mine.merge(theirs);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.values.iter().all(|value| value.count == 0)
    }
}

/// How a run ended, as far as dgxtop can tell without root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndCause {
    /// dgxtop sent it this signal, and it ended within a minute.
    SignaledByDgxtop(Signal),
    /// Its wait status says this signal ended it (another process, or the
    /// kernel).
    Signal(u8),
    /// Its wait status says it exited with this code.
    Exit(u8),
    /// The kernel's OOM-kill count rose as it ended: most likely the OOM
    /// killer.
    OutOfMemory,
    /// It ended without a cause dgxtop could see (its parent reaped it
    /// before a probe).
    NotVisible,
    /// It ended while dgxtop was not running.
    WhileAway,
    /// The host restarted.
    HostRestarted,
}

impl EndCause {
    /// Short text for the list and the detail page.
    pub fn text(self) -> String {
        match self {
            Self::SignaledByDgxtop(signal) => format!("{} by dgxtop", signal.name()),
            Self::Signal(signal) => match signal_name(signal) {
                Some(name) => format!("killed by {name}"),
                None => format!("killed by signal {signal}"),
            },
            Self::Exit(0) => "exited (code 0)".to_owned(),
            Self::Exit(code) => format!("exited with code {code}"),
            Self::OutOfMemory => "OOM killer (likely)".to_owned(),
            Self::NotVisible => "ended (cause not visible)".to_owned(),
            Self::WhileAway => "ended while dgxtop was off".to_owned(),
            Self::HostRestarted => "host restarted".to_owned(),
        }
    }

    /// Whether it says a signal ended the run (dgxtop's, another's or the
    /// kernel's).
    pub const fn was_killed(self) -> bool {
        matches!(
            self,
            Self::SignaledByDgxtop(_) | Self::Signal(_) | Self::OutOfMemory
        )
    }
}

/// The names of the signals that commonly end a process.
pub const fn signal_name(signal: u8) -> Option<&'static str> {
    Some(match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        7 => "SIGBUS",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => return None,
    })
}

/// When and how a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEnd {
    /// When dgxtop found it ended (within a cycle of the end), UTC.
    pub utc_ns: i64,
    pub cause: EndCause,
}

/// What a run was: kept for as long as the run is listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunInfo {
    pub key: ProcessKey,
    pub name: String,
    /// The full command line, cleaned and bounded; empty when unread.
    pub command: String,
    /// The working folder; empty when unread.
    pub cwd: String,
    pub uid: Option<u32>,
    /// The GPUs it used, as labelled when it used them (`GPU 0`).
    pub gpus: Vec<String>,
    /// What the probe read of it once, beside its command (D-77).
    pub details: RunDetails,
    /// When it started, UTC.
    pub started_utc_ns: i64,
    /// When dgxtop last saw it, UTC.
    pub last_seen_utc_ns: i64,
    pub end: Option<RunEnd>,
}

/// What a run is beside its command line, read once while it runs (D-77);
/// empty or `None` where unread.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RunDetails {
    /// Its parent when first read: PID and name.
    pub parent_pid: Option<i32>,
    pub parent_name: String,
    /// The program file it runs (readable for one's own processes only).
    pub executable: String,
    /// Its control group (`/user.slice/…`, a container's scope).
    pub cgroup: String,
    /// Its scheduling niceness, -20 to 19.
    pub nice: Option<i8>,
}

impl RunInfo {
    /// How long it ran (so far, while it runs).
    pub fn runtime_ns(&self) -> u64 {
        let until = self.end.map_or(self.last_seen_utc_ns, |end| end.utc_ns);
        u64::try_from(until.saturating_sub(self.started_utc_ns)).unwrap_or(0)
    }
}

/// One stored bucket of one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunBucketRow {
    pub key: ProcessKey,
    pub tier: Tier,
    pub bucket: RunBucket,
}

/// What the history hands its long-term store (D-94): runs that changed
/// and buckets that closed, and what the history decided the store no
/// longer keeps. The store applies them in this order, in one transaction.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ProcessHistoryJournal {
    pub runs: Vec<RunInfo>,
    pub buckets: Vec<RunBucketRow>,
    /// Each named size of a run keeps only its latest buckets.
    pub keep_latest: Vec<KeepLatest>,
    /// Runs deleted with their buckets.
    pub forget: Vec<ProcessKey>,
    /// Only this many runs stay, the last seen; the others are deleted.
    pub keep_runs: Option<usize>,
}

impl ProcessHistoryJournal {
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
            && self.buckets.is_empty()
            && self.keep_latest.is_empty()
            && self.forget.is_empty()
            && self.keep_runs.is_none()
    }
}

/// One run's size keeps only its latest `buckets` buckets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepLatest {
    pub key: ProcessKey,
    pub tier: Tier,
    pub buckets: usize,
}

/// What the long-term store holds after an append, for the history's
/// byte budget (D-94).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StoreUsage {
    /// Bytes of the database pages in use.
    pub used_bytes: u64,
    /// Runs it holds.
    pub runs: u64,
}

/// Which of a run's stored buckets to read: of each size named, its latest
/// this many, coarsest size first and each oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BucketRequest {
    pub latest: Vec<(Tier, usize)>,
}

/// What the history asks of its stores at startup (D-94).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProcessHistoryLoadRequest {
    /// The runs last seen, at most this many, the last seen first.
    pub runs: usize,
    /// The unfinished runs among them, and this many of the ended ones
    /// last seen, come with their buckets.
    pub ended_runs_with_buckets: usize,
    pub buckets: BucketRequest,
}

/// The short-term file: the live runs' recent buckets and the minute each
/// is filling, so a restart within the boot keeps the last minutes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentHistory {
    pub boot_id: BootId,
    pub saved_utc_ns: i64,
    pub runs: Vec<RecentRun>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentRun {
    pub key: ProcessKey,
    pub recent: Vec<RunBucket>,
    pub filling: Vec<(Tier, RunBucket)>,
}

/// What the history finds at startup.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ProcessHistoryLoad {
    pub runs: Vec<RunInfo>,
    pub buckets: Vec<RunBucketRow>,
    pub recent: Option<RecentHistory>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_merge_means_and_peaks() {
        let mut one = Aggregate::default();
        assert_eq!((one.mean(), one.peak()), (None, None));
        one.add(2.0);
        one.add(f64::NAN);
        one.add(4.0);
        let mut other = Aggregate::default();
        other.add(9.0);
        one.merge(&other);
        one.merge(&Aggregate::default());
        assert_eq!(one.count, 3);
        assert_eq!(one.mean(), Some(5.0));
        assert_eq!(one.peak(), Some(9.0));
        let stored = Aggregate::from_parts(one.count, 5.0, 9.0);
        assert_eq!(stored, one);
    }

    #[test]
    fn negative_values_keep_their_peak() {
        let mut aggregate = Aggregate::default();
        aggregate.add(-3.0);
        assert_eq!(aggregate.peak(), Some(-3.0));
    }

    /// A bucket saved before metrics were added reads with them empty.
    #[test]
    fn buckets_of_older_versions_deserialize() {
        let mut aggregate = Aggregate::default();
        aggregate.add(3.0);
        let saved = serde_json::json!({
            "start_utc_ns": 60,
            "values": vec![aggregate; 8],
        });
        let bucket: RunBucket = serde_json::from_value(saved).unwrap();
        assert_eq!(bucket.value(RunMetric::HostNetworkTransmit).count, 1);
        assert_eq!(bucket.value(RunMetric::Threads).count, 0);
        let newer = serde_json::json!({
            "start_utc_ns": 60,
            "values": vec![aggregate; RUN_METRICS + 2],
        });
        let bucket: RunBucket = serde_json::from_value(newer).unwrap();
        assert_eq!(bucket.value(RunMetric::OpenFiles).count, 1);
    }

    #[test]
    fn end_causes_read_as_text() {
        assert_eq!(
            EndCause::SignaledByDgxtop(Signal::Sigkill).text(),
            "SIGKILL by dgxtop"
        );
        assert_eq!(EndCause::Signal(9).text(), "killed by SIGKILL");
        assert_eq!(EndCause::Signal(40).text(), "killed by signal 40");
        assert_eq!(EndCause::Exit(1).text(), "exited with code 1");
        assert_eq!(EndCause::Exit(0).text(), "exited (code 0)");
        assert!(EndCause::OutOfMemory.was_killed());
        assert!(!EndCause::NotVisible.was_killed());
    }

    #[test]
    fn metrics_index_their_values() {
        for (index, metric) in RunMetric::ALL.iter().enumerate() {
            assert_eq!(metric.index(), index);
        }
        let mut bucket = RunBucket::new(0);
        assert!(bucket.is_empty());
        bucket.add(RunMetric::GpuMemory, Some(8.0));
        bucket.add(RunMetric::Cpu, None);
        assert_eq!(bucket.value(RunMetric::GpuMemory).mean(), Some(8.0));
        assert_eq!(bucket.value(RunMetric::Cpu).count, 0);
    }
}
