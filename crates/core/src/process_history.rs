//! The process history (D-76): every GPU process dgxtop saw, as a *run*
//! (one [`ProcessKey`]: a PID with its start time on one boot), with what it
//! ran, where, on which GPUs, when it started and ended and why, and the
//! averages and peaks of what it used over its life.
//!
//! What it used is kept as [`RunBucket`]s of the sizes a chart draws as
//! bars (D-77, [`Tier`]): 1, 5, 10 and 30 minutes, 1, 2, 4, 6, 8, 12, 18 and
//! 24 hours, each bucket the merge of the finer ones it covers, and 5 s
//! buckets for the last 10 minutes (kept in a short-term file). Each size
//! keeps its latest [`TIER_BUCKETS`], as many as the widest chart draws:
//! older ones are in the coarser sizes already. So a run costs the same
//! however long it lasts, and its whole life stays at the coarse sizes.

use serde::{Deserialize, Serialize};

use crate::action::Signal;
use crate::identity::{BootId, ProcessKey};

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Runs followed at once (live, probed every cycle).
pub const MAX_LIVE_RUNS: usize = crate::process_probe::MAX_PROBED_PROCESSES;
/// Runs the history lists; the oldest ended ones give way.
pub const MAX_LISTED_RUNS: usize = 512;
/// Buckets each size but the 5 s one keeps: bars of the widest chart.
pub const TIER_BUCKETS: usize = 300;

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

/// The size of a bucket: the 5 s buckets of the last minutes, and one size
/// for each bar size of the charts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Tier {
    Recent,
    OneMinute,
    FiveMinutes,
    TenMinutes,
    ThirtyMinutes,
    OneHour,
    TwoHours,
    FourHours,
    SixHours,
    EightHours,
    TwelveHours,
    EighteenHours,
    OneDay,
}

impl Tier {
    /// The sizes, finest first: a size's source comes before it.
    pub const ALL: [Self; 13] = [
        Self::Recent,
        Self::OneMinute,
        Self::FiveMinutes,
        Self::TenMinutes,
        Self::ThirtyMinutes,
        Self::OneHour,
        Self::TwoHours,
        Self::FourHours,
        Self::SixHours,
        Self::EightHours,
        Self::TwelveHours,
        Self::EighteenHours,
        Self::OneDay,
    ];

    pub const fn step_ns(self) -> u64 {
        let minutes = match self {
            Self::Recent => return 5 * NANOS_PER_SECOND,
            Self::OneMinute => 1,
            Self::FiveMinutes => 5,
            Self::TenMinutes => 10,
            Self::ThirtyMinutes => 30,
            Self::OneHour => 60,
            Self::TwoHours => 120,
            Self::FourHours => 240,
            Self::SixHours => 360,
            Self::EightHours => 480,
            Self::TwelveHours => 720,
            Self::EighteenHours => 1_080,
            Self::OneDay => 1_440,
        };
        minutes * 60 * NANOS_PER_SECOND
    }

    /// The size whose closed buckets this one merges: the coarsest finer
    /// size its step is a multiple of (18 hours take 6, 24 take 12).
    pub const fn source(self) -> Option<Self> {
        Some(match self {
            Self::Recent => return None,
            Self::OneMinute => Self::Recent,
            Self::FiveMinutes => Self::OneMinute,
            Self::TenMinutes => Self::FiveMinutes,
            Self::ThirtyMinutes => Self::TenMinutes,
            Self::OneHour => Self::ThirtyMinutes,
            Self::TwoHours => Self::OneHour,
            Self::FourHours | Self::SixHours => Self::TwoHours,
            Self::EightHours => Self::FourHours,
            Self::TwelveHours | Self::EighteenHours => Self::SixHours,
            Self::OneDay => Self::TwelveHours,
        })
    }

    /// Its position in [`Tier::ALL`].
    pub const fn level(self) -> usize {
        self as usize
    }

    /// Buckets of this size one run keeps at most: 10 minutes of 5 s
    /// buckets, else [`TIER_BUCKETS`].
    pub const fn capacity(self) -> usize {
        match self {
            Self::Recent => 120,
            _ => TIER_BUCKETS,
        }
    }

