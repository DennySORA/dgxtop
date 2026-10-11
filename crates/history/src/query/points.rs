//! Bounded point output with gap merging.

use dgxtop_core::history::{HistoryMethod, HistoryPoint};
use dgxtop_core::identity::SourceId;
use dgxtop_core::metric::Scalar;

use super::MAX_POINT_SOURCES;

/// Up to [`MAX_POINT_SOURCES`] distinct sources contributing to one point.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SourceSet {
    sources: Vec<SourceId>,
}

impl SourceSet {
    pub(crate) fn insert(&mut self, source: &SourceId) {
        if self.sources.len() < MAX_POINT_SOURCES && !self.sources.contains(source) {
            self.sources.push(source.clone());
        }
    }

    pub(crate) fn into_vec(self) -> Vec<SourceId> {
        self.sources
    }
}

/// A point with a value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DataPoint {
    pub start_ns: u64,
    pub end_ns: u64,
    pub value: Scalar,
    pub covered_ns: u64,
    pub method: HistoryMethod,
    pub sources: SourceSet,
    pub reason: Option<&'static str>,
}

/// Collects points up to a cap; remembers where the first rejected point
/// started so raw results can be paged.
pub(crate) struct PointSink {
    points: Vec<HistoryPoint>,
    cap: usize,
    overflow_start_ns: Option<u64>,
}

impl PointSink {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            points: Vec::new(),
            cap: cap.max(1),
            overflow_start_ns: None,
        }
    }

    /// Start of the first point that did not fit, if any.
    pub(crate) fn overflow_start(&self) -> Option<u64> {
        self.overflow_start_ns
    }

    fn admit(&mut self, start_ns: u64) -> bool {
        if self.overflow_start_ns.is_some() {
            return false;
        }
        if self.points.len() >= self.cap {
            self.overflow_start_ns = Some(start_ns);
            return false;
        }
        true
    }

    pub(crate) fn push_data(&mut self, point: DataPoint) {
        if point.end_ns <= point.start_ns || !self.admit(point.start_ns) {
            return;
        }
        self.points.push(HistoryPoint {
            start_boot_ns: point.start_ns,
            end_boot_ns: point.end_ns,
            value: Some(point.value),
            covered_ns: point.covered_ns,
            method: point.method,
            source_ids: point.sources.into_vec(),
            reason: point.reason.map(str::to_owned),
        });
    }

    /// Adds a point without value; merges with a contiguous previous one of
    /// the same method and reason.
    pub(crate) fn push_null(
        &mut self,
        start_ns: u64,
        end_ns: u64,
        method: HistoryMethod,
        reason: &str,
    ) {
        if end_ns <= start_ns || self.overflow_start_ns.is_some() {
            return;
        }
        if let Some(previous) = self.points.last_mut()
            && previous.value.is_none()
            && previous.method == method
            && previous.end_boot_ns == start_ns
            && previous.reason.as_deref() == Some(reason)
        {
            previous.end_boot_ns = end_ns;
            return;
        }
        if !self.admit(start_ns) {
            return;
        }
        self.points.push(HistoryPoint {
            start_boot_ns: start_ns,
            end_boot_ns: end_ns,
            value: None,
            covered_ns: 0,
            method,
            source_ids: Vec::new(),
            reason: Some(reason.to_owned()),
        });
    }

    pub(crate) fn into_points(self) -> Vec<HistoryPoint> {
        self.points
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::source_id;

    fn data(start: u64, end: u64) -> DataPoint {
        let mut sources = SourceSet::default();
        sources.insert(&source_id("a"));
        DataPoint {
            start_ns: start,
            end_ns: end,
            value: Scalar::from_u64(1),
            covered_ns: end - start,
            method: HistoryMethod::Raw,
            sources,
            reason: None,
        }
    }

    #[test]
    fn contiguous_gaps_merge_and_cap_records_overflow() {
        let mut sink = PointSink::new(3);
        sink.push_data(data(0, 10));
        sink.push_null(10, 20, HistoryMethod::Gap, "stale");
        sink.push_null(20, 30, HistoryMethod::Gap, "stale");
        sink.push_null(30, 40, HistoryMethod::Gap, "timeout");
        sink.push_data(data(40, 50));
        assert_eq!(sink.overflow_start(), Some(40));
        sink.push_data(data(50, 60));
        let points = sink.into_points();
        assert_eq!(points.len(), 3);
        assert_eq!((points[1].start_boot_ns, points[1].end_boot_ns), (10, 30));
        assert_eq!(points[2].reason.as_deref(), Some("timeout"));
        assert_eq!(points[0].source_ids, vec![source_id("a")]);
    }

    #[test]
    fn source_sets_are_bounded_and_distinct() {
        let mut set = SourceSet::default();
        for name in ["a", "b", "a", "c", "d", "e"] {
            set.insert(&source_id(name));
        }
        assert_eq!(set.into_vec().len(), MAX_POINT_SOURCES);
    }
}
