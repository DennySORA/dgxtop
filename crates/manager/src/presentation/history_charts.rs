//! Series the HistoryService queries for a session on the history view.
//! The charts (D-46, D-50) are the overview's series over a longer window:
//! the CPU total against 100 %, the RAM in use against the RAM total, each
//! GPU's utilization, memory in use, temperature and power against their
//! scales ([`gpu_series`]), and the read/write and receive/transmit rates of
//! one disk and one interface. The disk and the interface are the busiest
//! when the view opens and stay while they report, so the charts never jump
//! between devices. The statistics (D-48) summarize the same four GPU
//! series over 1 h to 24 h.

use dgxtop_core::canonical::{CatalogSnapshot, ImmutableSnapshot};
use dgxtop_core::metric::ids;
use dgxtop_core::naming::entity;
use dgxtop_core::view::HistoryChartKind;

use super::TrendBook;
use super::disk::device_views;
use super::gpu::gpu_name;
use super::gpu_directory::{GpuDirectory, GpuIdentity};
use super::network::ranked_interfaces;
use super::reader::MetricReader;
use crate::history::{ChartRequest, HistoryChartSpec, HistoryPicks, MAX_CHARTS, MAX_STAT_SERIES};

/// Charts besides the GPUs: CPU, memory and two rates each of a disk and an
/// interface; the GPUs get the rest of [`MAX_CHARTS`].
const HOST_CHARTS: usize = 6;
const PERCENT_SCALE: f64 = 100.0;
/// Fixed top of a GPU temperature chart, as on the GPU view's bar.
const TEMPERATURE_SCALE_C: f64 = 100.0;

/// Statistics series of one GPU: utilization, memory, temperature, power.
const SERIES_PER_GPU: usize = 4;

/// The series for a session's request.
pub(crate) fn history_charts(
    snapshot: &ImmutableSnapshot,
    catalog: &CatalogSnapshot,
    trends: &TrendBook,
    now_boot_ns: u64,
    request: &ChartRequest<'_>,
) -> HistoryPicks {
    let reader = MetricReader::new(snapshot, catalog, trends, now_boot_ns);
    HistoryPicks {
        charts: dashboard(&reader, catalog, request.shown),
        stats: gpu_stats(&reader, catalog),
    }
}

/// The history view's charts, keeping the disk and interface `previous`
/// shows.
fn dashboard(
    reader: &MetricReader<'_>,
    catalog: &CatalogSnapshot,
    previous: &[HistoryChartSpec],
) -> Vec<HistoryChartSpec> {
    let mut charts = Charts {
        reader,
        specs: Vec::new(),
    };
    charts.add(
        HistoryChartKind::CpuUtilization,
        "",
        (
            ids::CPU_UTILIZATION_TOTAL,
            entity::CPU_AGGREGATE,
            entity::SCOPE_HOST,
        ),
        Some(PERCENT_SCALE),
    );
    charts.add(
        HistoryChartKind::MemoryUsed,
        "",
        (ids::MEMORY_USED, entity::MEMORY, entity::SCOPE_SYSTEM),
        ram_total(reader),
    );
    let directory = GpuDirectory::new(catalog, reader.index());
    for gpu in directory
        .gpus()
        .iter()
        .take((MAX_CHARTS - HOST_CHARTS) / SERIES_PER_GPU)
    {
        charts.add_gpu(gpu);
    }
    let busiest_disk = || {
        device_views(reader)
            .into_iter()
            .next()
            .map(|device| (device.entity_id.to_string(), device.name))
    };
    charts.add_device(
        previous,
        [HistoryChartKind::DiskRead, HistoryChartKind::DiskWrite],
        [ids::DISK_READ_BPS, ids::DISK_WRITE_BPS],
        entity::SCOPE_DEVICE,
        busiest_disk,
    );
    let busiest_interface = || {
        ranked_interfaces(reader)
            .into_iter()
            .next()
            .map(|interface| (interface.entity_id.to_string(), interface.name))
    };
    charts.add_device(
        previous,
        [
            HistoryChartKind::NetworkReceive,
            HistoryChartKind::NetworkTransmit,
        ],
        [ids::NET_RX_BPS, ids::NET_TX_BPS],
        entity::SCOPE_INTERFACE,
        busiest_interface,
    );
    charts.specs
}

