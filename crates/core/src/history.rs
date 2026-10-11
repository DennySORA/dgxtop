//! Typed, bounded history queries (`history-query.schema.json`) and results
//! (`history-result.schema.json`). Windows are half-open `[start, end)` on the
//! boot clock of one boot; coverage is the union of valid data time.
//!
//! Archive reads (D-51) answer what earlier runs and boots left in the
//! history archive: whole minutes of one gauge series over a window of the
//! wall clock, because boots share no boot clock (STORAGE.md §3 lets UTC
//! index and display). They are internal; the query contract stays one boot.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::identity::{EntityId, HostId, MetricId, MetricKey, Scope, SourceId};
use crate::metric::Scalar;

/// History schema version.
pub const HISTORY_SCHEMA_VERSION: u32 = 1;
/// Absolute maximum points per result.
pub const MAX_HISTORY_POINTS: u32 = 2000;

/// Requested statistic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryStat {
    Mean,
    Min,
    Max,
    Total,
    Raw,
}

/// Aggregation method of one point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMethod {
    Raw,
    TimeWeightedStep,
    CounterDelta,
    EstimatedIntegral,
    BucketExact,
    BoundaryGap,
    Gap,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HistoryQueryError {
    #[error("end_boot_ns must be greater than start_boot_ns")]
    EmptyWindow,
    #[error("resolution_ms must be 0 or 100..=86400000")]
    Resolution,
    #[error("max_points must be 1..={0}")]
    MaxPoints(u32),
    #[error("an archive point must span whole minutes")]
    ArchivePoint,
}

/// One bounded history query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryQuery {
    pub schema_version: u32,
    pub request_id: Uuid,
    pub key: MetricKey,
    #[serde(with = "crate::wire::u64_decimal")]
    pub start_boot_ns: u64,
    #[serde(with = "crate::wire::u64_decimal")]
    pub end_boot_ns: u64,
    pub resolution_ms: u32,
    pub stat: HistoryStat,
    pub max_points: u32,
    pub source_filter: Option<SourceId>,
}

impl HistoryQuery {
    /// Validates the window and limits against the configured point cap.
    pub fn validate(&self, configured_max_points: u32) -> Result<(), HistoryQueryError> {
        if self.end_boot_ns <= self.start_boot_ns {
            return Err(HistoryQueryError::EmptyWindow);
        }
        if self.resolution_ms != 0 && !(100..=86_400_000).contains(&self.resolution_ms) {
            return Err(HistoryQueryError::Resolution);
        }
        let cap = configured_max_points.min(MAX_HISTORY_POINTS);
        if self.max_points == 0 || self.max_points > cap {
            return Err(HistoryQueryError::MaxPoints(cap));
        }
        Ok(())
    }

    pub fn span_ns(&self) -> u64 {
        self.end_boot_ns.saturating_sub(self.start_boot_ns)
    }
}

/// One result point over `[start, end)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryPoint {
    #[serde(with = "crate::wire::u64_decimal")]
    pub start_boot_ns: u64,
    #[serde(with = "crate::wire::u64_decimal")]
    pub end_boot_ns: u64,
    pub value: Option<Scalar>,
    #[serde(with = "crate::wire::u64_decimal")]
    pub covered_ns: u64,
    pub method: HistoryMethod,
    pub source_ids: Vec<SourceId>,
    pub reason: Option<String>,
}

/// Complete result; `truncated` + `next_start_boot_ns` page raw results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryResult {
    pub schema_version: u32,
    pub request_id: Uuid,
    pub key: MetricKey,
    #[serde(with = "crate::wire::u64_decimal")]
    pub start_boot_ns: u64,
    #[serde(with = "crate::wire::u64_decimal")]
    pub end_boot_ns: u64,
    #[serde(with = "crate::wire::u64_decimal")]
    pub covered_ns: u64,
    pub coverage_ratio: f64,
    pub truncated: bool,
    pub points: Vec<HistoryPoint>,
    #[serde(with = "crate::wire::vec_u64_decimal")]
    pub source_changes: Vec<u64>,
    #[serde(with = "crate::wire::vec_u64_decimal")]
    pub clock_discontinuities: Vec<u64>,
    #[serde(with = "crate::wire::u64_decimal")]
    pub snapshot_revision: u64,
    #[serde(with = "crate::wire::opt_u64_decimal")]
    pub next_start_boot_ns: Option<u64>,
}

