//! Bounded in-memory diagnostics: counters, gauges and latency histograms.
//!
//! Label values come from small bounded sets (adapter id, read group, reason).
//! Each metric keeps at most [`MAX_LABELS_PER_METRIC`] label values; further
//! values collapse into `other` and increment `diagnostics_labels_dropped`, so
//! 100 000 short-lived PIDs can never create 100 000 series.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::text::sanitize_display;

/// Label values retained per metric before collapsing into `other`.
pub const MAX_LABELS_PER_METRIC: usize = 32;
/// Histogram bucket upper bounds in microseconds (last bucket is +inf).
const BUCKETS_US: [u64; 20] = [
    50, 100, 250, 500, 1_000, 2_000, 4_000, 8_000, 16_000, 33_000, 50_000, 100_000, 200_000,
    500_000, 1_000_000, 2_000_000, 5_000_000, 10_000_000, 30_000_000, 60_000_000,
];

/// Well-known metric names (`OPERATIONS.md` §3, `ARCHITECTURE.md` §15).
pub mod names {
    pub const ADAPTER_READ_DURATION_MS: &str = "adapter_read_duration_ms";
    pub const ADAPTER_TIMEOUT_TOTAL: &str = "adapter_timeout_total";
    pub const COLLECTOR_DURATION_MS: &str = "collector_duration_ms";
    pub const MANAGER_COMMAND_ACK_MS: &str = "manager_command_ack_ms";
    pub const COMMAND_REJECTED_TOTAL: &str = "command_rejected_total";
    pub const COMMIT_TO_PROJECTION_MS: &str = "commit_to_projection_ms";
    pub const STORE_COMMIT_DURATION_MS: &str = "store_commit_duration_ms";
    pub const VIEWMODEL_AGE_MS: &str = "viewmodel_age_ms";
    pub const QUEUE_ITEMS: &str = "queue_items";
    pub const QUEUE_BYTES: &str = "queue_bytes";
    pub const QUEUE_DROPPED_TOTAL: &str = "queue_dropped_total";
    pub const SOURCE_CONFLICT_TOTAL: &str = "source_conflict_total";
    pub const SOURCE_CHANGED_TOTAL: &str = "source_changed_total";
    pub const HISTORY_COVERAGE_RATIO: &str = "history_coverage_ratio";
    pub const ARCHIVE_GAP_TOTAL: &str = "archive_gap_total";
    pub const ACTION_UNKNOWN_TOTAL: &str = "action_unknown_total";
    pub const PROCESS_TREE_RSS_BYTES: &str = "process_tree_rss_bytes";
    pub const OWNED_ALLOCATION_BYTES: &str = "owned_allocation_bytes";
    pub const WORKER_THREADS: &str = "worker_threads";
    pub const OPEN_FDS: &str = "open_fds";
    pub const FRAME_COMPUTE_MS: &str = "frame_compute_ms";
    pub const TTY_FLUSH_MS: &str = "tty_flush_ms";
    pub const STALE_RESULT_DROPPED_TOTAL: &str = "stale_result_dropped_total";
    /// Adapter dispatches a worker refused; the work stays pending and is
    /// retried, so these are deferrals, never dropped items (D-37).
    pub const ADAPTER_DISPATCH_DEFERRED_TOTAL: &str = "adapter_dispatch_deferred_total";
    pub const GAP_RECORDED_TOTAL: &str = "gap_recorded_total";
    pub const OVERLOAD_TRANSITIONS_TOTAL: &str = "overload_transitions_total";
    pub const LOG_DROPPED_TOTAL: &str = "log_dropped_total";
    pub const LABELS_DROPPED: &str = "diagnostics_labels_dropped";
}

/// Fixed-bucket latency histogram (approximate quantiles, exact count/sum/max).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Histogram {
    pub buckets: Vec<u64>,
    pub count: u64,
    pub sum_us: u64,
    pub max_us: u64,
}

impl Histogram {
    fn record_us(&mut self, value_us: u64) {
        if self.buckets.is_empty() {
            self.buckets = vec![0; BUCKETS_US.len() + 1];
        }
        let index = BUCKETS_US
            .iter()
            .position(|bound| value_us <= *bound)
            .unwrap_or(BUCKETS_US.len());
        self.buckets[index] = self.buckets[index].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(value_us);
        self.max_us = self.max_us.max(value_us);
    }

    /// Upper bound (µs) of the bucket containing the nearest-rank quantile.
    pub fn quantile_upper_us(&self, quantile: f64) -> Option<u64> {
        if self.count == 0 || !(0.0..=1.0).contains(&quantile) {
            return None;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let rank = ((quantile * self.count as f64).ceil() as u64).max(1);
        let mut seen = 0u64;
        for (index, count) in self.buckets.iter().enumerate() {
            seen = seen.saturating_add(*count);
            if seen >= rank {
                return Some(BUCKETS_US.get(index).copied().unwrap_or(self.max_us));
            }
        }
        Some(self.max_us)
    }
}

#[derive(Debug, Default)]
struct Registry {
    counters: BTreeMap<&'static str, BTreeMap<String, u64>>,
    gauges: BTreeMap<&'static str, BTreeMap<String, f64>>,
    histograms: BTreeMap<&'static str, BTreeMap<String, Histogram>>,
    labels_dropped: u64,
}

fn bounded_label<V>(map: &BTreeMap<String, V>, label: &str, dropped: &mut u64) -> String {
    let clean = sanitize_display(label, 64);
    if map.contains_key(&clean) || map.len() < MAX_LABELS_PER_METRIC {
        clean
    } else {
        *dropped = dropped.saturating_add(1);
        "other".to_owned()
    }
}

/// Shared diagnostics registry (cheap to clone).
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    inner: Arc<Mutex<Registry>>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    fn with<R>(&self, apply: impl FnOnce(&mut Registry) -> R) -> R {
        let mut registry = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        apply(&mut registry)
    }

