//! Series the HistoryService queries for a session on the history view.
//! The charts (D-46, D-50) are the overview's series over a longer window:
//! the CPU total against 100 %, the RAM in use against the RAM total, the
//! host's total power and each CPU package's power against their peaks
//! (D-106), each GPU's utilization, memory in use, temperature and power
//! against their scales ([`gpu_series`]), and the read/write and
//! receive/transmit rates of one disk and one interface. The disk and the
//! interface are the busiest when the view opens and stay while they
//! report, so the charts never jump between devices. A memory chart names
//! the GPUs whose tracked processes' parts it stacks (D-64): a GPU's memory
//! its GPU, the RAM used the GPUs that share the RAM; the history joins
//! them (D-99). On the GPU view the HistoryService reads only the power of
//! each GPU that reports no power limit, whose recorded peak scales its
//! power bar (D-118).

use std::collections::BTreeSet;

use dgxtop_core::canonical::{CatalogSnapshot, ImmutableSnapshot};
use dgxtop_core::commands::ViewKind;
use dgxtop_core::metric::ids;
use dgxtop_core::naming::entity;
use dgxtop_core::view::{HistoryChartKind, TEMPERATURE_SCALE_C};

use super::disk::device_views;
use super::gpu::gpu_name;
use super::gpu_directory::{GpuDirectory, GpuIdentity};
use super::network::ranked_interfaces;
use super::reader::MetricReader;
use crate::history::{ChartRequest, HistoryChartSpec, HistoryPicks, MAX_CHARTS};

/// CPU packages whose power is charted, the first ones by number: a host
/// has one or two.
const CPU_POWER_CHARTS: usize = 2;
/// Charts besides the GPUs: CPU, memory, the total power, the CPU
/// packages' power and two rates each of a disk and an interface; the GPUs
/// get the rest of [`MAX_CHARTS`].
const HOST_CHARTS: usize = 7 + CPU_POWER_CHARTS;
const PERCENT_SCALE: f64 = 100.0;

/// Series of one GPU: utilization, memory, temperature, power.
const SERIES_PER_GPU: usize = 4;

/// The series for a session's request: the history view's charts, or the
/// GPU view's power.
pub(crate) fn history_charts(
    snapshot: &ImmutableSnapshot,
    catalog: &CatalogSnapshot,
    now_boot_ns: u64,
    request: &ChartRequest<'_>,
) -> HistoryPicks {
    let reader = MetricReader::new(snapshot, catalog, now_boot_ns);
    let charts = match request.view {
        ViewKind::GpuDetail => unlimited_power(&reader, catalog),
        _ => dashboard(&reader, catalog, request.shown),
    };
    HistoryPicks { charts }
}

/// The power of each GPU that reports no power limit (GB10), whose highest
/// draw recorded scales its power bar on the GPU view (D-118).
fn unlimited_power(reader: &MetricReader<'_>, catalog: &CatalogSnapshot) -> Vec<HistoryChartSpec> {
    let mut charts = Charts {
        reader,
        specs: Vec::new(),
    };
    let directory = GpuDirectory::new(catalog, reader.index());
    for gpu in directory.gpus().iter().take(MAX_CHARTS) {
        let entity_id = gpu.entity_id.as_str();
        let limit = reader
            .cell(ids::GPU_POWER_LIMIT, entity_id, entity::SCOPE_GPU)
            .value_f64();
        if limit.is_none() {
            charts.add(
                HistoryChartKind::GpuPower,
                &gpu_subject(gpu),
                (ids::GPU_POWER_DRAW, entity_id, entity::SCOPE_GPU),
                None,
            );
        }
    }
    charts.specs
}

/// `GPU 0 · NVIDIA GB10`: what a GPU's charts are of.
fn gpu_subject(gpu: &GpuIdentity<'_>) -> String {
    format!("{} · {}", gpu.label, gpu_name(gpu.record))
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
    charts.add_power();
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
    let unified = directory.unified_gpus();
    for spec in &mut charts.specs {
        spec.parts_of = match spec.kind {
            HistoryChartKind::GpuMemory => BTreeSet::from([spec.key.entity_id.clone()]),
            HistoryChartKind::MemoryUsed => unified.clone(),
            _ => BTreeSet::new(),
        };
    }
    charts.specs
}

/// The RAM total, the scale of the memory chart and of a unified GPU's.
pub(crate) fn ram_total(reader: &MetricReader<'_>) -> Option<f64> {
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
        ids::GPU_MEMORY_PROCESS_ALLOCATED
    } else {
        ids::GPU_MEMORY_FB_USED
    };
    let memory = (memory, gpu_memory_scale(reader, gpu));
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