impl HistoryResult {
    /// Rough retained size for the history-query partition.
    pub fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.points.len() * (std::mem::size_of::<HistoryPoint>() + 64)
            + (self.source_changes.len() + self.clock_discontinuities.len()) * 8
    }

    /// Coverage ratio computed with integer arithmetic, clamped to `[0, 1]`.
    #[allow(clippy::cast_precision_loss)]
    pub fn ratio(covered_ns: u64, span_ns: u64) -> f64 {
        if span_ns == 0 {
            return 0.0;
        }
        (covered_ns.min(span_ns) as f64) / (span_ns as f64)
    }
}

/// The archive keeps whole minutes, so an archive point spans at least one.
pub const ARCHIVE_MINUTE_NS: u64 = 60_000_000_000;

/// A series in every boot: its metric key without the boot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SeriesInEveryBoot {
    pub host_id: HostId,
    pub entity_id: EntityId,
    pub metric_id: MetricId,
    pub scope: Scope,
}

impl From<&MetricKey> for SeriesInEveryBoot {
    fn from(key: &MetricKey) -> Self {
        Self {
            host_id: key.host_id.clone(),
            entity_id: key.entity_id.clone(),
            metric_id: key.metric_id.clone(),
            scope: key.scope.clone(),
        }
    }
}

/// The archived minutes of one gauge series that started in
/// `[start_utc_ns, end_utc_ns)`, from every boot, merged into points
/// `point_ns` wide on the UTC grid (multiples of `point_ns`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveQuery {
    pub request_id: Uuid,
    pub series: SeriesInEveryBoot,
    pub start_utc_ns: u64,
    pub end_utc_ns: u64,
    /// A whole number of minutes.
    pub point_ns: u64,
    pub max_points: u32,
}

impl ArchiveQuery {
    /// Validates the window, the point width and the point cap.
    pub fn validate(&self) -> Result<(), HistoryQueryError> {
        if self.end_utc_ns <= self.start_utc_ns {
            return Err(HistoryQueryError::EmptyWindow);
        }
        if self.point_ns < ARCHIVE_MINUTE_NS || !self.point_ns.is_multiple_of(ARCHIVE_MINUTE_NS) {
            return Err(HistoryQueryError::ArchivePoint);
        }
        if self.max_points == 0 || self.max_points > MAX_HISTORY_POINTS {
            return Err(HistoryQueryError::MaxPoints(MAX_HISTORY_POINTS));
        }
        Ok(())
    }

    /// Start of the point holding `utc_ns`.
    pub fn point_start(&self, utc_ns: u64) -> u64 {
        utc_ns - utc_ns % self.point_ns.max(1)
    }
}

/// The minutes of one archive point: their time-weighted mean, their
/// maximum and the time they cover (never more than the point spans).
#[derive(Debug, Clone, PartialEq)]
pub struct ArchivePoint {
    pub start_utc_ns: u64,
    pub end_utc_ns: u64,
    pub mean: Option<f64>,
    pub max: Option<f64>,
    pub covered_ns: u64,
}

/// The points that hold archived minutes (a point with none is left out),
/// oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveResult {
    pub request_id: Uuid,
    pub points: Vec<ArchivePoint>,
    pub covered_ns: u64,
    /// Boots the minutes came from.
    pub boots: u32,
    /// The window held more points than `max_points`: the newest are kept.
    pub truncated: bool,
}

