//! The short trends the state fills at commit (D-79, D-99): the overview's
//! sparkline series, read by the Manager's presentation through
//! `TrendReader`.
//!
//! Tracked series: `cpu.utilization.total` (host scope), `memory.used`, per
//! GPU entity `gpu.utilization.compute`, `gpu.temperature`,
//! `gpu.power.draw` and the GPU memory in use (framebuffer, or on unified
//! memory the processes' allocations), and per device the disk read/write
//! and network receive/transmit rates the overview charts draw. A point is
//! appended only when the committed sample is a new read (its
//! `read_finished_boot_ns` moved), so commits of other domains do not
//! duplicate points; a committed sample without a value is a gap. Keys are
//! the committed `MetricKey`s. Beside each new `memory.used` point go each
//! GPU's process allocations and what each process of the process table
//! holds on each GPU (D-64), so the memory chart draws the RAM unified
//! GPUs hold, and each tracked process's part of it, on its time axis; the
//! reader picks the GPUs that share the RAM.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use dgxtop_core::canonical::{CanonicalSample, ImmutableSnapshot};
use dgxtop_core::identity::{EntityId, MetricKey, ProcessKey};
use dgxtop_core::metric::{Scalar, ids};
use dgxtop_core::naming::entity;
use dgxtop_core::ports::TrendReader;
use dgxtop_core::trends::{TREND_POINTS, TrendBook};
use dgxtop_core::view::Trend;

/// Read side of the trends (`TrendReader`): the book behind a lock held
/// only while one series is copied. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct SharedTrends {
    book: Arc<RwLock<TrendBook>>,
}

impl SharedTrends {
    fn read(&self) -> RwLockReadGuard<'_, TrendBook> {
        self.book.read().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TrendReader for SharedTrends {
    fn trend(&self, key: &MetricKey, width: usize) -> Trend {
        self.read().trend(key, width)
    }

    fn gpu_memory_beside(
        &self,
        used: &MetricKey,
        gpus: &BTreeSet<EntityId>,
        width: usize,
    ) -> Trend {
        self.read().gpu_memory_beside(used, gpus, width)
    }

    fn process_memory_beside(
        &self,
        used: &MetricKey,
        process: &ProcessKey,
        gpus: &BTreeSet<EntityId>,
        width: usize,
    ) -> Trend {
        self.read()
            .process_memory_beside(used, process, gpus, width)
    }
}

/// Write side, owned by the state writer.
#[derive(Debug)]
pub(crate) struct TrendRecorder {
    shared: SharedTrends,
    step_ns: u64,
    last_read: BTreeMap<MetricKey, u64>,
}

impl TrendRecorder {
    pub(crate) fn new(step_ns: u64) -> Self {
        Self {
            shared: SharedTrends::default(),
            step_ns,
            last_read: BTreeMap::new(),
        }
    }

    pub(crate) fn reader(&self) -> SharedTrends {
        self.shared.clone()
    }