/// The top of a GPU's memory scale: its framebuffer, or on unified memory
/// the RAM its processes draw from.
pub(crate) fn gpu_memory_scale(reader: &MetricReader<'_>, gpu: &GpuIdentity<'_>) -> Option<f64> {
    if gpu.has_unified_memory() {
        ram_total(reader)
    } else {
        reader
            .cell(
                ids::GPU_MEMORY_FB_TOTAL,
                gpu.entity_id.as_str(),
                entity::SCOPE_GPU,
            )
            .value_f64()
    }
}

/// `N` of a `cpu.package.N` entity.
fn package_number(entity_id: &str) -> Option<u32> {
    entity_id.strip_prefix("cpu.package.")?.parse().ok()
}

/// Collects the charts whose series the snapshot reports.
struct Charts<'r, 'a> {
    reader: &'r MetricReader<'a>,
    specs: Vec<HistoryChartSpec>,
}

impl Charts<'_, '_> {
    /// Adds the host's total power and the power of its first CPU packages
    /// (D-106), each scaled to its peak: neither has a limit to scale to.
    fn add_power(&mut self) {
        self.add(
            HistoryChartKind::TotalPower,
            "",
            (ids::HOST_POWER_TOTAL, entity::HOST, entity::SCOPE_HOST),
            None,
        );
        let mut packages: Vec<(u32, String, String)> = self
            .reader
            .index()
            .entries_of_metric(ids::CPU_POWER_PACKAGE)
            .filter_map(|entry| {
                let key = &entry.sample.key;
                let number = package_number(key.entity_id.as_str())?;
                Some((number, key.entity_id.to_string(), key.scope.to_string()))
            })
            .collect();
        packages.sort();
        for (number, entity_id, scope) in packages.into_iter().take(CPU_POWER_CHARTS) {
            self.add(
                HistoryChartKind::CpuPower,
                &format!("CPU {number}"),
                (ids::CPU_POWER_PACKAGE, &entity_id, &scope),
                None,
            );
        }
    }

