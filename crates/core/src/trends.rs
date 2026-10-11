//! Short trends (D-79, D-99): the latest points of the series the overview
//! draws as sparklines, one point per new read, a read without a value a
//! gap (`None`). The state keeps the one [`TrendBook`] of a run, filled at
//! commit, and the presentation reads it through
//! [`crate::ports::TrendReader`].
//!
//! Beside each new point of `memory.used` the book records what each GPU's
//! processes hold (`gpu.memory.process_allocated`) and what each listed
//! process holds on each GPU (D-64), so the memory chart draws the RAM
//! unified-memory GPUs take, and each process's part of it, on its own
//! time axis. Which GPUs share the RAM is the catalog's fact, so it is
//! given with each read.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::identity::{EntityId, MetricKey, ProcessKey};
use crate::ports::TrendReader;
use crate::view::Trend;

/// Points kept per series (4 minutes at the default 1 s interval).
pub const TREND_POINTS: usize = 240;
/// (process, GPU) series kept beside one `memory.used` series at most: the
/// tracked processes are the largest holders, so a host with more GPU
/// processes keeps the largest.
pub const MAX_PROCESS_SERIES: usize = 256;

type Points = VecDeque<Option<f64>>;

/// What is recorded beside one `memory.used` series, point for point.
#[derive(Debug, Clone, Default, PartialEq)]
struct BesideMemory {
    /// Each GPU's `gpu.memory.process_allocated`.
    gpus: BTreeMap<EntityId, Points>,
    /// What each process holds on each GPU; `None` while it holds nothing.
    processes: BTreeMap<(ProcessKey, EntityId), Points>,
}

/// The bounded recent points of every trend series.
#[derive(Debug, Clone, PartialEq)]
pub struct TrendBook {
    capacity: usize,
    series: BTreeMap<MetricKey, Points>,
    beside: BTreeMap<MetricKey, BesideMemory>,
}

impl Default for TrendBook {
    fn default() -> Self {
        Self::new(TREND_POINTS)
    }
}

impl TrendBook {
    /// `capacity` points per series.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            series: BTreeMap::new(),
            beside: BTreeMap::new(),
        }
    }

    /// Appends one point (or a gap) to a series, evicting the oldest.
    pub fn push(&mut self, key: &MetricKey, value: Option<f64>) {
        let points = self.series.entry(key.clone()).or_default();
        push_bounded(points, self.capacity, value);
    }

    /// Appends, beside a new point of the `memory.used` series `used`, each
    /// GPU's allocations (`gpus`) and what each process holds on each GPU
    /// (`processes`, bytes above zero). A GPU not given is let go; a
    /// process not given gets a gap and is let go once its points are all
    /// gaps; a new process series starts with a point holding memory, the
    /// largest first while there is room.
    pub fn push_beside_memory(
        &mut self,
        used: &MetricKey,
        gpus: &BTreeMap<EntityId, Option<f64>>,
        processes: &BTreeMap<(ProcessKey, EntityId), u64>,
    ) {
        let capacity = self.capacity;
        let beside = self.beside.entry(used.clone()).or_default();
        beside.gpus.retain(|gpu, _| gpus.contains_key(gpu));
        for (gpu, value) in gpus {
            let points = beside.gpus.entry(gpu.clone()).or_default();
            push_bounded(points, capacity, *value);
        }
        for (key, points) in &mut beside.processes {
            #[allow(clippy::cast_precision_loss)]
            let held = processes.get(key).map(|bytes| *bytes as f64);
            push_bounded(points, capacity, held);
        }
        beside
            .processes
            .retain(|_, points| points.iter().any(Option::is_some));
        let mut newcomers: Vec<(&(ProcessKey, EntityId), u64)> = processes
            .iter()
            .filter(|(key, bytes)| **bytes > 0 && !beside.processes.contains_key(*key))
            .map(|(key, bytes)| (key, *bytes))
            .collect();
        newcomers.sort_by(|left, right| right.1.cmp(&left.1));
        let room = MAX_PROCESS_SERIES.saturating_sub(beside.processes.len());
        for (key, bytes) in newcomers.into_iter().take(room) {
            let mut points = Points::new();
            #[allow(clippy::cast_precision_loss)]
            let held = bytes as f64;
            push_bounded(&mut points, capacity, Some(held));
            beside.processes.insert(key.clone(), points);
        }
    }

    /// Whether a series has points.
    pub fn contains(&self, key: &MetricKey) -> bool {
        self.series.contains_key(key)
    }

    /// Keeps only the series (and what is recorded beside them) `keep`
    /// accepts (bounded key set).
    pub fn retain(&mut self, mut keep: impl FnMut(&MetricKey) -> bool) {
        self.series.retain(|key, _| keep(key));
        self.beside.retain(|key, _| keep(key));
    }

    pub fn len(&self) -> usize {
        self.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    /// The latest `width` of `points`, oldest first.
    fn latest(points: impl DoubleEndedIterator<Item = Option<f64>>, width: usize) -> Trend {
        let mut newest: Vec<Option<f64>> = points.rev().take(width).collect();
        newest.reverse();
        Trend { points: newest }
    }

    /// The beside-series of `selected` aligned on their newest point (a
    /// shorter one started later: its missing points are gaps), combined
    /// point by point; empty when none of them has points.
    fn combined<'a>(
        selected: impl Iterator<Item = &'a Points>,
        combine: impl Fn(&[Option<f64>]) -> Option<f64>,
    ) -> Vec<Option<f64>> {
        let selected: Vec<&Points> = selected.collect();
        let length = selected
            .iter()
            .map(|points| points.len())
            .max()
            .unwrap_or(0);
        (0..length)
            .map(|index| {
                let column: Vec<Option<f64>> = selected
                    .iter()
                    .map(|points| {
                        let offset = length - points.len();
                        index
                            .checked_sub(offset)
                            .and_then(|position| points.get(position).copied().flatten())
                    })
                    .collect();
                combine(&column)
            })
            .collect()
    }
}