    /// The code stored in the database; the minutes and ten minutes keep
    /// their codes of version 2.
    pub const fn code(self) -> i64 {
        match self {
            Self::Recent => 0,
            Self::OneMinute => 1,
            Self::TenMinutes => 2,
            Self::FiveMinutes => 3,
            Self::ThirtyMinutes => 4,
            Self::OneHour => 5,
            Self::TwoHours => 6,
            Self::FourHours => 7,
            Self::SixHours => 8,
            Self::EightHours => 9,
            Self::TwelveHours => 10,
            Self::EighteenHours => 11,
            Self::OneDay => 12,
        }
    }

    pub fn from_code(code: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|tier| tier.code() == code)
    }

    /// The start of the bucket holding `utc_ns`, its boundaries moved by
    /// `shift_ns` (the local clock's offset, so a day is a local day).
    pub fn align(self, utc_ns: i64, shift_ns: i64) -> i64 {
        let step = i64::try_from(self.step_ns()).unwrap_or(i64::MAX);
        utc_ns
            .saturating_add(shift_ns)
            .div_euclid(step)
            .saturating_mul(step)
            .saturating_sub(shift_ns)
    }
}

/// How much time one bar of a chart covers (D-77, D-78): its mean of every
/// sample in that time. The Process and the Overview History share these
/// sizes; `Auto` (Process History only) takes the finest that shows the
/// whole run. Serialized as its token (`1m`, `24h`, `auto`), as the
/// session's bar sizes travel (D-80).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BarSize {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "1m")]
    OneMinute,
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "10m")]
    TenMinutes,
    #[serde(rename = "30m")]
    ThirtyMinutes,
    #[serde(rename = "1h")]
    OneHour,
    #[serde(rename = "2h")]
    TwoHours,
    #[serde(rename = "4h")]
    FourHours,
    #[serde(rename = "6h")]
    SixHours,
    #[serde(rename = "8h")]
    EightHours,
    #[serde(rename = "12h")]
    TwelveHours,
    #[serde(rename = "18h")]
    EighteenHours,
    #[serde(rename = "24h")]
    OneDay,
}

impl Default for BarSize {
    /// The Process History's default.
    fn default() -> Self {
        Self::Auto
    }
}

impl BarSize {
    /// The Overview History's default (it has no `Auto`).
    pub const fn overview_default() -> Self {
        Self::OneMinute
    }

    /// As the Overview History draws it: `Auto` is its default.
    pub const fn for_overview(self) -> Self {
        match self {
            Self::Auto => Self::overview_default(),
            fixed => fixed,
        }
    }

    /// The next (or previous) size the Overview History steps to, wrapping
    /// past `Auto`.
    pub fn step_overview(self, forward: bool) -> Self {
        match self.for_overview().step(forward) {
            Self::Auto => self.for_overview().step(forward).step(forward),
            next => next,
        }
    }

    /// In the order `w` steps through.
    pub const ALL: [Self; 13] = [
        Self::Auto,
        Self::OneMinute,
        Self::FiveMinutes,
        Self::TenMinutes,
        Self::ThirtyMinutes,
        Self::OneHour,
        Self::TwoHours,
        Self::FourHours,
        Self::SixHours,
        Self::EightHours,
        Self::TwelveHours,
        Self::EighteenHours,
        Self::OneDay,
    ];

    pub const fn token(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::OneMinute => "1m",
            Self::FiveMinutes => "5m",
            Self::TenMinutes => "10m",
            Self::ThirtyMinutes => "30m",
            Self::OneHour => "1h",
            Self::TwoHours => "2h",
            Self::FourHours => "4h",
            Self::SixHours => "6h",
            Self::EightHours => "8h",
            Self::TwelveHours => "12h",
            Self::EighteenHours => "18h",
            Self::OneDay => "24h",
        }
    }

    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|bar| bar.token() == token)
    }

    /// The bucket size its bars are; `None` for `Auto`.
    pub const fn tier(self) -> Option<Tier> {
        Some(match self {
            Self::Auto => return None,
            Self::OneMinute => Tier::OneMinute,
            Self::FiveMinutes => Tier::FiveMinutes,
            Self::TenMinutes => Tier::TenMinutes,
            Self::ThirtyMinutes => Tier::ThirtyMinutes,
            Self::OneHour => Tier::OneHour,
            Self::TwoHours => Tier::TwoHours,
            Self::FourHours => Tier::FourHours,
            Self::SixHours => Tier::SixHours,
            Self::EightHours => Tier::EightHours,
            Self::TwelveHours => Tier::TwelveHours,
            Self::EighteenHours => Tier::EighteenHours,
            Self::OneDay => Tier::OneDay,
        })
    }

    pub const fn step_ns(self) -> Option<u64> {
        match self.tier() {
            Some(tier) => Some(tier.step_ns()),
            None => None,
        }
    }

    /// The next (or with `forward` false the previous) size, wrapping.
    pub fn step(self, forward: bool) -> Self {
        let count = Self::ALL.len();
        let position = Self::ALL.iter().position(|bar| *bar == self).unwrap_or(0);
        let next = if forward {
            (position + 1) % count
        } else {
            (position + count - 1) % count
        };
        Self::ALL[next]
    }

    /// The sizes `Auto` chooses from, finest first.
    pub fn fixed() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter().skip(1)
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

