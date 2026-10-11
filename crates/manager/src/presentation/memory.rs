//! Memory module projection: system RAM and swap (never GPU framebuffer),
//! plus the RAM held by GPU allocations when a GPU shares system RAM, and
//! of it the part each tracked GPU process holds (D-64).

use dgxtop_core::metric::{Scalar, ids};
use dgxtop_core::naming::entity;
use dgxtop_core::process_model::TrackedProcess;
use dgxtop_core::quality::SampleKind;
use dgxtop_core::view::{CellState, MemoryView, MetricCell, ProcessShare, ProcessTrend, Trend};

use super::gpu_directory::GpuDirectory;
use super::process_parts::process_shares;
use super::reader::MetricReader;

/// Source label of the summed GPU allocations: only processes the driver
/// lists are counted, so driver-internal memory is not included.
const GPU_RESIDENT_SOURCE: &str = "GPU processes visible to the driver";

pub(crate) fn build_memory_view(
    reader: &MetricReader<'_>,
    directory: &GpuDirectory<'_>,
    processes: &[TrackedProcess],
    trend_width: usize,
) -> MemoryView {
    let cell = |metric: &str| reader.cell(metric, entity::MEMORY, entity::SCOPE_SYSTEM);
    MemoryView {
        total: cell(ids::MEMORY_TOTAL),
        available: cell(ids::MEMORY_AVAILABLE),
        used: cell(ids::MEMORY_USED),
        buffers: cell(ids::MEMORY_BUFFERS),
        cached: cell(ids::MEMORY_CACHED),
        swap_total: cell(ids::MEMORY_SWAP_TOTAL),
        swap_used: cell(ids::MEMORY_SWAP_USED),
        gpu_resident: unified_gpu_resident(reader, directory),
        used_trend: reader.trend_preferring(
            ids::MEMORY_USED,
            entity::MEMORY,
            entity::SCOPE_SYSTEM,
            trend_width,
        ),
        gpu_resident_trend: gpu_resident_trend(reader, directory, trend_width),
        gpu_processes: unified_process_shares(directory, processes),
        gpu_process_trends: process_trends(reader, directory, processes, trend_width),
    }
}

/// What each tracked process holds on the unified-memory GPUs now; none
/// without such a GPU (a discrete GPU's memory is not RAM).
fn unified_process_shares(
    directory: &GpuDirectory<'_>,
    processes: &[TrackedProcess],
) -> Vec<ProcessShare> {
    let unified = directory.unified_gpus();
    if unified.is_empty() {
        return Vec::new();
    }
    process_shares(processes, |gpu| unified.contains(gpu))
}

/// Each tracked process's part of the used chart, recorded beside
/// `memory.used`; a process without a point there is left out.
fn process_trends(
    reader: &MetricReader<'_>,
    directory: &GpuDirectory<'_>,
    processes: &[TrackedProcess],
    width: usize,
) -> Vec<ProcessTrend> {
    let unified = directory.unified_gpus();
    if unified.is_empty() {
        return Vec::new();
    }
    processes
        .iter()
        .filter_map(|process| {
            let trend = reader.process_memory_trend(&process.key, &unified, width);
            trend
                .points
                .iter()
                .any(Option::is_some)
                .then_some(ProcessTrend {
                    slot: process.slot,
                    trend,
                })
        })
        .collect()
}

/// What unified-memory GPUs held beside each point of `memory.used`
/// (recorded with it, so both share their time axis).
fn gpu_resident_trend(
    reader: &MetricReader<'_>,
    directory: &GpuDirectory<'_>,
    width: usize,
) -> Trend {
    reader.gpu_memory_trend(&directory.unified_gpus(), width)
}

/// RAM held by the allocations of unified-memory GPUs: the sum of their
/// `gpu.memory.process_allocated` values. `None` when no GPU shares system
/// RAM (a discrete GPU's memory is its own VRAM). Any GPU without a value
/// makes the sum unknown, never a partial total shown as complete.
fn unified_gpu_resident(
    reader: &MetricReader<'_>,
    directory: &GpuDirectory<'_>,
) -> Option<MetricCell> {
    let cells: Vec<MetricCell> = directory
        .gpus()
        .iter()
        .filter(|gpu| gpu.has_unified_memory())
        .map(|gpu| {
            reader.cell_preferring(
                ids::GPU_MEMORY_PROCESS_ALLOCATED,
                gpu.entity_id.as_str(),
                entity::SCOPE_GPU,
            )
        })
        .collect();
    let first = cells.first()?;
    if let Some(missing) = cells.iter().find(|cell| cell.value.is_none()) {
        return Some(missing.clone());
    }
    let degraded = cells.iter().any(|cell| cell.state != CellState::Ok);
    Some(MetricCell {
        value: summed(&cells),
        unit: first.unit,
        state: if degraded {
            CellState::Degraded
        } else {
            CellState::Ok
        },
        kind: SampleKind::Derived,
        quality: cells
            .iter()
            .map(|cell| cell.quality)
            .max()
            .unwrap_or(first.quality),
        age_ms: cells.iter().filter_map(|cell| cell.age_ms).max(),
        source_label: Some(GPU_RESIDENT_SOURCE.to_owned()),
        conflict: None,
        last_good: None,
    })
}