    /// A new sampling interval restarts the trends (points must stay
    /// evenly spaced).
    pub(crate) fn set_step(&mut self, step_ns: u64) {
        if step_ns != self.step_ns {
            self.step_ns = step_ns;
            self.last_read.clear();
            *self.write() = TrendBook::new(TREND_POINTS);
        }
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, TrendBook> {
        self.shared
            .book
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Appends the new reads of the committed `snapshot`.
    pub(crate) fn record(&mut self, snapshot: &ImmutableSnapshot) {
        let mut present = BTreeSet::new();
        let mut points = Vec::new();
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
                points.push((key.clone(), sample_value(&entry.sample)));
            }
        }
        let beside = (!new_memory_reads.is_empty())
            .then(|| (gpu_allocations(snapshot), process_holdings(snapshot)));
        self.last_read.retain(|key, _| present.contains(key));
        let mut book = self.write();
        for (key, value) in points {
            book.push(&key, value);
        }
        if let Some((gpus, processes)) = beside {
            for used in &new_memory_reads {
                book.push_beside_memory(used, &gpus, &processes);
            }
        }
        book.retain(|key| present.contains(key));
    }
}

/// Each GPU's committed `gpu.memory.process_allocated` (`None` without a
/// value).
fn gpu_allocations(snapshot: &ImmutableSnapshot) -> BTreeMap<EntityId, Option<f64>> {
    let mut gpus = BTreeMap::new();
    for domain in snapshot.domains.values() {
        for (key, entry) in &domain.samples {
            if key.metric_id.as_str() == ids::GPU_MEMORY_PROCESS_ALLOCATED
                && entity::parse_gpu(key.entity_id.as_str()).is_some()
            {
                gpus.entry(key.entity_id.clone())
                    .or_insert_with(|| sample_value(&entry.sample));
            }
        }
    }
    gpus
}

/// What each process of the committed table holds on each GPU, bytes above
/// zero.
fn process_holdings(snapshot: &ImmutableSnapshot) -> BTreeMap<(ProcessKey, EntityId), u64> {
    let mut held: BTreeMap<(ProcessKey, EntityId), u64> = BTreeMap::new();
    for row in &snapshot.process_table.rows {
        for link in &row.gpu {
            let Some(bytes) = link.memory_bytes.filter(|bytes| *bytes > 0) else {
                continue;
            };
            let total = held
                .entry((row.key.clone(), link.gpu_entity_id.clone()))
                .or_default();
            *total = total.saturating_add(bytes);
        }
    }
    held
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
    use std::sync::Arc;

    use super::*;
    use dgxtop_core::testing::{
        gpu_process, metric_key, process_table, snapshot_entry, snapshot_of, test_process_key,
    };

    const SECOND: u64 = 1_000_000_000;
    const GIB: u64 = 1 << 30;

    #[test]
    fn records_only_new_reads_of_tracked_series() {
        let cpu = metric_key("cpu", "cpu.utilization.total", "host");
        let core = metric_key("cpu.core.0", "cpu.utilization.total", "core:0");
        let gpu = metric_key("gpu:GPU-1", "gpu.temperature", "gpu");
        let mut recorder = TrendRecorder::new(SECOND);
        let trends = recorder.reader();
        let first = snapshot_of(
            1,
            vec![
                ("cpu", snapshot_entry(&cpu, Some(12.5), 1_000)),
                ("cpu", snapshot_entry(&core, Some(50.0), 1_000)),
                ("gpu", snapshot_entry(&gpu, Some(61.0), 1_000)),
            ],
        );
        recorder.record(&first);
        // The same reads committed again (another domain's commit) add nothing.
        recorder.record(&first);
        assert_eq!(trends.trend(&cpu, 10).points, vec![Some(12.5)]);
        assert!(trends.trend(&core, 10).points.is_empty());
        let second = snapshot_of(
            2,
            vec![
                ("cpu", snapshot_entry(&cpu, None, 2_000)),
                ("gpu", snapshot_entry(&gpu, Some(62.0), 2_000)),
            ],
        );
        recorder.record(&second);
        assert_eq!(
            trends.trend(&cpu, 10).points,
            vec![Some(12.5), None],
            "a sample without value is a gap"
        );
        assert_eq!(trends.trend(&gpu, 10).points, vec![Some(61.0), Some(62.0)]);
        assert_eq!(trends.read().len(), 2);
    }

    #[test]
    fn vanished_series_are_dropped_and_step_change_restarts() {
        let gpu = metric_key("gpu:GPU-1", "gpu.power.draw", "gpu");
        let memory = metric_key("memory", "memory.used", "system");
        let mut recorder = TrendRecorder::new(SECOND);
        let trends = recorder.reader();
        recorder.record(&snapshot_of(
            1,
            vec![
                ("gpu", snapshot_entry(&gpu, Some(100.0), 1)),
                ("memory", snapshot_entry(&memory, Some(5.0), 1)),
            ],
        ));
        recorder.record(&snapshot_of(
            2,
            vec![("memory", snapshot_entry(&memory, Some(6.0), 2))],
        ));
        assert!(trends.trend(&gpu, 10).points.is_empty());
        assert_eq!(trends.read().len(), 1);
        recorder.set_step(SECOND);
        assert_eq!(trends.read().len(), 1);
        recorder.set_step(2 * SECOND);
        assert!(trends.read().is_empty());
        // After a restart the same read is a new point again.
        recorder.record(&snapshot_of(
            3,
            vec![("memory", snapshot_entry(&memory, Some(6.0), 2))],
        ));
        assert_eq!(trends.trend(&memory, 10).points, vec![Some(6.0)]);
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
        let mut recorder = TrendRecorder::new(SECOND);
        let trends = recorder.reader();
        recorder.record(&snapshot_of(
            1,
            vec![
                ("memory", snapshot_entry(&memory, Some(50.0), 1)),
                ("gpu", snapshot_entry(&unified, Some(20.0), 1)),
                ("gpu", snapshot_entry(&other, Some(5.0), 1)),
                ("gpu", snapshot_entry(&discrete, Some(99.0), 1)),
            ],
        ));
        // A GPU read alone adds no point; the next memory read carries the
        // GPUs' latest values.
        recorder.record(&snapshot_of(
            2,
            vec![
                ("memory", snapshot_entry(&memory, Some(50.0), 1)),
                ("gpu", snapshot_entry(&unified, Some(30.0), 2)),
                ("gpu", snapshot_entry(&other, Some(5.0), 1)),
            ],
        ));
        // A GPU without a value makes the share a gap, never a partial sum.
        recorder.record(&snapshot_of(
            3,
            vec![
                ("memory", snapshot_entry(&memory, Some(60.0), 3)),
                ("gpu", snapshot_entry(&unified, Some(30.0), 2)),
                ("gpu", snapshot_entry(&other, None, 3)),
            ],
        ));
        assert_eq!(
            trends.trend(&memory, 10).points,
            vec![Some(50.0), Some(60.0)]
        );
        assert_eq!(
            trends.gpu_memory_beside(&memory, &gpus, 10).points,
            vec![Some(25.0), None]
        );
        // Without a unified GPU there is no share series.
        assert!(
            trends
                .gpu_memory_beside(&memory, &BTreeSet::new(), 10)
                .points
                .is_empty()
        );
    }

    /// Beside each new `memory.used` point goes what each process holds on
    /// each GPU (D-64): a process gets its part once it holds memory on the
    /// GPUs asked, keeps it with a gap while it holds none, and a process
    /// only on another GPU has none there.
    #[test]
    fn each_process_has_its_part_beside_used_memory() {
        let memory = metric_key("memory", "memory.used", "system");
        let unified: BTreeSet<EntityId> = [EntityId::new("gpu:GPU-1").unwrap()].into();
        let mut recorder = TrendRecorder::new(SECOND);
        let trends = recorder.reader();
        let mut step = |read: u64, train: Option<u64>, render: Option<u64>| {
            let mut snapshot = snapshot_of(
                read,
                vec![("memory", snapshot_entry(&memory, Some(50.0), read))],
            );
            let mut rows = Vec::new();
            if let Some(bytes) = train {
                rows.push(gpu_process(10, "train", &[("gpu:GPU-1", Some(bytes))]));
            }
            if let Some(bytes) = render {
                rows.push(gpu_process(11, "render", &[("gpu:GPU-2", Some(bytes))]));
            }
            snapshot.process_table = Arc::new(process_table(read, read * SECOND, rows));
            recorder.record(&snapshot);
        };
        step(1, Some(6 * GIB), Some(2 * GIB));
        step(2, None, None);
        step(3, Some(7 * GIB), None);
        #[allow(clippy::cast_precision_loss)]
        let gib = |value: u64| Some((value * GIB) as f64);
        let part = |pid| {
            trends
                .process_memory_beside(&memory, &test_process_key(pid), &unified, 10)
                .points
        };
        assert_eq!(part(10), vec![gib(6), None, gib(7)]);
        assert!(part(11).is_empty(), "discrete only");
    }
}