/// What the history hands its long-term store: runs that changed and
/// buckets that closed.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ProcessHistoryJournal {
    pub runs: Vec<RunInfo>,
    pub buckets: Vec<RunBucketRow>,
}

impl ProcessHistoryJournal {
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty() && self.buckets.is_empty()
    }
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

    #[test]
    fn tiers_align_nest_and_keep_their_buckets() {
        const HOUR: i64 = 3_600 * 1_000_000_000;
        assert_eq!(Tier::Recent.capacity(), 120);
        assert_eq!(Tier::OneDay.capacity(), TIER_BUCKETS);
        assert_eq!(Tier::OneMinute.align(119_000_000_000, 0), 60_000_000_000);
        assert_eq!(Tier::OneMinute.align(-1, 0), -60_000_000_000);
        // UTC+8: a day starts at 16:00 UTC.
        assert_eq!(Tier::OneDay.align(HOUR, 8 * HOUR), -8 * HOUR);
        for (level, tier) in Tier::ALL.into_iter().enumerate() {
            assert_eq!(Tier::from_code(tier.code()), Some(tier));
            assert_eq!(tier.level(), level);
            if let Some(source) = tier.source() {
                assert!(source < tier, "{tier:?} after its source");
                assert_eq!(tier.step_ns() % source.step_ns(), 0, "{tier:?} nests");
            }
        }
        assert_eq!(Tier::Recent.source(), None);
        assert_eq!(Tier::from_code(13), None);
        assert_eq!(
            Tier::from_code(1),
            Some(Tier::OneMinute),
            "version 2's minutes"
        );
        assert_eq!(Tier::from_code(2), Some(Tier::TenMinutes));
        for bar in BarSize::fixed() {
            assert_eq!(bar.tier().map(Tier::step_ns), bar.step_ns());
        }
    }

    #[test]
    fn bars_parse_step_and_wrap() {
        let tokens: Vec<&str> = BarSize::ALL.iter().map(|bar| bar.token()).collect();
        assert_eq!(
            tokens,
            vec![
                "auto", "1m", "5m", "10m", "30m", "1h", "2h", "4h", "6h", "8h", "12h", "18h", "24h"
            ]
        );
        for bar in BarSize::ALL {
            assert_eq!(BarSize::parse(bar.token()), Some(bar));
        }
        assert_eq!(BarSize::parse("3h"), None);
        assert_eq!(BarSize::Auto.step_ns(), None);
        assert_eq!(
            BarSize::EighteenHours.step_ns(),
            Some(18 * 3_600 * 1_000_000_000)
        );
        assert_eq!(BarSize::OneDay.step(true), BarSize::Auto);
        assert_eq!(BarSize::Auto.step(false), BarSize::OneDay);
        assert_eq!(BarSize::OneMinute.step(true), BarSize::FiveMinutes);
        assert_eq!(BarSize::fixed().count(), 12);
        // The Overview History has no `auto`: stepping skips it (D-78).
        assert_eq!(BarSize::OneDay.step_overview(true), BarSize::OneMinute);
        assert_eq!(BarSize::OneMinute.step_overview(false), BarSize::OneDay);
        assert_eq!(BarSize::Auto.for_overview(), BarSize::OneMinute);
        // Sizes travel as their tokens (D-80).
        assert_eq!(serde_json::to_value(BarSize::EighteenHours).unwrap(), "18h");
        assert_eq!(
            serde_json::from_value::<BarSize>(serde_json::json!("auto")).unwrap(),
            BarSize::Auto
        );
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