/// Exact sum when every value is an exact byte count, else a float sum.
fn summed(cells: &[MetricCell]) -> Option<Scalar> {
    let exact = cells.iter().try_fold(0u64, |sum, cell| {
        cell.value
            .and_then(Scalar::as_u64)
            .map(|value| sum.saturating_add(value))
    });
    match exact {
        Some(total) => Some(Scalar::from_u64(total)),
        None => Scalar::from_f64(cells.iter().filter_map(MetricCell::value_f64).sum()).ok(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::presentation::index::SnapshotLookup;
    use crate::presentation::testkit::{
        Fixture, NOW, entity_record, gpu_link, process_key, process_row, sample,
    };
    use dgxtop_core::identity::EntityId;
    use dgxtop_core::naming::attributes;
    use dgxtop_core::observation::EntityKind;

    /// The color the history gave `train`.
    const TRAIN: u8 = 2;
    const UNIFIED_A: &str = "gpu:GPU-aaaa";
    const UNIFIED_B: &str = "gpu:GPU-bbbb";
    const DISCRETE: &str = "gpu:GPU-cccc";

    fn add_gpu(fixture: &mut Fixture, id: &str, index: &str, memory_model: Option<&str>) {
        let mut attributes = vec![(attributes::NVML_INDEX, index)];
        if let Some(model) = memory_model {
            attributes.push((attributes::MEMORY_MODEL, model));
        }
        fixture.add_entity(entity_record(id, EntityKind::Gpu, "GPU", &attributes));
    }

    fn allocated(fixture: &mut Fixture, id: &str, bytes: u64) {
        fixture.add(
            "gpu",
            sample("gpu.memory.process_allocated", id, "gpu").u64(bytes),
        );
    }

    fn resident(fixture: &Fixture) -> Option<MetricCell> {
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW)
            .with_trends(&fixture.trends);
        let index = SnapshotLookup::new(&fixture.snapshot);
        let directory = GpuDirectory::new(&fixture.catalog, &index);
        build_memory_view(&reader, &directory, &fixture.processes.tracked, 30).gpu_resident
    }

    #[test]
    fn unified_gpus_report_the_ram_their_processes_hold() {
        let mut fixture = Fixture::new();
        add_gpu(&mut fixture, UNIFIED_A, "0", Some("unified"));
        add_gpu(&mut fixture, UNIFIED_B, "1", Some("Unified"));
        add_gpu(&mut fixture, DISCRETE, "2", Some("dedicated"));
        allocated(&mut fixture, UNIFIED_A, 6 << 30);
        allocated(&mut fixture, UNIFIED_B, 4 << 30);
        allocated(&mut fixture, DISCRETE, 30 << 30);
        let cell = resident(&fixture).unwrap();
        // The discrete GPU's VRAM is not system RAM and is left out.
        assert_eq!(cell.value.unwrap().as_u64(), Some(10 << 30));
        assert_eq!(cell.state, CellState::Ok);
        assert_eq!(cell.kind, SampleKind::Derived);
        assert_eq!(cell.source_label.as_deref(), Some(GPU_RESIDENT_SOURCE));
    }

    /// The memory chart draws the GPU share under the used RAM: the share's
    /// points come beside `memory.used`, from the state's trends (D-99),
    /// summed over the GPUs that share the RAM.
    #[test]
    fn the_used_chart_carries_the_unified_gpu_share() {
        let mut fixture = Fixture::new();
        add_gpu(&mut fixture, UNIFIED_A, "0", Some("unified"));
        allocated(&mut fixture, UNIFIED_A, 6 << 30);
        fixture.add(
            "memory",
            sample("memory.used", "memory", "system").u64(8 << 30),
        );
        let used = fixture
            .snapshot
            .domains
            .values()
            .flat_map(|domain| domain.samples.keys())
            .find(|key| key.metric_id.as_str() == "memory.used")
            .unwrap()
            .clone();
        fixture.trends.push(&used, Some(8.0));
        fixture.trends.push_beside_memory(
            &used,
            &BTreeMap::from([
                (EntityId::new(UNIFIED_A).unwrap(), Some(6.0)),
                (EntityId::new(DISCRETE).unwrap(), Some(30.0)),
            ]),
            &BTreeMap::new(),
        );
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW)
            .with_trends(&fixture.trends);
        let index = SnapshotLookup::new(&fixture.snapshot);
        let directory = GpuDirectory::new(&fixture.catalog, &index);
        let view = build_memory_view(&reader, &directory, &fixture.processes.tracked, 30);
        assert_eq!(view.used_trend.points, vec![Some(8.0)]);
        assert_eq!(view.gpu_resident_trend.points, vec![Some(6.0)]);
        // Without a unified GPU the chart has no share.
        let plain = Fixture::new();
        let reader =
            MetricReader::new(&plain.snapshot, &plain.catalog, NOW).with_trends(&plain.trends);
        let index = SnapshotLookup::new(&plain.snapshot);
        let directory = GpuDirectory::new(&plain.catalog, &index);
        assert!(
            build_memory_view(&reader, &directory, &plain.processes.tracked, 30)
                .gpu_resident_trend
                .points
                .is_empty()
        );
    }

    /// The bar and the chart split the RAM unified GPUs hold by tracked
    /// process (D-64): what each holds there now, and its points recorded
    /// beside `memory.used`. A discrete GPU's processes hold no RAM.
    #[test]
    fn the_unified_share_is_split_by_tracked_process() {
        let mut fixture = Fixture::new();
        add_gpu(&mut fixture, UNIFIED_A, "0", Some("unified"));
        add_gpu(&mut fixture, DISCRETE, "2", Some("dedicated"));
        fixture.add(
            "memory",
            sample("memory.used", "memory", "system").u64(8 << 30),
        );
        let mut train = process_row(1, "train");
        train.gpu = vec![gpu_link(UNIFIED_A, None, Some(6 << 30))];
        let mut render = process_row(2, "render");
        render.gpu = vec![gpu_link(DISCRETE, None, Some(1 << 30))];
        fixture.set_processes(vec![train, render]);
        let gpu = |id: &str| dgxtop_core::identity::EntityId::new(id).unwrap();
        fixture.processes.tracked = vec![
            TrackedProcess {
                slot: TRAIN,
                key: process_key(1, 1000),
                pid: 1,
                name: "train".to_owned(),
                current: vec![(gpu(UNIFIED_A), 6 << 30)],
            },
            TrackedProcess {
                slot: TRAIN + 1,
                key: process_key(2, 1000),
                pid: 2,
                name: "render".to_owned(),
                current: vec![(gpu(DISCRETE), 1 << 30)],
            },
        ];
        let used = fixture
            .snapshot
            .domains
            .values()
            .flat_map(|domain| domain.samples.keys())
            .find(|key| key.metric_id.as_str() == "memory.used")
            .unwrap()
            .clone();
        let held =
            |pid, gpu: &str, bytes| ((process_key(pid, 1000), EntityId::new(gpu).unwrap()), bytes);
        fixture.trends.push_beside_memory(
            &used,
            &BTreeMap::new(),
            &BTreeMap::from([held(1, UNIFIED_A, 6), held(2, DISCRETE, 1)]),
        );
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW)
            .with_trends(&fixture.trends);
        let index = SnapshotLookup::new(&fixture.snapshot);
        let directory = GpuDirectory::new(&fixture.catalog, &index);
        let view = build_memory_view(&reader, &directory, &fixture.processes.tracked, 30);
        let shares: Vec<(u8, &str, u64)> = view
            .gpu_processes
            .iter()
            .map(|share| (share.slot, share.name.as_str(), share.bytes))
            .collect();
        assert_eq!(shares, vec![(TRAIN, "train", 6 << 30)]);
        let parts: Vec<(u8, Vec<Option<f64>>)> = view
            .gpu_process_trends
            .iter()
            .map(|part| (part.slot, part.trend.points.clone()))
            .collect();
        assert_eq!(parts, vec![(TRAIN, vec![Some(6.0)])]);
    }

    #[test]
    fn a_unified_gpu_without_a_value_makes_the_sum_unknown() {
        let mut fixture = Fixture::new();
        add_gpu(&mut fixture, UNIFIED_A, "0", Some("unified"));
        add_gpu(&mut fixture, UNIFIED_B, "1", Some("unified"));
        allocated(&mut fixture, UNIFIED_A, 6 << 30);
        fixture.add(
            "gpu",
            sample("gpu.memory.process_allocated", UNIFIED_B, "gpu")
                .unavailable()
                .reason("NVML_PROCESS_LIST_PARTIAL"),
        );
        let cell = resident(&fixture).unwrap();
        assert_eq!(
            cell.value, None,
            "a partial sum is never shown as the total"
        );
        assert!(!cell.state.has_value());
    }

    #[test]
    fn memory_cells_keep_zero_swap_and_missing_cached_apart() {
        let mut fixture = Fixture::new();
        fixture.add(
            "memory",
            sample("memory.total", "memory", "system").u64(128 << 30),
        );
        fixture.add(
            "memory",
            sample("memory.swap_total", "memory", "system").u64(0),
        );
        fixture.add(
            "memory",
            sample("memory.cached", "memory", "system")
                .unavailable()
                .reason("TIMEOUT"),
        );
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, NOW)
            .with_trends(&fixture.trends);
        let index = SnapshotLookup::new(&fixture.snapshot);
        let directory = GpuDirectory::new(&fixture.catalog, &index);
        let view = build_memory_view(&reader, &directory, &fixture.processes.tracked, 30);
        assert_eq!(view.total.value.unwrap().as_u64(), Some(128 << 30));
        assert_eq!(view.swap_total.value_f64(), Some(0.0));
        assert_eq!(view.cached.state, CellState::ReadFailed);
        assert_eq!(view.cached.value, None);
        assert_eq!(view.used.state, CellState::Unavailable);
        assert!(view.used_trend.points.is_empty());
    }
}