fn push_bounded(points: &mut Points, capacity: usize, value: Option<f64>) {
    if points.len() == capacity {
        points.pop_front();
    }
    points.push_back(value.filter(|number| number.is_finite()));
}

impl TrendReader for TrendBook {
    fn trend(&self, key: &MetricKey, width: usize) -> Trend {
        self.series.get(key).map_or_else(Trend::default, |points| {
            Self::latest(points.iter().copied(), width)
        })
    }

    fn gpu_memory_beside(
        &self,
        used: &MetricKey,
        gpus: &BTreeSet<EntityId>,
        width: usize,
    ) -> Trend {
        let Some(beside) = self.beside.get(used).filter(|_| !gpus.is_empty()) else {
            return Trend::default();
        };
        let known = gpus.iter().filter_map(|gpu| beside.gpus.get(gpu));
        let all_known = gpus.iter().all(|gpu| beside.gpus.contains_key(gpu));
        // A GPU without a value makes the sum a gap, never a partial sum.
        let sums = Self::combined(known, |column| {
            all_known
                .then(|| column.iter().copied().sum::<Option<f64>>())
                .flatten()
        });
        Self::latest(sums.into_iter(), width)
    }

    fn process_memory_beside(
        &self,
        used: &MetricKey,
        process: &ProcessKey,
        gpus: &BTreeSet<EntityId>,
        width: usize,
    ) -> Trend {
        let Some(beside) = self.beside.get(used) else {
            return Trend::default();
        };
        let held = beside
            .processes
            .iter()
            .filter(|((key, gpu), _)| key == process && gpus.contains(gpu))
            .map(|(_, points)| points);
        let parts = Self::combined(held, |column| {
            column
                .iter()
                .flatten()
                .copied()
                .reduce(|sum, bytes| sum + bytes)
        });
        // The part starts with the first point holding some memory.
        let first = parts
            .iter()
            .position(Option::is_some)
            .unwrap_or(parts.len());
        Self::latest(parts.into_iter().skip(first), width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{metric_key, test_process_key};

    fn gpu(id: &str) -> EntityId {
        EntityId::new(id).unwrap()
    }

    #[test]
    fn a_series_keeps_its_latest_points_with_gaps() {
        let key = metric_key("cpu", "cpu.utilization.total", "host");
        let mut book = TrendBook::new(3);
        for value in [Some(1.0), None, Some(f64::NAN), Some(4.0)] {
            book.push(&key, value);
        }
        assert_eq!(book.trend(&key, 10).points, vec![None, None, Some(4.0)]);
        assert_eq!(book.trend(&key, 1).points, vec![Some(4.0)]);
        let other = metric_key("memory", "memory.used", "system");
        assert!(book.trend(&other, 10).points.is_empty());
        book.retain(|kept| *kept == other);
        assert!(book.is_empty() && !book.contains(&key));
    }

    /// The GPUs' sum is a gap where any asked GPU had no value or no series
    /// yet; a GPU that started later counts as unknown before its first
    /// point; no GPU asked gives no trend.
    #[test]
    fn the_gpu_sum_is_never_partial() {
        let used = metric_key("memory", "memory.used", "system");
        let (one, two) = (gpu("gpu:GPU-1"), gpu("gpu:GPU-2"));
        let mut book = TrendBook::new(10);
        let none = BTreeMap::new();
        book.push_beside_memory(&used, &BTreeMap::from([(one.clone(), Some(4.0))]), &none);
        let both = BTreeMap::from([(one.clone(), Some(4.0)), (two.clone(), Some(1.0))]);
        book.push_beside_memory(&used, &both, &none);
        let gap = BTreeMap::from([(one.clone(), Some(5.0)), (two.clone(), None)]);
        book.push_beside_memory(&used, &gap, &none);
        let asked = BTreeSet::from([one.clone(), two.clone()]);
        assert_eq!(
            book.gpu_memory_beside(&used, &asked, 10).points,
            vec![None, Some(5.0), None]
        );
        assert_eq!(
            book.gpu_memory_beside(&used, &BTreeSet::from([one.clone()]), 2)
                .points,
            vec![Some(4.0), Some(5.0)]
        );
        assert!(
            book.gpu_memory_beside(&used, &BTreeSet::new(), 10)
                .points
                .is_empty()
        );
        let missing = BTreeSet::from([one, gpu("gpu:GPU-9")]);
        assert_eq!(
            book.gpu_memory_beside(&used, &missing, 10).points,
            vec![None; 3]
        );
    }

    /// A process's part sums what it holds on the asked GPUs, starts with
    /// its first point holding memory, keeps gaps while it holds none and
    /// goes once all its points are gaps.
    #[test]
    fn a_process_part_starts_when_it_holds_memory() {
        let used = metric_key("memory", "memory.used", "system");
        let (unified, discrete) = (gpu("gpu:GPU-1"), gpu("gpu:GPU-2"));
        let train = test_process_key(10);
        let mut book = TrendBook::new(3);
        let gpus = BTreeMap::new();
        let held = |pairs: &[(&EntityId, u64)]| -> BTreeMap<(ProcessKey, EntityId), u64> {
            pairs
                .iter()
                .map(|(gpu, bytes)| ((train.clone(), (*gpu).clone()), *bytes))
                .collect()
        };
        book.push_beside_memory(&used, &gpus, &held(&[(&discrete, 1)]));
        book.push_beside_memory(&used, &gpus, &held(&[(&unified, 6), (&discrete, 1)]));
        book.push_beside_memory(&used, &gpus, &held(&[(&discrete, 1)]));
        let on = BTreeSet::from([unified.clone()]);
        assert_eq!(
            book.process_memory_beside(&used, &train, &on, 10).points,
            vec![Some(6.0), None]
        );
        let all = BTreeSet::from([unified.clone(), discrete.clone()]);
        assert_eq!(
            book.process_memory_beside(&used, &train, &all, 10).points,
            vec![Some(1.0), Some(7.0), Some(1.0)]
        );
        for _ in 0..3 {
            book.push_beside_memory(&used, &gpus, &BTreeMap::new());
        }
        assert!(
            book.process_memory_beside(&used, &train, &all, 10)
                .points
                .is_empty(),
            "let go once only gaps are left"
        );
    }

    /// Past [`MAX_PROCESS_SERIES`] the largest holders get a series.
    #[test]
    fn process_series_are_bounded_largest_first() {
        let used = metric_key("memory", "memory.used", "system");
        let gpu = gpu("gpu:GPU-1");
        let count = i64::try_from(MAX_PROCESS_SERIES).unwrap() + 2;
        let processes: BTreeMap<(ProcessKey, EntityId), u64> = (1..=count)
            .map(|pid| {
                (
                    (test_process_key(pid), gpu.clone()),
                    u64::try_from(pid).unwrap(),
                )
            })
            .collect();
        let mut book = TrendBook::new(4);
        book.push_beside_memory(&used, &BTreeMap::new(), &processes);
        let asked = BTreeSet::from([gpu]);
        let has = |pid| {
            !book
                .process_memory_beside(&used, &test_process_key(pid), &asked, 4)
                .points
                .is_empty()
        };
        assert!(has(count) && has(3));
        assert!(!has(1) && !has(2), "the smallest give way");
    }
}
