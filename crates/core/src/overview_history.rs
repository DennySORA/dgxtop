//! The Overview History's long-term buckets (D-82): the history keeps each
//! Overview series in every bar size (1 minute to 24 hours, [`Tier`]),
//! each size aligned to the local clock and keeping its latest
//! [`crate::tiers::TIER_BUCKETS`], fed by the minutes it
//! finalizes. A bucket carries what makes merging exact: the time it holds
//! a value, the time-weighted sum, the minimum, the maximum, the sample
//! count and, for a counter, the exact total. Its mean is the weighted sum
//! over the time held, so a bar or a window statistic merged from buckets
//! of any sizes is the mean of the time it actually holds.
//!
//! Besides the bucket, this module holds what crosses the ports: the rows
//! the history asks its store to keep, the archived minutes it compresses
//! at startup and the reads the Manager asks of it.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::errors::InfraError;
use crate::history::SeriesInEveryBoot;
use crate::identity::{EntityId, HostId};
use crate::metric::ids;
use crate::naming::entity;
use crate::process_model::MemoryPart;
use crate::tiers::Tier;

const NANOS_PER_SECOND_F64: f64 = 1e9;

/// The metrics the Overview History charts or summarizes (D-46, D-48,
/// D-50): the only series the history keeps in every bar size.
pub const OVERVIEW_METRICS: [&str; 11] = [
    ids::CPU_UTILIZATION_TOTAL,
    ids::MEMORY_USED,
    ids::GPU_UTILIZATION_COMPUTE,
    ids::GPU_MEMORY_FB_USED,
    ids::GPU_MEMORY_PROCESS_ALLOCATED,
    ids::GPU_TEMPERATURE,
    ids::GPU_POWER_DRAW,
    ids::DISK_READ_BPS,
    ids::DISK_WRITE_BPS,
    ids::NET_RX_BPS,
    ids::NET_TX_BPS,
];

/// Whether `metric_id` is one the Overview History charts or summarizes.
pub fn is_overview_metric(metric_id: &str) -> bool {
    OVERVIEW_METRICS.contains(&metric_id)
}

/// Whether the history keeps the series of `metric_id` on `entity_id` in
/// every bar size: an Overview metric, and of the CPU only the whole (the
/// history view charts the total, never each core).
pub fn is_overview_series(metric_id: &str, entity_id: &str) -> bool {
    is_overview_metric(metric_id)
        && (metric_id != ids::CPU_UTILIZATION_TOTAL || entity_id == entity::CPU_AGGREGATE)
}

/// A span of one series' time, merged exactly from what it holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverviewBucket {
    /// UTC start, aligned to its size's step on the local clock.
    pub start_utc_ns: i64,
    /// Time the series held a value in it.
    pub covered_ns: u64,
    /// Σ value × time held, in value-nanoseconds.
    pub weighted_sum: f64,
    /// Lowest and highest value held; `±∞` while it holds none.
    pub minimum: f64,
    pub maximum: f64,
    /// Samples (or counter intervals) merged.
    pub samples: u32,
    /// A counter's total over the intervals merged (0 for a gauge).
    pub total: u64,
    /// Whether `total` is exact: no interval was fractional and nothing
    /// overflowed.
    pub total_exact: bool,
}

impl OverviewBucket {
    /// An empty bucket starting at `start_utc_ns`.
    pub const fn new(start_utc_ns: i64) -> Self {
        Self {
            start_utc_ns,
            covered_ns: 0,
            weighted_sum: 0.0,
            minimum: f64::INFINITY,
            maximum: f64::NEG_INFINITY,
            samples: 0,
            total: 0,
            total_exact: true,
        }
    }

    /// A gauge's minute: `covered_ns` of steps summing to `weighted_sum`.
    pub fn gauge(
        start_utc_ns: i64,
        covered_ns: u64,
        weighted_sum: f64,
        (minimum, maximum): (f64, f64),
        samples: u32,
    ) -> Self {
        Self {
            covered_ns,
            weighted_sum,
            minimum,
            maximum,
            samples,
            ..Self::new(start_utc_ns)
        }
    }

    /// A counter's minute: `covered_ns` of intervals totalling `total`
    /// (`None` when the total was not exact).
    pub fn counter(
        start_utc_ns: i64,
        covered_ns: u64,
        total: Option<u128>,
        intervals: u32,
    ) -> Self {
        let exact = total.and_then(|total| u64::try_from(total).ok());
        Self {
            covered_ns,
            samples: intervals,
            total: exact.unwrap_or(0),
            total_exact: exact.is_some(),
            ..Self::new(start_utc_ns)
        }
    }

