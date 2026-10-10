//! Latency evidence: nearest-rank summaries over *all* raw samples, the
//! start/finish matching of asynchronous requests (commands, history jobs)
//! and the "adapter read finished → projection published" tracker.
//!
//! Everything is bounded: a request that never finishes is evicted and
//! counted, never kept forever, and a sample is never invented for it.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::nearest_rank;

/// Nearest-rank summary of one latency stream (milliseconds).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LatencySummary {
    pub samples: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
}

/// Summary of raw microsecond samples; `None` when there is no sample.
#[allow(clippy::cast_precision_loss)]
pub fn summarize_us(samples_us: &[u64]) -> Option<LatencySummary> {
    if samples_us.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = samples_us
        .iter()
        .map(|value| *value as f64 / 1000.0)
        .collect();
    sorted.sort_by(f64::total_cmp);
    let total: f64 = sorted.iter().sum();
    Some(LatencySummary {
        samples: u64::try_from(sorted.len()).unwrap_or(u64::MAX),
        p50_ms: nearest_rank(&sorted, 0.50)?,
        p95_ms: nearest_rank(&sorted, 0.95)?,
        p99_ms: nearest_rank(&sorted, 0.99)?,
        max_ms: sorted.last().copied()?,
        mean_ms: total / sorted.len() as f64,
    })
}

/// Microseconds of a duration, saturating.
pub fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// Requests started but not yet answered, keyed by request id.
#[derive(Debug)]
pub struct PendingRequests<K> {
    started: HashMap<K, Instant>,
    capacity: usize,
    evicted: u64,
}

impl<K: Eq + Hash + Clone> PendingRequests<K> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            started: HashMap::new(),
            capacity: capacity.max(1),
            evicted: 0,
        }
    }

    /// Records a start; when full, the oldest pending request is evicted
    /// (and counted) so memory stays bounded.
    pub fn start(&mut self, key: K, at: Instant) {
        if self.started.len() >= self.capacity && !self.started.contains_key(&key) {
            let oldest = self
                .started
                .iter()
                .min_by_key(|(_, started)| **started)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.started.remove(&oldest);
                self.evicted = self.evicted.saturating_add(1);
            }
        }
        self.started.insert(key, at);
    }

    /// Elapsed time of a finished request; `None` for unknown keys.
    pub fn finish(&mut self, key: &K, at: Instant) -> Option<Duration> {
        self.started
            .remove(key)
            .map(|started| at.saturating_duration_since(started))
    }

    /// Longest wait among the pending requests at `now`.
    pub fn oldest_wait(&self, now: Instant) -> Option<Duration> {
        self.started
            .values()
            .map(|started| now.saturating_duration_since(*started))
            .max()
    }

    pub fn len(&self) -> usize {
        self.started.len()
    }

    pub fn is_empty(&self) -> bool {
        self.started.is_empty()
    }

    /// Requests dropped from the table without an answer.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }
}

/// "Adapter read finished → Manager published the projection holding it".
///
/// Each committed snapshot revision remembers the newest `read_finished`
/// of its inputs; a published projection resolves every pending revision
/// up to its `snapshot_revision` with `generated − read_finished`. Frame
/// pacing and queueing are therefore included, as `PERFORMANCE.md` asks.
#[derive(Debug)]
pub struct ProjectionLatencyTracker {
    pending: BTreeMap<u64, u64>,
    capacity: usize,
    samples_us: Vec<u64>,
    evicted: u64,
    clock_skew: u64,
}

