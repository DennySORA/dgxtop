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
//! with the key of the sample it renders. Beside each new `memory.used`
//! point go the RAM unified-memory GPUs hold and each tracked GPU
//! process's part of it (D-64), so the memory chart draws them on its time
//! axis.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::canonical::{CanonicalSample, CatalogSnapshot, ImmutableSnapshot};
use dgxtop_core::identity::{EntityId, MetricId, MetricKey, ProcessKey};
use dgxtop_core::metric::{Scalar, ids};
use dgxtop_core::naming::entity;
use dgxtop_core::observation::EntityKind;
use dgxtop_core::process_model::TrackedProcess;

use crate::presentation::TrendBook;
use crate::presentation::gpu_directory::is_unified_memory;

/// Metric id of the RAM unified-memory GPUs hold, recorded beside
/// `memory.used` (not a collected metric).
pub(crate) const GPU_RESIDENT_TREND: &str = "memory.used.gpu_resident";
/// Metric id of a tracked process's part of that RAM, recorded beside
/// `memory.used` with the process as its entity (not a collected metric).
pub(crate) const GPU_PROCESS_TREND: &str = "memory.used.gpu_process";

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
            book: TrendBook::new(TREND_POINTS),
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
    /// allocate from system RAM, whose share, and the part of it each of
    /// the `processes` tracked holds, follow `memory.used`.
    pub(crate) fn record(
        &mut self,
        snapshot: &ImmutableSnapshot,
        unified_gpus: &BTreeSet<EntityId>,
        processes: &[TrackedProcess],
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
            self.record_process_shares(unified_gpus, processes, &new_memory_reads, &mut present);
        }
        self.last_read.retain(|key, _| present.contains(key));
        self.book.retain(|key| present.contains(key));
    }

    /// Beside each new `memory.used` point, what each tracked process holds
    /// on the unified GPUs (`None` while it holds nothing there). A process
    /// gets its series once it holds some and keeps it while it is tracked.
    fn record_process_shares(
        &mut self,
        unified_gpus: &BTreeSet<EntityId>,
        processes: &[TrackedProcess],
        new_memory_reads: &[MetricKey],
        present: &mut BTreeSet<MetricKey>,
    ) {
        let used_keys: Vec<MetricKey> = present
            .iter()
            .filter(|key| key.metric_id.as_str() == ids::MEMORY_USED)
            .cloned()
            .collect();
        for process in processes {
            let bytes = process.bytes_on(|gpu| unified_gpus.contains(gpu));
            #[allow(clippy::cast_precision_loss)]
            let share = (bytes > 0).then_some(bytes as f64);
            for used in &used_keys {
                let Some(key) = process_share_key(used, &process.key) else {
                    continue;
                };
                if share.is_none() && !self.book.contains(&key) {
                    continue;
                }
                if new_memory_reads.contains(used) {
                    self.book.push(&key, share);
                }
                present.insert(key);
            }
        }
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

/// The key of `process`'s part of that RAM, beside the `memory.used` key.
pub(crate) fn process_share_key(used: &MetricKey, process: &ProcessKey) -> Option<MetricKey> {
    Some(MetricKey {
        metric_id: MetricId::new(GPU_PROCESS_TREND).ok()?,
        entity_id: process.entity_id().ok()?,
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
    use crate::test_support::{committed_snapshot, sample_entry, test_process_key};
    use dgxtop_core::testing::metric_key;

    /// No GPU process tracked.
    fn untracked() -> Vec<TrackedProcess> {
        Vec::new()
    }

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
        recorder.record(&first, &BTreeSet::new(), &untracked());
        // The same reads committed again (another domain's commit) add nothing.
        recorder.record(&first, &BTreeSet::new(), &untracked());
        assert_eq!(recorder.book().trend(&cpu, 10).points, vec![Some(12.5)]);
        assert!(recorder.book().trend(&core, 10).points.is_empty());
        let second = committed_snapshot(
            2,
            vec![
                ("cpu", sample_entry(&cpu, None, 2_000)),
                ("gpu", sample_entry(&gpu, Some(62.0), 2_000)),
            ],
        );
        recorder.record(&second, &BTreeSet::new(), &untracked());
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
            &untracked(),
        );
        recorder.record(
            &committed_snapshot(2, vec![("memory", sample_entry(&memory, Some(6.0), 2))]),
            &BTreeSet::new(),
            &untracked(),
        );
        assert!(recorder.book().trend(&gpu, 10).points.is_empty());
        assert_eq!(recorder.book().len(), 1);
        recorder.set_step(1000);
        assert_eq!(recorder.book().len(), 1);
        recorder.set_step(2000);
        assert!(recorder.book().is_empty());
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
            &untracked(),
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
            &untracked(),
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
            &untracked(),
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
            &untracked(),
        );
        assert!(plain.book().trend(&share, 10).points.is_empty());
    }

    /// Beside each new `memory.used` point goes what each tracked process
    /// holds on the unified GPUs (D-64): a process gets its series once it
    /// holds memory there, keeps it with a gap while it holds none, and
    /// loses it with its color slot. A process only on a discrete GPU has
    /// none.
    #[test]
    fn each_tracked_process_has_its_part_beside_used_memory() {
        const GIB: u64 = 1 << 30;
        let memory = metric_key("memory", "memory.used", "system");
        let gpus: BTreeSet<EntityId> = [EntityId::new("gpu:GPU-1").unwrap()].into();
        let part = |pid| process_share_key(&memory, &test_process_key(pid)).unwrap();
        // The tracked processes as the history publishes them: an exited
        // one keeps its color with nothing listed.
        let tracked = |train: Option<u64>, render: Option<u64>| {
            let process = |pid: i64, name: &str, gpu: &str, held: Option<u64>| TrackedProcess {
                slot: u8::try_from(pid - 10).unwrap(),
                key: test_process_key(pid),
                pid: i32::try_from(pid).unwrap(),
                name: name.to_owned(),
                current: held
                    .map(|bytes| vec![(EntityId::new(gpu).unwrap(), bytes)])
                    .unwrap_or_default(),
            };
            vec![
                process(10, "train", "gpu:GPU-1", train),
                process(11, "render", "gpu:GPU-2", render),
            ]
        };
        let mut processes = Vec::new();
        let mut recorder = TrendRecorder::new(1000);
        let mut step = |read: u64, now: Vec<TrackedProcess>, recorder: &mut TrendRecorder| {
            processes = now;
            let snapshot = committed_snapshot(
                read,
                vec![("memory", sample_entry(&memory, Some(50.0), read))],
            );
            recorder.record(&snapshot, &gpus, &processes);
        };
        step(1, tracked(Some(6 * GIB), Some(2 * GIB)), &mut recorder);
        step(2, tracked(None, None), &mut recorder);
        step(3, tracked(Some(7 * GIB), None), &mut recorder);
        #[allow(clippy::cast_precision_loss)]
        let gib = |value: u64| Some((value * GIB) as f64);
        assert_eq!(
            recorder.book().trend(&part(10), 10).points,
            vec![gib(6), None, gib(7)]
        );
        assert!(!recorder.book().contains(&part(11)), "discrete only");
        // Without a unified GPU the parts go too.
        let snapshot =
            committed_snapshot(4, vec![("memory", sample_entry(&memory, Some(50.0), 4))]);
        recorder.record(&snapshot, &BTreeSet::new(), &processes);
        assert!(!recorder.book().contains(&part(10)));
    }
}