/// Each GPU's statistics series (D-48), the four of [`gpu_series`], for as
/// many GPUs as [`MAX_STAT_SERIES`] allows.
fn gpu_stats(reader: &MetricReader<'_>, catalog: &CatalogSnapshot) -> Vec<HistoryChartSpec> {
    let directory = GpuDirectory::new(catalog, reader.index());
    let mut charts = Charts {
        reader,
        specs: Vec::new(),
    };
    for gpu in directory
        .gpus()
        .iter()
        .take(MAX_STAT_SERIES / SERIES_PER_GPU)
    {
        charts.add_gpu(gpu);
    }
    charts.specs
}

/// The RAM total, the scale of the memory chart and of a unified GPU's.
fn ram_total(reader: &MetricReader<'_>) -> Option<f64> {
    reader
        .cell(ids::MEMORY_TOTAL, entity::MEMORY, entity::SCOPE_SYSTEM)
        .value_f64()
}

/// A GPU's series with their scales (D-50): utilization against 100 %, the
/// memory in use against the framebuffer (on unified memory, what its
/// processes hold against the RAM), the temperature against a fixed 100 °C
/// and the power against its enforced limit.
fn gpu_series(
    reader: &MetricReader<'_>,
    gpu: &GpuIdentity<'_>,
) -> [(HistoryChartKind, &'static str, Option<f64>); SERIES_PER_GPU] {
    let entity_id = gpu.entity_id.as_str();
    let gpu_cell = |metric: &str| {
        reader
            .cell(metric, entity_id, entity::SCOPE_GPU)
            .value_f64()
    };
    let memory = if gpu.has_unified_memory() {
        (ids::GPU_MEMORY_PROCESS_ALLOCATED, ram_total(reader))
    } else {
        (ids::GPU_MEMORY_FB_USED, gpu_cell(ids::GPU_MEMORY_FB_TOTAL))
    };
    [
        (
            HistoryChartKind::GpuUtilization,
            ids::GPU_UTILIZATION_COMPUTE,
            Some(PERCENT_SCALE),
        ),
        (HistoryChartKind::GpuMemory, memory.0, memory.1),
        (
            HistoryChartKind::GpuTemperature,
            ids::GPU_TEMPERATURE,
            Some(TEMPERATURE_SCALE_C),
        ),
        (
            HistoryChartKind::GpuPower,
            ids::GPU_POWER_DRAW,
            gpu_cell(ids::GPU_POWER_LIMIT),
        ),
    ]
}

/// Collects the charts whose series the snapshot reports.
struct Charts<'r, 'a> {
    reader: &'r MetricReader<'a>,
    specs: Vec<HistoryChartSpec>,
}

impl Charts<'_, '_> {
    /// Adds the series of `gpu` it reports ([`gpu_series`]).
    fn add_gpu(&mut self, gpu: &GpuIdentity<'_>) {
        let subject = format!("{} · {}", gpu.label, gpu_name(gpu.record));
        for (kind, metric, scale_max) in gpu_series(self.reader, gpu) {
            self.add(
                kind,
                &subject,
                (metric, gpu.entity_id.as_str(), entity::SCOPE_GPU),
                scale_max,
            );
        }
    }

    /// Adds the chart of `(metric, entity, preferred scope)` when reported.
    fn add(
        &mut self,
        kind: HistoryChartKind,
        subject: &str,
        (metric, entity_id, scope): (&str, &str, &str),
        scale_max: Option<f64>,
    ) {
        if let Some(entry) = self.reader.index().find(metric, entity_id, scope) {
            self.specs.push(HistoryChartSpec {
                kind,
                subject: subject.to_owned(),
                key: entry.sample.key.clone(),
                scale_max,
            });
        }
    }

    /// The two rate charts of one device: the one `previous` shows while it
    /// still reports `metrics[0]`, else the busiest.
    fn add_device(
        &mut self,
        previous: &[HistoryChartSpec],
        kinds: [HistoryChartKind; 2],
        metrics: [&str; 2],
        scope: &str,
        busiest: impl FnOnce() -> Option<(String, String)>,
    ) {
        let kept = previous
            .iter()
            .find(|spec| spec.kind == kinds[0])
            .filter(|spec| {
                self.reader
                    .index()
                    .find(metrics[0], spec.key.entity_id.as_str(), scope)
                    .is_some()
            })
            .map(|spec| (spec.key.entity_id.to_string(), spec.subject.clone()));
        let Some((entity_id, name)) = kept.or_else(busiest) else {
            return;
        };
        for (kind, metric) in kinds.into_iter().zip(metrics) {
            self.add(kind, &name, (metric, &entity_id, scope), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{Fixture, NOW, entity_record, sample};
    use dgxtop_core::naming::attributes;
    use dgxtop_core::observation::EntityKind;

    const GPU_A: &str = "gpu:GPU-aaaa1111";
    const GPU_B: &str = "gpu:GPU-bbbb2222";

    fn charts(fixture: &Fixture, previous: &[HistoryChartSpec]) -> Vec<HistoryChartSpec> {
        pick(fixture, previous).charts
    }

    fn pick(fixture: &Fixture, shown: &[HistoryChartSpec]) -> HistoryPicks {
        history_charts(
            &fixture.snapshot,
            &fixture.catalog,
            &fixture.trends,
            NOW,
            &ChartRequest { shown },
        )
    }

    fn host() -> Fixture {
        let mut fixture = Fixture::new();
        fixture.add(
            "cpu",
            sample("cpu.utilization.total", "cpu", "host").f64(12.0),
        );
        fixture.add(
            "memory",
            sample("memory.used", "memory", "system").u64(4 << 30),
        );
        fixture.add(
            "memory",
            sample("memory.total", "memory", "system").u64(64 << 30),
        );
        fixture.add_entity(entity_record(
            GPU_A,
            EntityKind::Gpu,
            "NVIDIA H100",
            &[(attributes::NVML_INDEX, "0")],
        ));
        fixture.add(
            "gpu",
            sample("gpu.utilization.compute", GPU_A, "gpu").f64(50.0),
        );
        fixture
    }

    fn disk(fixture: &mut Fixture, id: &str, name: &str, read: f64) {
        fixture.add_entity(entity_record(id, EntityKind::BlockDevice, name, &[]));
        fixture.add(
            "disk",
            sample("disk.read.bytes_per_second", id, "device").f64(read),
        );
        fixture.add(
            "disk",
            sample("disk.write.bytes_per_second", id, "device").f64(0.0),
        );
    }

    fn kinds(charts: &[HistoryChartSpec]) -> Vec<HistoryChartKind> {
        charts.iter().map(|chart| chart.kind).collect()
    }

    #[test]
    fn the_history_view_charts_the_overview_series() {
        let mut fixture = host();
        disk(&mut fixture, "blockdev:259:0", "nvme0n1", 100.0);
        let charts = charts(&fixture, &[]);
        assert_eq!(
            kinds(&charts),
            vec![
                HistoryChartKind::CpuUtilization,
                HistoryChartKind::MemoryUsed,
                HistoryChartKind::GpuUtilization,
                HistoryChartKind::DiskRead,
                HistoryChartKind::DiskWrite,
            ],
            "no interface reports, so no network charts"
        );
        assert_eq!(charts[0].scale_max, Some(100.0));
        assert_eq!(charts[1].scale_max, Some(64.0 * 1_073_741_824.0));
        assert_eq!(charts[2].subject, "GPU 0 · NVIDIA H100");
        assert_eq!(charts[2].key.entity_id.as_str(), GPU_A);
        assert_eq!(charts[3].subject, "nvme0n1");
        assert_eq!(charts[3].scale_max, None, "rates scale to their peak");
        assert_eq!(
            charts[4].key.metric_id.as_str(),
            "disk.write.bytes_per_second"
        );
    }

    #[test]
    fn nothing_reported_gives_no_charts() {
        assert_eq!(pick(&Fixture::new(), &[]), HistoryPicks::default());
    }

    #[test]
    fn each_gpu_charts_its_four_series_against_their_scales() {
        let mut fixture = host();
        fixture.add_entity(entity_record(
            GPU_B,
            EntityKind::Gpu,
            "NVIDIA L4",
            &[(attributes::NVML_INDEX, "1")],
        ));
        for (metric, value) in [
            ("gpu.utilization.compute", 20.0),
            ("gpu.memory.framebuffer.used", 1e9),
            ("gpu.memory.framebuffer.total", 24e9),
            ("gpu.temperature", 41.0),
            ("gpu.power.draw", 70.0),
            ("gpu.power.limit", 72.0),
        ] {
            fixture.add("gpu", sample(metric, GPU_B, "gpu").f64(value));
        }
        let charts = charts(&fixture, &[]);
        let gpu_b: Vec<(HistoryChartKind, Option<f64>)> = charts
            .iter()
            .filter(|chart| chart.key.entity_id.as_str() == GPU_B)
            .map(|chart| (chart.kind, chart.scale_max))
            .collect();
        assert_eq!(
            gpu_b,
            vec![
                (HistoryChartKind::GpuUtilization, Some(100.0)),
                (HistoryChartKind::GpuMemory, Some(24e9)),
                (HistoryChartKind::GpuTemperature, Some(100.0)),
                (HistoryChartKind::GpuPower, Some(72.0)),
            ]
        );
        // GPU A reports utilization only: one chart, before GPU B's.
        assert_eq!(
            charts
                .iter()
                .filter(|chart| chart.key.entity_id.as_str() == GPU_A)
                .count(),
            1
        );
        assert!(charts.len() <= MAX_CHARTS);
    }

    #[test]
    fn every_gpu_is_summarized_by_the_series_it_reports() {
        let mut fixture = host();
        fixture.add_entity(entity_record(
            GPU_B,
            EntityKind::Gpu,
            "NVIDIA L4",
            &[(attributes::NVML_INDEX, "1")],
        ));
        for (metric, value) in [
            ("gpu.utilization.compute", 20.0),
            ("gpu.memory.framebuffer.used", 1e9),
            ("gpu.temperature", 41.0),
            ("gpu.power.draw", 70.0),
        ] {
            fixture.add("gpu", sample(metric, GPU_B, "gpu").f64(value));
        }
        let stats = pick(&fixture, &[]).stats;
        // GPU A reports utilization only; GPU B all four series.
        assert_eq!(
            kinds(&stats),
            vec![
                HistoryChartKind::GpuUtilization,
                HistoryChartKind::GpuUtilization,
                HistoryChartKind::GpuMemory,
                HistoryChartKind::GpuTemperature,
                HistoryChartKind::GpuPower,
            ]
        );
        assert_eq!(stats[0].key.entity_id.as_str(), GPU_A);
        assert_eq!(stats[0].subject, "GPU 0 · NVIDIA H100");
        assert!(stats[1..].iter().all(
            |spec| spec.key.entity_id.as_str() == GPU_B && spec.subject == "GPU 1 · NVIDIA L4"
        ));
        assert_eq!(
            stats[2].key.metric_id.as_str(),
            "gpu.memory.framebuffer.used"
        );
        assert_eq!(stats[1].scale_max, Some(100.0));
    }
}
