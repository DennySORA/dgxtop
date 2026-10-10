//! Sparkline trends fed from committed snapshots.
//!
//! Tracked series: `cpu.utilization.total` (host scope), `memory.used`, per
//! GPU entity `gpu.utilization.compute`, `gpu.temperature`,
//! `gpu.power.draw` and the GPU memory in use (framebuffer, or on unified
//! memory the processes' allocations), and per device the disk read/write
//! and network receive/transmit rates the overview charts draw. A point is
//! appended only when the committed sample is a
//! new read (its `read_finished_boot_ns` moved), so commits of other domains do
//! not duplicate points; a committed sample without a value is a gap (`None`).
//! Keys use the committed `MetricKey`, so the presentation can look them up
//! with the key of the sample it renders.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::canonical::ImmutableSnapshot;
use dgxtop_core::identity::MetricKey;
use dgxtop_core::metric::{Scalar, ids};
use dgxtop_core::naming::entity;

use crate::presentation::TrendBook;

/// Points kept per series (4 minutes at the default 1 s interval).
pub(crate) const TREND_POINTS: usize = 240;

#[derive(Debug)]
pub(crate) struct TrendRecorder {
    book: TrendBook,
    step_ms: u64,
    last_read: BTreeMap<MetricKey, u64>,
}

impl TrendRecorder {
    pub(crate) fn new(step_ms: u64) -> Self {
        Self {
            book: TrendBook::new(TREND_POINTS, step_ms),
            step_ms,
            last_read: BTreeMap::new(),
        }
    }

    pub(crate) fn book(&self) -> &TrendBook {
        &self.book
    }

    /// A new sampling interval restarts the trends (points must stay evenly spaced).
    pub(crate) fn set_step(&mut self, step_ms: u64) {
        if step_ms != self.step_ms {
            *self = Self::new(step_ms);
        }
    }

    pub(crate) fn record(&mut self, snapshot: &ImmutableSnapshot) {
        let mut present = BTreeSet::new();
        for domain in snapshot.domains.values() {
            for (key, entry) in &domain.samples {
                if !is_trend_key(key) {
                    continue;
                }
                present.insert(key.clone());
                let read_ns = entry.sample.read_finished_boot_ns;
                if self.last_read.get(key) == Some(&read_ns) {
                    continue;
                }
                self.last_read.insert(key.clone(), read_ns);
                let value = entry
                    .sample
                    .selection_state
                    .has_value()
                    .then_some(entry.sample.value)
                    .flatten()
                    .map(Scalar::to_f64_lossy);
                self.book.push(key, value);
            }
        }
        self.last_read.retain(|key, _| present.contains(key));
        self.book.retain(|key| present.contains(key));
    }
}

fn is_trend_key(key: &MetricKey) -> bool {
    match key.metric_id.as_str() {
        ids::CPU_UTILIZATION_TOTAL => key.scope.as_str() == entity::SCOPE_HOST,
        ids::MEMORY_USED => true,
        ids::GPU_UTILIZATION_COMPUTE
        | ids::GPU_TEMPERATURE
        | ids::GPU_POWER_DRAW
        | ids::GPU_MEMORY_FB_USED
        | ids::GPU_MEMORY_PROCESS_ALLOCATED => entity::parse_gpu(key.entity_id.as_str()).is_some(),
        ids::DISK_READ_BPS | ids::DISK_WRITE_BPS | ids::NET_RX_BPS | ids::NET_TX_BPS => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{committed_snapshot, sample_entry};
    use dgxtop_core::testing::metric_key;

    #[test]
    fn records_only_new_reads_of_tracked_series() {
        let cpu = metric_key("cpu", "cpu.utilization.total", "host");
        let core = metric_key("cpu.core.0", "cpu.utilization.total", "core:0");
        let gpu = metric_key("gpu:GPU-1", "gpu.temperature", "gpu");
        let mut recorder = TrendRecorder::new(1000);
        let first = committed_snapshot(
            1,
            vec![
                ("cpu", sample_entry(&cpu, Some(12.5), 1_000)),
                ("cpu", sample_entry(&core, Some(50.0), 1_000)),
                ("gpu", sample_entry(&gpu, Some(61.0), 1_000)),
            ],
        );
        recorder.record(&first);
        // The same reads committed again (another domain's commit) add nothing.
        recorder.record(&first);
        assert_eq!(recorder.book().trend(&cpu, 10).points, vec![Some(12.5)]);
        assert!(recorder.book().trend(&core, 10).points.is_empty());
        let second = committed_snapshot(
            2,
            vec![
                ("cpu", sample_entry(&cpu, None, 2_000)),
                ("gpu", sample_entry(&gpu, Some(62.0), 2_000)),
            ],
        );
        recorder.record(&second);
        assert_eq!(
            recorder.book().trend(&cpu, 10).points,
            vec![Some(12.5), None],
            "a sample without value is a gap"
        );
        assert_eq!(
            recorder.book().trend(&gpu, 10).points,
            vec![Some(61.0), Some(62.0)]
        );
        assert_eq!(recorder.book().len(), 2);
    }

    #[test]
    fn vanished_series_are_dropped_and_step_change_restarts() {
        let gpu = metric_key("gpu:GPU-1", "gpu.power.draw", "gpu");
        let memory = metric_key("memory", "memory.used", "system");
        let mut recorder = TrendRecorder::new(1000);
        recorder.record(&committed_snapshot(
            1,
            vec![
                ("gpu", sample_entry(&gpu, Some(100.0), 1)),
                ("memory", sample_entry(&memory, Some(5.0), 1)),
            ],
        ));
        recorder.record(&committed_snapshot(
            2,
            vec![("memory", sample_entry(&memory, Some(6.0), 2))],
        ));
        assert!(recorder.book().trend(&gpu, 10).points.is_empty());
        assert_eq!(recorder.book().len(), 1);
        recorder.set_step(1000);
        assert_eq!(recorder.book().len(), 1);
        recorder.set_step(2000);
        assert!(recorder.book().is_empty());
        assert_eq!(recorder.book().trend(&memory, 1).step_ms, 2000);
    }
}
