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

use dgxtop_core::canonical::{CanonicalSample, CatalogSnapshot, ImmutableSnapshot};
use dgxtop_core::identity::{EntityId, MetricId, MetricKey};
use dgxtop_core::metric::{Scalar, ids};
use dgxtop_core::naming::entity;
use dgxtop_core::observation::EntityKind;

use crate::presentation::TrendBook;
use crate::presentation::gpu_directory::is_unified_memory;

/// Metric id of the RAM unified-memory GPUs hold, recorded beside
/// `memory.used` (not a collected metric).
pub(crate) const GPU_RESIDENT_TREND: &str = "memory.used.gpu_resident";

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

    /// Appends the new reads of `snapshot`; `unified_gpus` are the GPUs that
    /// allocate from system RAM, whose share follows `memory.used`.
    pub(crate) fn record(
        &mut self,
        snapshot: &ImmutableSnapshot,
        unified_gpus: &BTreeSet<EntityId>,
    ) {
        let mut present = BTreeSet::new();
        let mut new_memory_reads = Vec::new();
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
                if key.metric_id.as_str() == ids::MEMORY_USED {
                    new_memory_reads.push(key.clone());
                }
                self.book.push(key, sample_value(&entry.sample));
            }
        }
        if !unified_gpus.is_empty() {
            self.record_gpu_share(snapshot, unified_gpus, &new_memory_reads, &mut present);
        }
        self.last_read.retain(|key, _| present.contains(key));
        self.book.retain(|key| present.contains(key));
    }

    /// One point of the unified GPUs' share beside each new `memory.used`
    /// point, so both series share their time axis.
    fn record_gpu_share(
        &mut self,
        snapshot: &ImmutableSnapshot,
        unified_gpus: &BTreeSet<EntityId>,
        new_memory_reads: &[MetricKey],
        present: &mut BTreeSet<MetricKey>,
    ) {
        let used_keys = present
            .iter()
            .filter(|key| key.metric_id.as_str() == ids::MEMORY_USED)
            .filter_map(gpu_resident_key)
            .collect::<Vec<_>>();
        present.extend(used_keys);
        let share = unified_share(snapshot, unified_gpus);
        for used in new_memory_reads {
            if let Some(key) = gpu_resident_key(used) {
                self.book.push(&key, share);
            }
        }
    }
}

/// The GPUs of `catalog` that allocate from system RAM.
pub(crate) fn unified_gpus(catalog: &CatalogSnapshot) -> BTreeSet<EntityId> {
    catalog
        .entities
        .values()
        .filter(|record| record.kind == EntityKind::Gpu && is_unified_memory(record))
        .map(|record| record.entity_id.clone())
        .collect()
}

/// The key of the RAM unified GPUs hold, beside the `memory.used` key.
pub(crate) fn gpu_resident_key(used: &MetricKey) -> Option<MetricKey> {
    Some(MetricKey {
        metric_id: MetricId::new(GPU_RESIDENT_TREND).ok()?,
        ..used.clone()
    })
}

/// The sum of what the processes of `unified_gpus` hold; `None` when any GPU
/// has no value, never a partial sum.
fn unified_share(snapshot: &ImmutableSnapshot, unified_gpus: &BTreeSet<EntityId>) -> Option<f64> {
    let mut found = BTreeSet::new();
    let mut sum = 0.0;
    for domain in snapshot.domains.values() {
        for (key, entry) in &domain.samples {
            if key.metric_id.as_str() == ids::GPU_MEMORY_PROCESS_ALLOCATED
                && unified_gpus.contains(&key.entity_id)
                && found.insert(&key.entity_id)
            {
                sum += sample_value(&entry.sample)?;
            }
        }
    }
    (found.len() == unified_gpus.len()).then_some(sum)
}

