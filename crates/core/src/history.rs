//! What the history's series share with the rest of dgxtop: how a
//! segment's points were aggregated ([`HistoryMethod`], a segment
//! identity part), the most bars a chart asks for, the coverage ratio,
//! and a series in every boot ([`SeriesInEveryBoot`]).
//!
//! There is no general history query: the Manager reads the history only
//! through the Overview's sizes (D-82, D-90).
//!
//! A series in every boot names what the archive and the Overview's sizes
//! keep across boots (D-51, D-82): boots share no boot clock, so they are
//! placed on the wall clock, which only orders and displays them.

use serde::{Deserialize, Serialize};

use crate::identity::{EntityId, HostId, MetricId, MetricKey, Scope};

/// Most bars one chart asks for.
pub const MAX_HISTORY_POINTS: u32 = 2000;

/// Aggregation method of a history segment.
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

/// The share of `span_ns` that `covered_ns` covers, clamped to `[0, 1]`.
#[allow(clippy::cast_precision_loss)]
pub fn coverage_ratio(covered_ns: u64, span_ns: u64) -> f64 {
    if span_ns == 0 {
        return 0.0;
    }
    (covered_ns.min(span_ns) as f64) / (span_ns as f64)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_ratio_is_clamped() {
        assert_eq!(coverage_ratio(5, 10), 0.5);
        assert_eq!(coverage_ratio(20, 10), 1.0);
        assert_eq!(coverage_ratio(5, 0), 0.0);
    }
}