    pub fn increment(&self, metric: &'static str, label: &str, by: u64) {
        self.with(|registry| {
            let mut dropped = registry.labels_dropped;
            let series = registry.counters.entry(metric).or_default();
            let key = bounded_label(series, label, &mut dropped);
            let value = series.entry(key).or_insert(0);
            *value = value.saturating_add(by);
            registry.labels_dropped = dropped;
        });
    }

    pub fn set_gauge(&self, metric: &'static str, label: &str, value: f64) {
        if !value.is_finite() {
            return;
        }
        self.with(|registry| {
            let mut dropped = registry.labels_dropped;
            let series = registry.gauges.entry(metric).or_default();
            let key = bounded_label(series, label, &mut dropped);
            series.insert(key, value);
            registry.labels_dropped = dropped;
        });
    }

    pub fn observe_us(&self, metric: &'static str, label: &str, value_us: u64) {
        self.with(|registry| {
            let mut dropped = registry.labels_dropped;
            let series = registry.histograms.entry(metric).or_default();
            let key = bounded_label(series, label, &mut dropped);
            series.entry(key).or_default().record_us(value_us);
            registry.labels_dropped = dropped;
        });
    }

    pub fn observe_duration(
        &self,
        metric: &'static str,
        label: &str,
        duration: std::time::Duration,
    ) {
        self.observe_us(
            metric,
            label,
            u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
        );
    }

    pub fn counter(&self, metric: &'static str, label: &str) -> u64 {
        self.with(|registry| {
            registry
                .counters
                .get(metric)
                .and_then(|series| series.get(label))
                .copied()
                .unwrap_or(0)
        })
    }

    pub fn snapshot(&self) -> DiagnosticsSnapshot {
        self.with(|registry| DiagnosticsSnapshot {
            counters: registry
                .counters
                .iter()
                .map(|(metric, series)| ((*metric).to_owned(), series.clone()))
                .collect(),
            gauges: registry
                .gauges
                .iter()
                .map(|(metric, series)| ((*metric).to_owned(), series.clone()))
                .collect(),
            histograms: registry
                .histograms
                .iter()
                .map(|(metric, series)| ((*metric).to_owned(), series.clone()))
                .collect(),
            labels_dropped: registry.labels_dropped,
        })
    }
}

/// Serializable diagnostics export (`--diagnostics-json`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DiagnosticsSnapshot {
    pub counters: BTreeMap<String, BTreeMap<String, u64>>,
    pub gauges: BTreeMap<String, BTreeMap<String, f64>>,
    pub histograms: BTreeMap<String, BTreeMap<String, Histogram>>,
    pub labels_dropped: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_bounded() {
        let diagnostics = Diagnostics::new();
        for pid in 0..100_000u32 {
            diagnostics.increment(names::QUEUE_DROPPED_TOTAL, &pid.to_string(), 1);
        }
        let snapshot = diagnostics.snapshot();
        let series = &snapshot.counters[names::QUEUE_DROPPED_TOTAL];
        assert_eq!(series.len(), MAX_LABELS_PER_METRIC + 1);
        assert_eq!(series["other"], 100_000 - MAX_LABELS_PER_METRIC as u64);
        assert_eq!(
            snapshot.labels_dropped,
            100_000 - MAX_LABELS_PER_METRIC as u64
        );
    }

    #[test]
    fn labels_are_sanitized() {
        let diagnostics = Diagnostics::new();
        diagnostics.increment(names::COMMAND_REJECTED_TOTAL, "busy\u{1b}[31m", 2);
        assert_eq!(
            diagnostics.counter(names::COMMAND_REJECTED_TOTAL, "busy"),
            2
        );
    }

    #[test]
    fn histogram_quantiles() {
        let diagnostics = Diagnostics::new();
        for value in 1..=100u64 {
            diagnostics.observe_us(names::FRAME_COMPUTE_MS, "render", value * 100);
        }
        let snapshot = diagnostics.snapshot();
        let histogram = &snapshot.histograms[names::FRAME_COMPUTE_MS]["render"];
        assert_eq!(histogram.count, 100);
        assert_eq!(histogram.max_us, 10_000);
        let p95 = histogram.quantile_upper_us(0.95).unwrap();
        assert!((9_500..=16_000).contains(&p95), "p95 bucket {p95}");
        assert!(Histogram::default().quantile_upper_us(0.5).is_none());
    }

    #[test]
    fn non_finite_gauges_are_ignored() {
        let diagnostics = Diagnostics::new();
        diagnostics.set_gauge(names::HISTORY_COVERAGE_RATIO, "cpu", f64::NAN);
        assert!(diagnostics.snapshot().gauges.is_empty());
    }
}