    /// Takes in everything `other` holds; its start stays.
    pub fn merge(&mut self, other: &Self) {
        if other.is_empty() {
            return;
        }
        self.covered_ns = self.covered_ns.saturating_add(other.covered_ns);
        self.weighted_sum += other.weighted_sum;
        self.minimum = self.minimum.min(other.minimum);
        self.maximum = self.maximum.max(other.maximum);
        self.samples = self.samples.saturating_add(other.samples);
        match self.total.checked_add(other.total) {
            Some(total) => self.total = total,
            None => {
                self.total = u64::MAX;
                self.total_exact = false;
            }
        }
        self.total_exact &= other.total_exact;
    }

    /// Whether it holds no time of the series.
    pub const fn is_empty(&self) -> bool {
        self.covered_ns == 0
    }

    /// The mean over the time it holds; `None` for a counter or when empty.
    #[allow(clippy::cast_precision_loss)]
    pub fn mean(&self) -> Option<f64> {
        (self.covered_ns > 0 && self.maximum.is_finite())
            .then(|| self.weighted_sum / self.covered_ns as f64)
    }

    /// The highest value held.
    pub fn peak(&self) -> Option<f64> {
        self.maximum.is_finite().then_some(self.maximum)
    }

    /// The lowest value held.
    pub fn low(&self) -> Option<f64> {
        self.minimum.is_finite().then_some(self.minimum)
    }

    /// Mean × time held in seconds (joules for a power series).
    pub fn integral(&self) -> Option<f64> {
        self.mean()
            .map(|_| self.weighted_sum / NANOS_PER_SECOND_F64)
    }
}

/// One closed bucket of one series and size, as the store keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct TierRow {
    pub series: SeriesInEveryBoot,
    pub tier: Tier,
    pub bucket: OverviewBucket,
}

/// One series' kept buckets as the store returns them at startup, each
/// size oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredTierSeries {
    pub series: SeriesInEveryBoot,
    pub buckets: Vec<(Tier, OverviewBucket)>,
}

/// What the history asks its store to write: first every bucket of the
/// `forget` series deleted (the history's cache let them go, so they are
/// not read back), then the closed buckets, then of every (series, size)
/// written only the latest `keep_latest` kept. The rows are shared: the
/// history keeps them (without a copy) when the store's queue is full.
#[derive(Debug, Clone, PartialEq)]
pub struct TierJournal {
    pub forget: Vec<SeriesInEveryBoot>,
    pub rows: Arc<Vec<TierRow>>,
    pub keep_latest: u32,
}

/// What the tiers store holds after an append, for the history's byte
/// budget (D-95): the store itself never lets a series go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TierStoreUsage {
    /// Bytes of the database pages in use.
    pub used_bytes: u64,
    /// Series it holds.
    pub series: u64,
}

/// The series of `metric_ids` the archive holds minutes of in earlier
/// runs: segments of `host_id` in their metric's canonical unit, started
/// before `before_utc_ns` (this run's own minutes reach the history live).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedSeriesList {
    pub host_id: HostId,
    pub metric_ids: Vec<String>,
    pub before_utc_ns: i64,
}

/// The archived gauge minutes of one series from earlier runs: those that
/// started at or after `from_utc_ns` in segments started before
/// `segments_before_utc_ns`, oldest first, at most `max_minutes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedMinutesRead {
    pub series: SeriesInEveryBoot,
    pub from_utc_ns: i64,
    pub segments_before_utc_ns: i64,
    pub max_minutes: u32,
}

/// Archived minutes as buckets starting at their minute's UTC start (not
/// aligned: a boot's minutes follow its boot clock).
#[derive(Debug, Clone, PartialEq)]
pub struct ArchivedMinutes {
    pub minutes: Vec<OverviewBucket>,
    /// More minutes follow the last one returned.
    pub more: bool,
}

/// What the history asks the archive to delete: one series' minutes, in
/// every boot, that started before `before_utc_ns` (the coarser sizes hold
/// them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePrune {
    pub series: SeriesInEveryBoot,
    pub before_utc_ns: i64,
}

/// Work of the history's store actor (D-82).
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryTierJob {
    /// Every kept bucket, at startup.
    Load,
    Append(TierJournal),
    ListArchived(ArchivedSeriesList),
    ReadArchived(ArchivedMinutesRead),
}

/// What became of a [`HistoryTierJob`].
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryTierCompletion {
    Loaded(Result<Vec<StoredTierSeries>, InfraError>),
    Appended(Result<TierStoreUsage, InfraError>),
    Listed(Result<Vec<SeriesInEveryBoot>, InfraError>),
    ArchivedRead {
        read: ArchivedMinutesRead,
        result: Result<ArchivedMinutes, InfraError>,
    },
}

