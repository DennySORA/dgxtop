//! What one history mutation produced: archive rows, drops and new gaps.

use std::sync::Arc;

use dgxtop_core::canonical::GapRecord;

use super::records::{CounterBucket, CounterSample, GaugeBucket};
use super::segment::SegmentMeta;

/// A finished row for the optional archive (`history.sql`). A minute
/// carries its end from the boot's grid: a boot's head minute is shorter.
#[derive(Debug, Clone)]
pub(crate) enum ArchiveRow {
    GaugeBucket {
        segment: Arc<SegmentMeta>,
        bucket: GaugeBucket,
        end_ns: u64,
    },
    CounterBucket {
        segment: Arc<SegmentMeta>,
        bucket: CounterBucket,
        end_ns: u64,
    },
    Bridge {
        segment: Arc<SegmentMeta>,
        bridge: CounterSample,
    },
    Gap(GapRecord),
}

/// Why a value or interval did not enter history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropReason {
    /// Older than the finalized horizon (its minute is already rolled up).
    Late,
    /// Not newer than the previous sample of the series.
    OutOfOrder,
    /// Overlaps the previous counter interval (would be counted twice).
    Overlap,
    /// Sample time is after the commit time.
    Future,
    /// The partition stayed full after evicting what could be evicted.
    Capacity,
    /// The series reached its segment limit.
    SegmentLimit,
    /// The store already holds `store.max_series` series.
    SeriesLimit,
}

impl DropReason {
    /// Reason code recorded in the gap index for data-loss drops.
    pub(crate) const fn gap_reason(self) -> Option<&'static str> {
        match self {
            Self::Capacity => Some("history_capacity"),
            Self::SegmentLimit => Some("segment_capacity"),
            Self::SeriesLimit => Some("series_capacity"),
            Self::Late | Self::OutOfOrder | Self::Overlap | Self::Future => None,
        }
    }
}

/// Per-reason drop counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DropCounts {
    pub late: u64,
    pub out_of_order: u64,
    pub overlap: u64,
    pub future: u64,
    pub capacity: u64,
    pub segment_limit: u64,
    pub series_limit: u64,
}

impl DropCounts {
    pub(crate) fn record(&mut self, reason: DropReason) {
        let counter = match reason {
            DropReason::Late => &mut self.late,
            DropReason::OutOfOrder => &mut self.out_of_order,
            DropReason::Overlap => &mut self.overlap,
            DropReason::Future => &mut self.future,
            DropReason::Capacity => &mut self.capacity,
            DropReason::SegmentLimit => &mut self.segment_limit,
            DropReason::SeriesLimit => &mut self.series_limit,
        };
        *counter = counter.saturating_add(1);
    }

    pub(crate) fn total(&self) -> u64 {
        [
            self.late,
            self.out_of_order,
            self.overlap,
            self.future,
            self.capacity,
            self.segment_limit,
            self.series_limit,
        ]
        .iter()
        .fold(0u64, |sum, count| sum.saturating_add(*count))
    }
}

/// Result of one history mutation.
#[derive(Debug, Default)]
pub(crate) struct HistoryReport {
    pub archive_rows: Vec<ArchiveRow>,
    pub gauge_drops: DropCounts,
    pub interval_drops: DropCounts,
    /// Records removed because a partition was full (not normal retention).
    pub evicted_records: u64,
    /// Gaps that became visible (new or extended) in this mutation.
    pub new_gaps: Vec<GapRecord>,
    /// Distinct gap entries created (extensions of a merged gap excluded).
    pub gaps_added: u64,
    pub series_count: u32,
    pub truncated: bool,
}
