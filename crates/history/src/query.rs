//! Bounded history query engine (`STORAGE.md` §5, `history-result.schema.json`).
//!
//! A query pins one series under a short read lock and evaluates it without
//! holding any lock: half-open `[start, end)` windows of one boot, gauges by
//! the time-weighted step model (no zero filling, stale values not extended),
//! totals only from counter intervals fully inside a point (whole buckets,
//! bridges and raw intervals; never proportional), coverage as the union of
//! valid time. Every query has a point cap, a byte cap, a scan budget, a
//! deadline and a cancellation flag.

mod counter;
mod evaluate;
mod gauge;
mod guard;
mod plan;
mod points;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::{ErrorCode, ErrorDetail};
use dgxtop_core::history::{
    HISTORY_SCHEMA_VERSION, HistoryPoint, HistoryQuery, HistoryResult, MAX_HISTORY_POINTS,
};
use dgxtop_core::identity::{MetricKey, SourceId};
use dgxtop_core::ports::{CancelFlag, HistoryReader};

use crate::HistoryIndex;
use crate::gaps::{GapEntry, MAX_REASON_BYTES};
use crate::pin::{PinOutcome, PinRequest};

pub(crate) use guard::ScanGuard;

/// Most source ids one point carries (schema `maxItems`).
pub(crate) const MAX_POINT_SOURCES: usize = 4;

/// Static limits of the query engine, from `store` and `runtime` config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueryLimits {
    /// `store.max_history_points`, never above the schema cap of 2000.
    pub max_points: u32,
    /// `runtime.max_history_result_bytes`.
    pub max_result_bytes: u64,
    /// `store.max_query_scan_records`.
    pub max_scan_records: u64,
}

fn point_bytes() -> u64 {
    let point = std::mem::size_of::<HistoryPoint>()
        + 64
        + MAX_POINT_SOURCES * std::mem::size_of::<SourceId>()
        + MAX_REASON_BYTES;
    u64::try_from(point).unwrap_or(u64::MAX)
}

fn result_base_bytes(points: usize) -> u64 {
    // Source changes and clock discontinuities are capped at the point cap.
    let base = std::mem::size_of::<HistoryResult>() + 2 * points * std::mem::size_of::<u64>();
    u64::try_from(base).unwrap_or(u64::MAX)
}

impl QueryLimits {
    pub(crate) fn new(max_points: u32, max_result_bytes: u64, max_scan_records: u64) -> Self {
        Self {
            max_points: max_points.clamp(1, MAX_HISTORY_POINTS),
            max_result_bytes,
            max_scan_records,
        }
    }

    /// Points a result may carry: the request, the configured cap and the
    /// byte cap, whichever is smallest.
    pub(crate) fn point_cap(&self, requested: u32) -> usize {
        let configured = usize::try_from(requested.min(self.max_points)).unwrap_or(usize::MAX);
        let base = result_base_bytes(configured);
        let by_bytes = self.max_result_bytes.saturating_sub(base) / point_bytes().max(1);
        configured
            .min(usize::try_from(by_bytes).unwrap_or(usize::MAX))
            .max(1)
    }

    /// Bytes a result with `points` points is estimated to retain.
    pub(crate) fn result_bytes(&self, points: usize) -> u64 {
        result_base_bytes(points)
            .saturating_add(point_bytes().saturating_mul(u64::try_from(points).unwrap_or(u64::MAX)))
    }
}

fn validation_error(query: &HistoryQuery, message: impl AsRef<str>) -> ErrorDetail {
    ErrorDetail::new(ErrorCode::ValidationFailed, message)
        .with_request(query.request_id)
        .with_affected(query.key.series_selector())
}

fn validate(query: &HistoryQuery, limits: &QueryLimits) -> Result<(), ErrorDetail> {
    if query.schema_version != HISTORY_SCHEMA_VERSION {
        return Err(validation_error(
            query,
            format!(
                "history query schema_version {} is not supported",
                query.schema_version
            ),
        ));
    }
    query
        .validate(limits.max_points)
        .map_err(|error| validation_error(query, error.to_string()))
}