    /// Adds the series of `gpu` it reports ([`gpu_series`]).
    fn add_gpu(&mut self, gpu: &GpuIdentity<'_>) {
        let subject = gpu_subject(gpu);
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
                parts_of: BTreeSet::new(),
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
            NOW,
            &ChartRequest {
                view: ViewKind::History,
                shown,
            },
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

    /// On the GPU view the history reads only the power of each GPU that
    /// reports no power limit (D-118), whose recorded peak scales its bar.
    #[test]
    fn the_gpu_view_picks_the_power_of_gpus_without_a_limit() {
        let mut fixture = host();
        fixture.add("gpu", sample("gpu.power.draw", GPU_A, "gpu").f64(300.0));
        fixture.add("gpu", sample("gpu.power.limit", GPU_A, "gpu").f64(700.0));
        fixture.add_entity(entity_record(
            GPU_B,
            EntityKind::Gpu,
            "NVIDIA GB10",
            &[(attributes::NVML_INDEX, "1")],
        ));
        fixture.add("gpu", sample("gpu.power.draw", GPU_B, "gpu").f64(12.4));
        fixture.add("gpu", sample("gpu.power.limit", GPU_B, "gpu").unavailable());
        let request = ChartRequest {
            view: ViewKind::GpuDetail,
            shown: &[],
        };
        let power = history_charts(&fixture.snapshot, &fixture.catalog, NOW, &request).charts;
        assert_eq!(kinds(&power), vec![HistoryChartKind::GpuPower], "{power:?}");
        assert_eq!(power[0].key.entity_id.as_str(), GPU_B);
        assert_eq!(power[0].key.metric_id.as_str(), ids::GPU_POWER_DRAW);
        assert_eq!(power[0].subject, "GPU 1 · NVIDIA GB10");
        assert_eq!(power[0].scale_max, None);
    }

    /// A memory chart names the GPUs whose processes' parts it stacks
    /// (D-64, D-99): a GPU's memory its own GPU, the RAM used the GPUs
    /// that share the RAM (none on a discrete GPU); the rest none.
    #[test]
    fn memory_charts_name_the_gpus_of_their_parts() {
        let parts_of = |fixture: &Fixture| -> Vec<(HistoryChartKind, Vec<String>)> {
            charts(fixture, &[])
                .iter()
                .map(|spec| {
                    let gpus = spec.parts_of.iter().map(ToString::to_string).collect();
                    (spec.kind, gpus)
                })
                .collect()
        };
        let mut fixture = host();
        fixture.add(
            "gpu",
            sample("gpu.memory.framebuffer.used", GPU_A, "gpu").u64(1 << 30),
        );
        let discrete = parts_of(&fixture);
        assert!(discrete.contains(&(HistoryChartKind::GpuMemory, vec![GPU_A.to_owned()])));
        assert!(discrete.contains(&(HistoryChartKind::MemoryUsed, Vec::new())));
        assert!(discrete.contains(&(HistoryChartKind::CpuUtilization, Vec::new())));
        fixture.add_entity(entity_record(
            GPU_B,
            EntityKind::Gpu,
            "NVIDIA GB10",
            &[
                (attributes::NVML_INDEX, "1"),
                (attributes::MEMORY_MODEL, "unified"),
            ],
        ));
        let unified = parts_of(&fixture);
        assert!(unified.contains(&(HistoryChartKind::MemoryUsed, vec![GPU_B.to_owned()])));
    }

    /// The history keeps only the Overview metrics in every bar size
    /// (D-82): every series the view charts or summarizes is one of them.
    #[test]
    fn every_pick_is_a_series_the_history_keeps_in_every_size() {
        use dgxtop_core::overview_history::is_overview_metric;
        let metrics = [
            ids::CPU_UTILIZATION_TOTAL,
            ids::MEMORY_USED,
            ids::GPU_UTILIZATION_COMPUTE,
            ids::GPU_MEMORY_FB_USED,
            ids::GPU_MEMORY_PROCESS_ALLOCATED,
            ids::GPU_TEMPERATURE,
            ids::GPU_POWER_DRAW,
            ids::HOST_POWER_TOTAL,
            ids::CPU_POWER_PACKAGE,
            ids::DISK_READ_BPS,
            ids::DISK_WRITE_BPS,
            ids::NET_RX_BPS,
            ids::NET_TX_BPS,
        ];
        assert!(metrics.iter().all(|metric| is_overview_metric(metric)));
        let mut fixture = host();
        disk(&mut fixture, "blockdev:259:0", "nvme0n1", 100.0);
        fixture.add(
            "cpu",
            sample(ids::HOST_POWER_TOTAL, "host", "host").f64(152.0),
        );
        fixture.add(
            "cpu",
            sample(ids::CPU_POWER_PACKAGE, "cpu.package.0", "package:0").f64(25.0),
        );
        let picked = pick(&fixture, &[]);
        for spec in &picked.charts {
            assert!(
                is_overview_metric(spec.key.metric_id.as_str()),
                "{}",
                spec.key.metric_id
            );
        }
    }

    /// The host's total power and each CPU package's power follow the
    /// memory (D-106), scaled to their peaks; a host whose CPU power cannot
    /// be read has neither.
    #[test]
    fn the_total_and_the_cpu_packages_power_are_charted() {
        let mut fixture = host();
        let without = charts(&fixture, &[]);
        assert!(without.iter().all(|chart| !chart.kind.is_power()));
        fixture.add(
            "cpu",
            sample(ids::HOST_POWER_TOTAL, "host", "host").f64(152.0),
        );
        for package in [2_u32, 0, 1] {
            fixture.add(
                "cpu",
                sample(
                    ids::CPU_POWER_PACKAGE,
                    &format!("cpu.package.{package}"),
                    &format!("package:{package}"),
                )
                .f64(20.0),
            );
        }
        let charts = charts(&fixture, &[]);
        let power: Vec<(HistoryChartKind, &str, &str, Option<f64>)> = charts
            .iter()
            .filter(|chart| chart.kind.is_power())
            .map(|chart| {
                (
                    chart.kind,
                    chart.subject.as_str(),
                    chart.key.entity_id.as_str(),
                    chart.scale_max,
                )
            })
            .collect();
        assert_eq!(
            power,
            vec![
                (HistoryChartKind::TotalPower, "", "host", None),
                (HistoryChartKind::CpuPower, "CPU 0", "cpu.package.0", None),
                (HistoryChartKind::CpuPower, "CPU 1", "cpu.package.1", None),
            ],
            "the first two packages"
        );
        assert_eq!(
            kinds(&charts)[..3],
            [
                HistoryChartKind::CpuUtilization,
                HistoryChartKind::MemoryUsed,
                HistoryChartKind::TotalPower,
            ]
        );
        assert_eq!(MAX_CHARTS, HOST_CHARTS + SERIES_PER_GPU * 8);
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
}