/// What the Manager asks the history of one Overview series (D-82).
#[derive(Debug, Clone, PartialEq)]
pub enum OverviewRequest {
    /// The bars of `tier` that start in `[from_utc_ns, end_utc_ns)`, the
    /// latest with what is still being filled up to `at_utc_ns`, with the
    /// window's totals and, for a memory chart, the tracked processes'
    /// parts of what the GPUs `parts_of` held (D-64, D-99).
    Bars {
        series: SeriesInEveryBoot,
        tier: Tier,
        from_utc_ns: i64,
        end_utc_ns: i64,
        at_utc_ns: i64,
        /// No GPU: no parts.
        parts_of: BTreeSet<EntityId>,
    },
    /// The series over each span ending at `at_utc_ns`, merged from the
    /// aligned buckets that cover it.
    Windows {
        series: SeriesInEveryBoot,
        spans_ns: Vec<u64>,
        at_utc_ns: i64,
    },
}

impl OverviewRequest {
    pub fn series(&self) -> &SeriesInEveryBoot {
        match self {
            Self::Bars { series, .. } | Self::Windows { series, .. } => series,
        }
    }
}

/// The bars of one series: those holding a value, oldest first (a bar
/// with nothing is a gap), read whole from the history's sizes each time
/// (D-99).
#[derive(Debug, Clone, PartialEq)]
pub struct OverviewBars {
    pub bars: Vec<OverviewBucket>,
    /// The window as one bucket, merged from its bars: its mean, peak and
    /// the time it holds (D-99).
    pub whole: OverviewBucket,
    /// Each tracked process's part of the bars, one cell per bar of the
    /// window, in color order; empty when no GPU was asked (D-64, D-97).
    pub parts: Vec<MemoryPart>,
    /// The history keeps the sizes on disk (its store is attached).
    pub kept_on_disk: bool,
    /// Why the history's store is not keeping the sizes, if it is not.
    pub store_issue: Option<String>,
}

/// The history's answer to an [`OverviewRequest`].
#[derive(Debug, Clone, PartialEq)]
pub enum OverviewAnswer {
    Bars(OverviewBars),
    /// One bucket per requested span, in order.
    Windows(Vec<OverviewBucket>),
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60_000_000_000;

    fn minute(start: i64, value: f64, covered: u64) -> OverviewBucket {
        #[allow(clippy::cast_precision_loss)]
        let sum = value * covered as f64;
        OverviewBucket::gauge(start, covered, sum, (value, value), 1)
    }

    /// Means, peaks and energy merge without loss: a bucket that holds 45
    /// minutes is the mean of those 45 minutes, whatever it is merged from.
    #[test]
    fn merging_is_exact_over_the_time_held() {
        let mut hour = OverviewBucket::new(0);
        assert!(hour.is_empty());
        assert_eq!(hour.mean(), None);
        hour.merge(&minute(0, 10.0, 15 * MINUTE));
        hour.merge(&minute(15, 30.0, 30 * MINUTE));
        hour.merge(&OverviewBucket::new(99));
        assert_eq!(hour.start_utc_ns, 0, "the start stays");
        assert_eq!(hour.covered_ns, 45 * MINUTE);
        let expected = (10.0 * 15.0 + 30.0 * 30.0) / 45.0;
        assert!((hour.mean().unwrap() - expected).abs() < 1e-9);
        assert_eq!(hour.peak(), Some(30.0));
        assert_eq!(hour.low(), Some(10.0));
        assert_eq!(hour.samples, 2);
        let joules = expected * 45.0 * 60.0;
        assert!((hour.integral().unwrap() - joules).abs() < 1e-6);
    }

    #[test]
    fn counter_totals_stay_exact_until_they_cannot() {
        let mut total = OverviewBucket::counter(0, MINUTE, Some(300), 3);
        total.merge(&OverviewBucket::counter(60, MINUTE, Some(5), 1));
        assert_eq!((total.total, total.total_exact), (305, true));
        assert_eq!(total.mean(), None, "a counter has no mean");
        total.merge(&OverviewBucket::counter(120, MINUTE, None, 1));
        assert!(!total.total_exact);
        let mut huge = OverviewBucket::counter(0, MINUTE, Some(u128::from(u64::MAX)), 1);
        huge.merge(&OverviewBucket::counter(0, MINUTE, Some(1), 1));
        assert!(!huge.total_exact, "an overflow is not exact");
        let beyond = OverviewBucket::counter(0, MINUTE, Some(u128::MAX), 1);
        assert!(!beyond.total_exact);
    }

    #[test]
    fn the_overview_metrics_are_known() {
        for metric in OVERVIEW_METRICS {
            assert!(crate::metric::descriptor(metric).is_some(), "{metric}");
            assert!(is_overview_metric(metric));
        }
        assert!(!is_overview_metric(ids::CPU_UTILIZATION_IOWAIT));
        assert!(is_overview_series(
            ids::CPU_UTILIZATION_TOTAL,
            entity::CPU_AGGREGATE
        ));
        assert!(!is_overview_series(
            ids::CPU_UTILIZATION_TOTAL,
            "cpu.core.3"
        ));
        assert!(is_overview_series(ids::GPU_POWER_DRAW, "gpu:GPU-1"));
    }
}