/// Result for a key the store holds no series for (unknown key, or a key of
/// another boot): no points and zero coverage, never an error.
fn empty_result(query: &HistoryQuery, revision: u64) -> HistoryResult {
    HistoryResult {
        schema_version: HISTORY_SCHEMA_VERSION,
        request_id: query.request_id,
        key: query.key.clone(),
        start_boot_ns: query.start_boot_ns,
        end_boot_ns: query.end_boot_ns,
        covered_ns: 0,
        coverage_ratio: 0.0,
        truncated: false,
        points: Vec::new(),
        source_changes: Vec::new(),
        clock_discontinuities: Vec::new(),
        snapshot_revision: revision,
        next_start_boot_ns: None,
    }
}

impl HistoryReader for HistoryIndex {
    fn query(
        &self,
        query: &HistoryQuery,
        deadline_boot_ns: u64,
        cancel: &CancelFlag,
        clock: &dyn TimeProvider,
    ) -> Result<HistoryResult, ErrorDetail> {
        let limits = *self.limits();
        let mut guard = ScanGuard::new(query, &limits, deadline_boot_ns, cancel, clock);
        guard.checkpoint()?;
        validate(query, &limits)?;
        let cap = limits.point_cap(query.max_points);
        let request = PinRequest {
            key: &query.key,
            start_ns: query.start_boot_ns,
            end_ns: query.end_boot_ns,
            stat: query.stat,
            result_bytes: limits.result_bytes(cap),
        };
        let outcome = self.read().pin(&request, self.ledger());
        let pinned = match outcome {
            Ok(PinOutcome::Pinned(pinned)) => pinned,
            Ok(PinOutcome::NoSeries { revision }) => return Ok(empty_result(query, revision)),
            // RUNTIME.md §5: insufficient query memory is a retryable Busy.
            Err(error) => {
                return Err(ErrorDetail::new(
                    ErrorCode::Busy,
                    format!("history query memory unavailable: {error}"),
                )
                .with_request(query.request_id));
            }
        };
        guard.checkpoint()?;
        evaluate::evaluate(&pinned, query, cap, &mut guard)
    }

    fn series(&self) -> Vec<MetricKey> {
        self.read().series_keys()
    }
}

/// Reason of the first gap overlapping `[start, end)`, if any.
pub(crate) fn gap_reason(gaps: &[GapEntry], start_ns: u64, end_ns: u64) -> Option<String> {
    gaps.iter()
        .find(|gap| gap.overlaps(start_ns, end_ns))
        .map(|gap| gap.reason.to_string())
}

/// Null point for a region without countable evidence: a partial bucket
/// whose raw data is gone (`boundary_gap`), or time before the retained
/// history (`gap`).
pub(crate) fn push_null_region(
    kind: plan::RegionKind,
    (start_ns, end_ns): (u64, u64),
    gaps: &[GapEntry],
    sink: &mut points::PointSink,
) {
    match kind {
        plan::RegionKind::BoundaryGap => sink.push_null(
            start_ns,
            end_ns,
            dgxtop_core::history::HistoryMethod::BoundaryGap,
            "partial_bucket_raw_evicted",
        ),
        plan::RegionKind::NoData => {
            let reason = gap_reason(gaps, start_ns, end_ns)
                .unwrap_or_else(|| "no_retained_history".to_owned());
            sink.push_null(
                start_ns,
                end_ns,
                dgxtop_core::history::HistoryMethod::Gap,
                &reason,
            );
        }
        plan::RegionKind::Raw | plan::RegionKind::Rollup => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_cap_respects_request_config_and_bytes() {
        let limits = QueryLimits::new(2000, 4 * 1024 * 1024, 200_000);
        assert_eq!(limits.point_cap(10), 10);
        assert_eq!(limits.point_cap(5000), 2000);
        let small = QueryLimits::new(2000, 64 * 1024, 200_000);
        let cap = small.point_cap(2000);
        assert!(cap < 2000);
        assert!(small.result_bytes(cap) <= 64 * 1024);
        assert_eq!(QueryLimits::new(5000, 1, 1).point_cap(3000), 1);
    }
}