/// The sample's value when its selection has one.
fn sample_value(sample: &CanonicalSample) -> Option<f64> {
    sample
        .selection_state
        .has_value()
        .then_some(sample.value)
        .flatten()
        .map(Scalar::to_f64_lossy)
        .filter(|value| value.is_finite())
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
        recorder.record(&first, &BTreeSet::new());
        // The same reads committed again (another domain's commit) add nothing.
        recorder.record(&first, &BTreeSet::new());
        assert_eq!(recorder.book().trend(&cpu, 10).points, vec![Some(12.5)]);
        assert!(recorder.book().trend(&core, 10).points.is_empty());
        let second = committed_snapshot(
            2,
            vec![
                ("cpu", sample_entry(&cpu, None, 2_000)),
                ("gpu", sample_entry(&gpu, Some(62.0), 2_000)),
            ],
        );
        recorder.record(&second, &BTreeSet::new());
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
        recorder.record(
            &committed_snapshot(
                1,
                vec![
                    ("gpu", sample_entry(&gpu, Some(100.0), 1)),
                    ("memory", sample_entry(&memory, Some(5.0), 1)),
                ],
            ),
            &BTreeSet::new(),
        );
        recorder.record(
            &committed_snapshot(2, vec![("memory", sample_entry(&memory, Some(6.0), 2))]),
            &BTreeSet::new(),
        );
        assert!(recorder.book().trend(&gpu, 10).points.is_empty());
        assert_eq!(recorder.book().len(), 1);
        recorder.set_step(1000);
        assert_eq!(recorder.book().len(), 1);
        recorder.set_step(2000);
        assert!(recorder.book().is_empty());
        assert_eq!(recorder.book().trend(&memory, 1).step_ms, 2000);
    }

    /// The RAM unified GPUs hold gets one point beside each new point of
    /// `memory.used`, so the memory chart can draw both on one time axis
    /// even when the GPU's processes are read at another cadence.
    #[test]
    fn the_unified_gpu_share_follows_each_point_of_used_memory() {
        let memory = metric_key("memory", "memory.used", "system");
        let unified = metric_key("gpu:GPU-1", "gpu.memory.process_allocated", "gpu");
        let other = metric_key("gpu:GPU-2", "gpu.memory.process_allocated", "gpu");
        let discrete = metric_key("gpu:GPU-3", "gpu.memory.process_allocated", "gpu");
        let gpus: BTreeSet<EntityId> = [unified.entity_id.clone(), other.entity_id.clone()].into();
        let share = gpu_resident_key(&memory).unwrap();
        let mut recorder = TrendRecorder::new(1000);
        recorder.record(
            &committed_snapshot(
                1,
                vec![
                    ("memory", sample_entry(&memory, Some(50.0), 1)),
                    ("gpu", sample_entry(&unified, Some(20.0), 1)),
                    ("gpu", sample_entry(&other, Some(5.0), 1)),
                    ("gpu", sample_entry(&discrete, Some(99.0), 1)),
                ],
            ),
            &gpus,
        );
        // A GPU read alone adds no point; the next memory read carries the
        // GPUs' latest values.
        recorder.record(
            &committed_snapshot(
                2,
                vec![
                    ("memory", sample_entry(&memory, Some(50.0), 1)),
                    ("gpu", sample_entry(&unified, Some(30.0), 2)),
                    ("gpu", sample_entry(&other, Some(5.0), 1)),
                ],
            ),
            &gpus,
        );
        // A GPU without a value makes the share a gap, never a partial sum.
        recorder.record(
            &committed_snapshot(
                3,
                vec![
                    ("memory", sample_entry(&memory, Some(60.0), 3)),
                    ("gpu", sample_entry(&unified, Some(30.0), 2)),
                    ("gpu", sample_entry(&other, None, 3)),
                ],
            ),
            &gpus,
        );
        assert_eq!(
            recorder.book().trend(&memory, 10).points,
            vec![Some(50.0), Some(60.0)]
        );
        assert_eq!(
            recorder.book().trend(&share, 10).points,
            vec![Some(25.0), None]
        );
        // Without a unified GPU there is no share series.
        let mut plain = TrendRecorder::new(1000);
        plain.record(
            &committed_snapshot(1, vec![("memory", sample_entry(&memory, Some(5.0), 1))]),
            &BTreeSet::new(),
        );
        assert!(plain.book().trend(&share, 10).points.is_empty());
    }
}