impl ArchiveResult {
    /// No archived minute in the window.
    pub fn empty(request_id: Uuid) -> Self {
        Self {
            request_id,
            points: Vec::new(),
            covered_ns: 0,
            boots: 0,
            truncated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_examples_round_trip() {
        let query_text = include_str!("../../../spec/dgxtop-v2/examples/history-query.json");
        let query: HistoryQuery = serde_json::from_str(query_text).unwrap();
        query.validate(2000).unwrap();
        assert_eq!(
            serde_json::to_value(&query).unwrap(),
            serde_json::from_str::<serde_json::Value>(query_text).unwrap()
        );
        let result_text =
            include_str!("../../../spec/dgxtop-v2/examples/history-result.partial.json");
        let result: HistoryResult = serde_json::from_str(result_text).unwrap();
        assert_eq!(result.points.len(), 3);
        assert!(result.points[1].value.is_none());
        assert_eq!(result.coverage_ratio, 0.5);
        // The example writes `68` where we emit `68.0`; compare semantically.
        let reencoded = serde_json::to_string(&result).unwrap();
        let back: HistoryResult = serde_json::from_str(&reencoded).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn query_validation() {
        let mut query: HistoryQuery = serde_json::from_str(include_str!(
            "../../../spec/dgxtop-v2/examples/history-query.json"
        ))
        .unwrap();
        query.end_boot_ns = query.start_boot_ns;
        assert_eq!(query.validate(2000), Err(HistoryQueryError::EmptyWindow));
        query.end_boot_ns = query.start_boot_ns + 1;
        query.resolution_ms = 50;
        assert_eq!(query.validate(2000), Err(HistoryQueryError::Resolution));
        query.resolution_ms = 0;
        query.max_points = 2001;
        assert!(query.validate(2000).is_err());
        query.max_points = 600;
        assert!(query.validate(500).is_err());
    }

    fn archive_query() -> ArchiveQuery {
        let key = crate::testing::metric_key("gpu:GPU-1", "gpu.temperature", "gpu");
        ArchiveQuery {
            request_id: Uuid::nil(),
            series: SeriesInEveryBoot::from(&key),
            start_utc_ns: 10 * ARCHIVE_MINUTE_NS,
            end_utc_ns: 70 * ARCHIVE_MINUTE_NS,
            point_ns: 2 * ARCHIVE_MINUTE_NS,
            max_points: 60,
        }
    }

    #[test]
    fn archive_queries_span_whole_minutes_on_a_utc_grid() {
        let query = archive_query();
        assert_eq!(query.validate(), Ok(()));
        assert_eq!(query.series.metric_id.as_str(), "gpu.temperature");
        assert_eq!(
            query.point_start(13 * ARCHIVE_MINUTE_NS + 5),
            12 * ARCHIVE_MINUTE_NS
        );
        let mut bad = archive_query();
        bad.end_utc_ns = bad.start_utc_ns;
        assert_eq!(bad.validate(), Err(HistoryQueryError::EmptyWindow));
        let mut bad = archive_query();
        bad.point_ns = 90_000_000_000;
        assert_eq!(bad.validate(), Err(HistoryQueryError::ArchivePoint));
        bad.point_ns = 1_000_000_000;
        assert_eq!(bad.validate(), Err(HistoryQueryError::ArchivePoint));
        let mut bad = archive_query();
        bad.max_points = 0;
        assert!(bad.validate().is_err());
        bad.max_points = MAX_HISTORY_POINTS + 1;
        assert!(bad.validate().is_err());
        assert!(ArchiveResult::empty(Uuid::nil()).points.is_empty());
    }

    #[test]
    fn coverage_ratio_is_clamped() {
        assert_eq!(HistoryResult::ratio(5, 10), 0.5);
        assert_eq!(HistoryResult::ratio(20, 10), 1.0);
        assert_eq!(HistoryResult::ratio(5, 0), 0.0);
    }
}