impl ProjectionLatencyTracker {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            pending: BTreeMap::new(),
            capacity: capacity.max(1),
            samples_us: Vec::new(),
            evicted: 0,
            clock_skew: 0,
        }
    }

    /// A data commit became snapshot `revision`; its newest input finished
    /// reading at `read_finished_boot_ns`.
    pub fn committed(&mut self, revision: u64, read_finished_boot_ns: u64) {
        let entry = self
            .pending
            .entry(revision)
            .or_insert(read_finished_boot_ns);
        *entry = (*entry).max(read_finished_boot_ns);
        while self.pending.len() > self.capacity {
            self.pending.pop_first();
            self.evicted = self.evicted.saturating_add(1);
        }
    }

    /// A projection of `snapshot_revision` was generated at `generated_boot_ns`.
    pub fn published(&mut self, snapshot_revision: u64, generated_boot_ns: u64) {
        let newer = self.pending.split_off(&snapshot_revision.saturating_add(1));
        let resolved = std::mem::replace(&mut self.pending, newer);
        for read_finished in resolved.into_values() {
            match generated_boot_ns.checked_sub(read_finished) {
                Some(elapsed_ns) => self.samples_us.push(elapsed_ns / 1_000),
                // A projection cannot predate its data; count, never clamp to 0.
                None => self.clock_skew = self.clock_skew.saturating_add(1),
            }
        }
    }

    /// Drops revisions committed before `cutoff_revision` without a sample
    /// (used to discard warm-up data); returns how many were dropped.
    pub fn forget_through(&mut self, cutoff_revision: u64) -> usize {
        let kept = self.pending.split_off(&cutoff_revision.saturating_add(1));
        std::mem::replace(&mut self.pending, kept).len()
    }

    pub fn take_samples(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.samples_us)
    }

    /// Revisions still waiting for a projection.
    pub fn unresolved(&self) -> usize {
        self.pending.len()
    }

    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Projections whose generation time preceded their data's read time.
    pub fn clock_skew(&self) -> u64 {
        self.clock_skew
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_uses_nearest_rank_over_all_samples() {
        let samples: Vec<u64> = (1..=100).map(|ms| ms * 1000).collect();
        let summary = summarize_us(&samples).unwrap();
        assert_eq!(summary.samples, 100);
        assert_eq!(summary.p50_ms, 50.0);
        assert_eq!(summary.p95_ms, 95.0);
        assert_eq!(summary.p99_ms, 99.0);
        assert_eq!(summary.max_ms, 100.0);
        assert!((summary.mean_ms - 50.5).abs() < 1e-9);
        assert!(summarize_us(&[]).is_none());
        assert_eq!(summarize_us(&[1500]).unwrap().p99_ms, 1.5);
    }

    #[test]
    fn pending_requests_match_and_stay_bounded() {
        let origin = Instant::now();
        let mut pending = PendingRequests::with_capacity(2);
        pending.start(1, origin);
        pending.start(2, origin + Duration::from_millis(1));
        assert_eq!(
            pending.finish(&1, origin + Duration::from_millis(5)),
            Some(Duration::from_millis(5))
        );
        assert_eq!(pending.finish(&1, origin), None, "answered only once");
        pending.start(3, origin + Duration::from_millis(2));
        pending.start(4, origin + Duration::from_millis(3));
        assert_eq!(pending.len(), 2);
        assert_eq!(pending.evicted(), 1, "oldest (2) evicted, not answered");
        assert_eq!(pending.finish(&2, origin), None);
        assert_eq!(
            pending.oldest_wait(origin + Duration::from_millis(10)),
            Some(Duration::from_millis(8))
        );
    }

    #[test]
    fn projection_latency_includes_waiting_for_the_next_frame() {
        let mut tracker = ProjectionLatencyTracker::with_capacity(16);
        tracker.committed(5, 1_000_000);
        tracker.committed(6, 2_000_000);
        tracker.committed(6, 3_000_000);
        tracker.published(5, 4_000_000);
        tracker.committed(7, 5_000_000);
        tracker.published(7, 10_000_000);
        assert_eq!(tracker.take_samples(), vec![3_000, 7_000, 5_000]);
        assert_eq!(tracker.unresolved(), 0);
        tracker.published(8, 11_000_000);
        assert!(
            tracker.take_samples().is_empty(),
            "nothing pending, nothing made up"
        );
    }

    #[test]
    fn projection_tracker_counts_skew_and_bounds_memory() {
        let mut tracker = ProjectionLatencyTracker::with_capacity(2);
        tracker.committed(1, 50);
        tracker.committed(2, 60);
        tracker.committed(3, 70);
        assert_eq!(tracker.evicted(), 1);
        tracker.published(3, 65);
        assert_eq!(tracker.take_samples(), vec![0]);
        assert_eq!(tracker.clock_skew(), 1);
        tracker.committed(9, 10);
        tracker.committed(10, 10);
        assert_eq!(tracker.forget_through(9), 1);
        assert_eq!(tracker.unresolved(), 1);
    }
}
