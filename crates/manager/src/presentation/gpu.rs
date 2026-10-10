//! GPU module projection: one card per `gpu:<UUID>` entity with every
//! METRICS.md GPU field. Unsupported fields stay "unsupported" (never 0)
//! and the views leave them out (D-52: no notes).

use dgxtop_core::identity::EntityId;
use dgxtop_core::metric::{Scalar, ids, native};
use dgxtop_core::naming::{attributes, entity};
use dgxtop_core::observation::EntityRecord;
use dgxtop_core::view::{
    GpuCard, GpuDetails, GpuView, HostState, HostStatus, LabeledCell, MetricCell,
};

use super::gpu_derived::{memory_bandwidth, pcie_bandwidth};
use super::gpu_directory::{GpuDirectory, GpuIdentity};
use super::labels::{host_state_text, label_text};
use super::reader::MetricReader;

pub(crate) const GPU_DISABLED_REASON: &str =
    "GPU monitoring disabled (sampling.gpu_enabled = false)";
pub(crate) const NO_GPU_REASON: &str = "no NVIDIA GPU detected";
const DISCOVERY_REASON: &str = "GPU discovery in progress";
const UNKNOWN_MODEL: &str = "unknown model";
const CURRENT_CLOCKS: [(&str, &str); 4] = [
    (ids::GPU_CLOCK_GRAPHICS, "graphics"),
    (ids::GPU_CLOCK_MEMORY, "memory"),
    (ids::GPU_CLOCK_SM, "sm"),
    (ids::GPU_CLOCK_VIDEO, "video"),
];
const METADATA_KEYS: [&str; 7] = [
    attributes::UUID,
    attributes::PCI_BUS_ID,
    attributes::DRIVER_VERSION,
    attributes::SERIAL,
    attributes::COMPUTE_MODE,
    attributes::PERSISTENCE_MODE,
    attributes::MEMORY_MODEL,
];
/// Identity, firmware, topology (D-54) and software attributes of the card
/// the GPU view shows (D-48).
const DETAIL_KEYS: [&str; 20] = [
    attributes::ARCHITECTURE,
    attributes::BRAND,
    attributes::COMPUTE_CAPABILITY,
    attributes::VBIOS_VERSION,
    attributes::INFOROM_IMAGE,
    attributes::GSP_FIRMWARE,
    attributes::BOARD_PART_NUMBER,
    attributes::MINOR_NUMBER,
    attributes::PCI_DEVICE_ID,
    attributes::PCI_SUBSYSTEM_ID,
    attributes::NVML_VERSION,
    attributes::CUDA_DRIVER_VERSION,
    attributes::KERNEL_MODULE,
    attributes::CUDA_TOOLKIT,
    attributes::CUDA_TOOLKIT_PATH,
    attributes::NVCC_VERSION,
    attributes::CUDA_TOOLKITS,
    attributes::NUMA_NODES,
    attributes::CPU_AFFINITY,
    attributes::GPU_PEERS,
];
/// Scope prefix of a fan's speed (`fan:0`).
const FAN_SCOPE_PREFIX: &str = "fan:";
/// Causes of throttle time, in the order the GPU view lists them.
const THROTTLE_CAUSES: [(&str, &str); 5] = [
    (entity::SCOPE_THROTTLE_POWER, "power"),
    (entity::SCOPE_THROTTLE_THERMAL, "thermal"),
    (entity::SCOPE_THROTTLE_SYNC_BOOST, "sync boost"),
    (entity::SCOPE_THROTTLE_BOARD_LIMIT, "board limit"),
    (entity::SCOPE_THROTTLE_RELIABILITY, "reliability"),
];

/// Trend widths: the selected card gets the detail width, the others the
/// summary width (0 = no trend copied).
#[derive(Debug, Clone, Copy)]
pub(crate) struct GpuTrendWidths {
    pub(crate) selected: usize,
    pub(crate) others: usize,
    /// The GPU view: its card also gets the memory, temperature and power
    /// trends and its processes; the overview charts utilization only
    /// (D-43, D-47).
    pub(crate) detail: bool,
}

/// Trend widths of one card (0 = no trend copied).
#[derive(Debug, Clone, Copy)]
struct CardTrendWidths {
    utilization: usize,
    memory_temperature_and_power: usize,
}

/// Facts explaining why GPU data may be absent.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GpuEnvironment<'a> {
    pub(crate) nvml_status: Option<&'a HostStatus>,
    pub(crate) gpu_enabled: bool,
}

pub(crate) fn build_gpu_view(
    reader: &MetricReader<'_>,
    directory: &GpuDirectory<'_>,
    requested: Option<&EntityId>,
    environment: GpuEnvironment<'_>,
    widths: GpuTrendWidths,
) -> GpuView {
    let selected = requested
        .filter(|entity_id| directory.contains(entity_id))
        .cloned()
        .or_else(|| directory.gpus().first().map(|gpu| gpu.entity_id.clone()));
    let cards: Vec<GpuCard> = directory
        .gpus()
        .iter()
        .map(|gpu| {
            let is_selected = selected.as_ref() == Some(&gpu.entity_id);
            let width = if is_selected {
                widths.selected
            } else {
                widths.others
            };
            let card_widths = CardTrendWidths {
                utilization: width,
                memory_temperature_and_power: if widths.detail { width } else { 0 },
            };
            let mut card = build_card(reader, gpu, card_widths);
            if widths.detail && is_selected {
                card.details = Some(Box::new(build_details(reader, gpu, &card)));
            }
            card
        })
        .collect();
    let unavailable_reason = unavailable_reason(cards.is_empty(), environment);
    GpuView {
        cards,
        selected,
        unavailable_reason,
    }
}

fn build_card(
    reader: &MetricReader<'_>,
    gpu: &GpuIdentity<'_>,
    widths: CardTrendWidths,
) -> GpuCard {
    let entity_id = gpu.entity_id.as_str();
    let cell = |metric: &str| reader.cell_preferring(metric, entity_id, entity::SCOPE_GPU);
    let trend = |metric: &str, width: usize| {
        reader.trend_preferring(metric, entity_id, entity::SCOPE_GPU, width)
    };
    GpuCard {
        entity_id: gpu.entity_id.clone(),
        label: gpu.label.clone(),
        name: gpu_name(gpu.record),
        uuid: gpu_uuid(gpu),
        unified_memory: gpu.has_unified_memory(),
        utilization: cell(ids::GPU_UTILIZATION_COMPUTE),
        memory_busy: cell(ids::GPU_UTILIZATION_MEMORY_BUSY),
        temperature: cell(ids::GPU_TEMPERATURE),
        power_draw: cell(ids::GPU_POWER_DRAW),
        power_limit: cell(ids::GPU_POWER_LIMIT),
        clocks: clock_cells(reader, entity_id),
        fan: cell(ids::GPU_FAN_SPEED),
        framebuffer_used: cell(ids::GPU_MEMORY_FB_USED),
        framebuffer_free: cell(ids::GPU_MEMORY_FB_FREE),
        framebuffer_total: cell(ids::GPU_MEMORY_FB_TOTAL),
        framebuffer_reserved: cell(ids::GPU_MEMORY_FB_RESERVED),
        process_allocated: cell(ids::GPU_MEMORY_PROCESS_ALLOCATED),
        pcie_tx: cell(ids::GPU_PCIE_TX),
        pcie_rx: cell(ids::GPU_PCIE_RX),
        energy: cell(ids::GPU_ENERGY_TOTAL),
        ecc_corrected: cell(ids::GPU_ECC_CORRECTED),
        ecc_uncorrected: cell(ids::GPU_ECC_UNCORRECTED),
        retired_single: cell(ids::GPU_RETIRED_PAGES_SINGLE),
        retired_double: cell(ids::GPU_RETIRED_PAGES_DOUBLE),
        encoder: cell(ids::GPU_ENCODER_UTILIZATION),
        decoder: cell(ids::GPU_DECODER_UTILIZATION),
        nvlink_active: cell(ids::GPU_NVLINK_ACTIVE),
        performance_state: cell(ids::GPU_PERFORMANCE_STATE),
        throttle_reasons: cell(ids::GPU_THROTTLE_REASONS),
        temperature_slowdown: reader.cell(
            ids::GPU_TEMPERATURE_THRESHOLD,
            entity_id,
            entity::SCOPE_SLOWDOWN,
        ),
        temperature_shutdown: reader.cell(
            ids::GPU_TEMPERATURE_THRESHOLD,
            entity_id,
            entity::SCOPE_SHUTDOWN,
        ),
        pcie_generation: cell(ids::GPU_PCIE_LINK_GENERATION),
        pcie_width: cell(ids::GPU_PCIE_LINK_WIDTH),
        pcie_generation_max: reader.cell(
            ids::GPU_PCIE_LINK_MAXIMUM,
            entity_id,
            entity::SCOPE_LINK_GENERATION,
        ),
        pcie_width_max: reader.cell(
            ids::GPU_PCIE_LINK_MAXIMUM,
            entity_id,
            entity::SCOPE_LINK_WIDTH,
        ),
        details: None,
        removed: gpu
            .record
            .is_some_and(|record| record.removed_boot_ns.is_some()),
        metadata: attribute_pairs(gpu.record, &METADATA_KEYS),
        utilization_trend: trend(ids::GPU_UTILIZATION_COMPUTE, widths.utilization),
        temperature_trend: trend(ids::GPU_TEMPERATURE, widths.memory_temperature_and_power),
        power_trend: trend(ids::GPU_POWER_DRAW, widths.memory_temperature_and_power),
        // A unified-memory GPU has no framebuffer: its memory in use is the
        // RAM its processes hold.
        memory_trend: trend(
            if gpu.has_unified_memory() {
                ids::GPU_MEMORY_PROCESS_ALLOCATED
            } else {
                ids::GPU_MEMORY_FB_USED
            },
            widths.memory_temperature_and_power,
        ),
    }
}

/// What the GPU view shows of `card` beyond it (D-48): capabilities, slow
/// health, the theoretical bandwidths, the driver's PCIe reading while its
/// unit is unverified, and the identity and software attributes.
fn build_details(reader: &MetricReader<'_>, gpu: &GpuIdentity<'_>, card: &GpuCard) -> GpuDetails {
    let entity_id = gpu.entity_id.as_str();
    let cell = |metric: &str| reader.cell_preferring(metric, entity_id, entity::SCOPE_GPU);
    let threshold = |scope: &str| reader.cell(ids::GPU_TEMPERATURE_THRESHOLD, entity_id, scope);
    let memory_bus_width = cell(ids::GPU_MEMORY_BUS_WIDTH);
    let max_memory_clock = reader.cell(ids::GPU_CLOCK_MAXIMUM, entity_id, "memory");
    GpuDetails {
        pcie_bandwidth: pcie_bandwidth(&card.pcie_generation, &card.pcie_width),
        pcie_bandwidth_max: pcie_bandwidth(&card.pcie_generation_max, &card.pcie_width_max),
        pcie_tx_driver_kb: driver_reading(reader, entity_id, ids::GPU_PCIE_TX, &card.pcie_tx),
        pcie_rx_driver_kb: driver_reading(reader, entity_id, ids::GPU_PCIE_RX, &card.pcie_rx),
        pcie_replays: cell(ids::GPU_PCIE_REPLAYS),
        cores: cell(ids::GPU_CORES),
        memory_bandwidth_max: memory_bandwidth(&max_memory_clock, &memory_bus_width),
        memory_bus_width,
        bar1_used: cell(ids::GPU_MEMORY_BAR1_USED),
        bar1_total: cell(ids::GPU_MEMORY_BAR1_TOTAL),
        power_limit_default: cell(ids::GPU_POWER_LIMIT_DEFAULT),
        power_limit_minimum: cell(ids::GPU_POWER_LIMIT_MINIMUM),
        power_limit_maximum: cell(ids::GPU_POWER_LIMIT_MAXIMUM),
        power_limit_requested: cell(ids::GPU_POWER_LIMIT_REQUESTED),
        power_instant: cell(ids::GPU_POWER_INSTANT),
        fans: fan_cells(reader, entity_id),
        temperature_max_operating: threshold(entity::SCOPE_MAX_OPERATING),
        temperature_target: threshold(entity::SCOPE_TARGET),
        encoder_sessions: cell(ids::GPU_ENCODER_SESSIONS),
        encoder_fps: cell(ids::GPU_ENCODER_FPS),
        encoder_latency: cell(ids::GPU_ENCODER_LATENCY),
        throttle_time: THROTTLE_CAUSES
            .iter()
            .map(|(scope, label)| LabeledCell {
                label: (*label).to_owned(),
                cell: reader.cell(ids::GPU_THROTTLE_TIME, entity_id, scope),
            })
            .collect(),
        attributes: attribute_pairs(gpu.record, &DETAIL_KEYS),
    }
}

/// The driver's own reading of a PCIe throughput the collector withheld
/// for its unverified unit (D-11, D-48).
fn driver_reading(
    reader: &MetricReader<'_>,
    entity_id: &str,
    metric: &str,
    cell: &MetricCell,
) -> Option<f64> {
    if cell.value.is_some() {
        return None;
    }
    let entry = reader.index().find(metric, entity_id, entity::SCOPE_GPU)?;
    reader
        .explanation(&entry.sample.key)?
        .candidates
        .iter()
        .filter(|candidate| is_unverified_unit(candidate.native_unit.as_str()))
        .find_map(|candidate| candidate.native_value.map(Scalar::to_f64_lossy))
}

/// NVML's PCIe throughput unit, whose KB is not proven 1000 or 1024 bytes.
fn is_unverified_unit(native_unit: &str) -> bool {
    native_unit == native::KB_UNVERIFIED_PER_SECOND
}

pub(crate) fn gpu_name(record: Option<&EntityRecord>) -> String {
    record
        .map(|record| label_text(&record.display_name))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| UNKNOWN_MODEL.to_owned())
}

fn gpu_uuid(gpu: &GpuIdentity<'_>) -> Option<String> {
    gpu.record
        .and_then(|record| record.attribute(attributes::UUID))
        .or_else(|| entity::parse_gpu(gpu.entity_id.as_str()))
        .map(label_text)
}

/// Current clocks always (missing ones say why); maximum clocks only when
/// the driver reported them (scope = clock domain).
fn clock_cells(reader: &MetricReader<'_>, entity_id: &str) -> Vec<LabeledCell> {
    let current = CURRENT_CLOCKS.iter().map(|(metric, label)| LabeledCell {
        label: (*label).to_owned(),
        cell: reader.cell_preferring(metric, entity_id, entity::SCOPE_GPU),
    });
    let maximum = CURRENT_CLOCKS.iter().filter_map(|(_, domain)| {
        let entry = reader
            .index()
            .get(ids::GPU_CLOCK_MAXIMUM, entity_id, domain)?;
        Some(LabeledCell {
            label: format!("{domain} max"),
            cell: reader.cell_of(entry),
        })
    });
    current.chain(maximum).collect()
}

/// Each fan's speed in fan order (scope `fan:<n>`), labeled `fan <n>`.
fn fan_cells(reader: &MetricReader<'_>, entity_id: &str) -> Vec<LabeledCell> {
    let mut fans: Vec<(u32, LabeledCell)> = reader
        .index()
        .entries_of_entity(ids::GPU_FAN_SPEED, entity_id)
        .into_iter()
        .filter_map(|entry| {
            let fan: u32 = entry
                .sample
                .key
                .scope
                .as_str()
                .strip_prefix(FAN_SCOPE_PREFIX)?
                .parse()
                .ok()?;
            let cell = reader.cell_of(entry);
            Some((
                fan,
                LabeledCell {
                    label: format!("fan {fan}"),
                    cell,
                },
            ))
        })
        .collect();
    fans.sort_by_key(|(fan, _)| *fan);
    fans.into_iter().map(|(_, cell)| cell).collect()
}

/// The `keys` the entity record carries, in the order of `keys`.
fn attribute_pairs(record: Option<&EntityRecord>, keys: &[&str]) -> Vec<(String, String)> {
    let Some(record) = record else {
        return Vec::new();
    };
    keys.iter()
        .filter_map(|key| {
            record
                .attribute(key)
                .map(|value| ((*key).to_owned(), label_text(value)))
        })
        .collect()
}

/// Why GPU data is missing or not refreshing: disabled by settings, an
/// NVML host that is not serving (with its detail), discovery still
/// running, or no GPU at all.
fn unavailable_reason(no_cards: bool, environment: GpuEnvironment<'_>) -> Option<String> {
    if !environment.gpu_enabled {
        return Some(GPU_DISABLED_REASON.to_owned());
    }
    if let Some(status) = environment.nvml_status {
        match status.state {
            HostState::Healthy | HostState::Degraded => {}
            HostState::Starting if no_cards => return Some(DISCOVERY_REASON.to_owned()),
            HostState::Starting => {}
            HostState::CircuitOpen
            | HostState::Quarantined
            | HostState::Disabled
            | HostState::Unavailable => return Some(host_state_text("NVML", status)),
        }
    }
    no_cards.then(|| NO_GPU_REASON.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::index::SampleIndex;
    use crate::presentation::testkit::{
        Fixture, NOW, candidate, entity_record, explanation_for, host_status, sample,
    };
    use dgxtop_core::canonical::{CandidateDisposition, RejectReason};
    use dgxtop_core::metric::CanonicalUnit;
    use dgxtop_core::observation::EntityKind;
    use dgxtop_core::quality::SampleKind;
    use dgxtop_core::testing::metric_key;
    use dgxtop_core::view::CellState;

    const GPU_A: &str = "gpu:GPU-aaaa1111";
    const GPU_B: &str = "gpu:GPU-bbbb2222";

    fn widths() -> GpuTrendWidths {
        GpuTrendWidths {
            selected: 60,
            others: 0,
            detail: true,
        }
    }

    fn enabled() -> GpuEnvironment<'static> {
        GpuEnvironment {
            nvml_status: None,
            gpu_enabled: true,
        }
    }

    fn build(
        fixture: &Fixture,
        requested: Option<&str>,
        environment: GpuEnvironment<'_>,
    ) -> GpuView {
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, &fixture.trends, NOW);
        let index = SampleIndex::new(&fixture.snapshot);
        let directory = GpuDirectory::new(&fixture.catalog, &index);
        let requested = requested.map(|id| EntityId::new(id).unwrap());
        build_gpu_view(
            &reader,
            &directory,
            requested.as_ref(),
            environment,
            widths(),
        )
    }

    fn two_gpus() -> Fixture {
        let mut fixture = Fixture::new();
        fixture.add_entity(entity_record(
            GPU_B,
            EntityKind::Gpu,
            "NVIDIA H100 80GB HBM3",
            &[
                (attributes::NVML_INDEX, "1"),
                (attributes::UUID, "GPU-bbbb2222"),
                (attributes::PCI_BUS_ID, "00000000:1B:00.0"),
                (attributes::DRIVER_VERSION, "550.54"),
                (attributes::PERSISTENCE_MODE, "enabled"),
            ],
        ));
        fixture.add_entity(entity_record(
            GPU_A,
            EntityKind::Gpu,
            "NVIDIA GB10",
            &[
                (attributes::NVML_INDEX, "0"),
                (attributes::MEMORY_MODEL, "unified"),
            ],
        ));
        fixture.add(
            "gpu",
            sample("gpu.utilization.compute", GPU_A, "gpu").f64(0.0),
        );
        fixture.add("gpu", sample("gpu.temperature", GPU_B, "gpu").f64(55.0));
        fixture.add(
            "gpu",
            sample("gpu.memory.framebuffer.used", GPU_A, "gpu")
                .unavailable()
                .reason("NVML_NOT_SUPPORTED"),
        );
        fixture.add(
            "gpu",
            sample("gpu.ecc.corrected", GPU_B, "gpu")
                .unavailable()
                .reason("UNSUPPORTED"),
        );
        fixture
    }

    #[test]
    fn cards_are_labeled_by_nvml_index_and_identified_by_entity() {
        let view = build(&two_gpus(), None, enabled());
        let labels: Vec<(&str, &str)> = view
            .cards
            .iter()
            .map(|card| (card.entity_id.as_str(), card.label.as_str()))
            .collect();
        assert_eq!(labels, vec![(GPU_A, "GPU 0"), (GPU_B, "GPU 1")]);
        assert_eq!(view.selected.as_ref().map(EntityId::as_str), Some(GPU_A));
        assert_eq!(view.unavailable_reason, None);
        let first = &view.cards[0];
        assert_eq!(first.utilization.value_f64(), Some(0.0));
        assert_eq!(first.utilization.state, CellState::Ok);
        assert_eq!(first.uuid.as_deref(), Some("GPU-aaaa1111"));
        assert_eq!(first.temperature.state, CellState::Unavailable);
    }

    #[test]
    fn unsupported_fields_stay_unsupported_and_cards_carry_no_notes() {
        let view = build(&two_gpus(), None, enabled());
        let unified = &view.cards[0];
        assert!(unified.unified_memory);
        assert_eq!(unified.framebuffer_used.state, CellState::Unsupported);
        assert_eq!(unified.framebuffer_used.value, None);
        let discrete = &view.cards[1];
        assert_eq!(discrete.ecc_corrected.state, CellState::Unsupported);
        assert_eq!(discrete.ecc_corrected.value, None);
        assert!(!unified.removed && !discrete.removed);
        let metadata: Vec<&str> = discrete
            .metadata
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(
            metadata,
            vec!["uuid", "pci_bus_id", "driver_version", "persistence_mode"]
        );
        assert_eq!(discrete.name, "NVIDIA H100 80GB HBM3");
    }

    #[test]
    fn a_removed_gpu_is_marked_removed() {
        let mut fixture = two_gpus();
        let mut record = entity_record(
            GPU_B,
            EntityKind::Gpu,
            "NVIDIA H100 80GB HBM3",
            &[
                (attributes::NVML_INDEX, "1"),
                (attributes::UUID, "GPU-bbbb2222"),
            ],
        );
        record.removed_boot_ns = Some(NOW);
        fixture.add_entity(record);
        let view = build(&fixture, None, enabled());
        let removed: Vec<bool> = view.cards.iter().map(|card| card.removed).collect();
        assert_eq!(removed, vec![false, true]);
    }

    #[test]
    fn an_unverified_pcie_unit_is_never_zeroed() {
        let mut fixture = two_gpus();
        fixture.add("gpu", sample("gpu.pcie.tx", GPU_B, "gpu").unavailable());
        let key = metric_key(GPU_B, "gpu.pcie.tx", "gpu");
        fixture.explain(
            "gpu",
            explanation_for(
                &key,
                vec![
                    candidate("nvml:1:pcie_tx", "nvml")
                        .native(Scalar::from_u64(1200), "KB_unverified_per_second")
                        .disposition(CandidateDisposition::Rejected(RejectReason::UnknownUnit))
                        .build(),
                ],
            ),
        );
        let view = build(&fixture, None, enabled());
        assert_eq!(view.cards[1].pcie_tx.value, None);
    }

    /// GPU B with its capabilities and slow health, as NVML reports them
    /// in canonical units.
    fn deep_gpu() -> Fixture {
        let mut fixture = two_gpus();
        for (metric, scope, value) in [
            ("gpu.cores", "gpu", 10_752.0),
            ("gpu.memory.bus_width", "gpu", 256.0),
            ("gpu.clock.maximum", "memory", 15_001.0),
            ("gpu.memory.bar1.total", "gpu", 16.0 * 1_073_741_824.0),
            ("gpu.memory.bar1.used", "gpu", 6.0 * 1_048_576.0),
            ("gpu.power.limit.default", "gpu", 360.0),
            ("gpu.power.limit.minimum", "gpu", 300.0),
            ("gpu.power.limit.maximum", "gpu", 390.0),
            ("gpu.temperature.threshold", "max_operating", 88.0),
            ("gpu.encoder.sessions", "gpu", 1.0),
            ("gpu.encoder.fps", "gpu", 60.0),
            ("gpu.encoder.latency", "gpu", 2.5),
            ("gpu.pcie.replays", "gpu", 0.0),
            ("gpu.throttle.time", "power", 219.0),
            ("gpu.throttle.time", "thermal", 0.0),
        ] {
            fixture.add("gpu", sample(metric, GPU_B, scope).f64(value));
        }
        // Link generations and widths are exact counts.
        for (metric, scope, value) in [
            ("gpu.pcie.link.generation", "gpu", 1),
            ("gpu.pcie.link.width", "gpu", 16),
            ("gpu.pcie.link.maximum", "generation", 5),
            ("gpu.pcie.link.maximum", "width", 16),
        ] {
            fixture.add("gpu", sample(metric, GPU_B, scope).u64(value));
        }
        fixture.add(
            "gpu",
            sample("gpu.temperature.threshold", GPU_B, "target")
                .unavailable()
                .reason("UNSUPPORTED"),
        );
        fixture
    }

    #[test]
    fn bandwidths_are_derived_from_clock_bus_and_link() {
        let view = build(&deep_gpu(), Some(GPU_B), enabled());
        let card = view.cards[1].details.as_deref().unwrap();
        let close = |cell: &MetricCell, expected: f64| {
            cell.value_f64()
                .is_some_and(|value| (value - expected).abs() < expected * 1e-9)
        };
        // 15001 MHz × 2 × 256 bit / 8 = 960 GB/s.
        assert!(close(&card.memory_bandwidth_max, 960.064e9), "{card:?}");
        assert_eq!(card.memory_bandwidth_max.kind, SampleKind::Derived);
        assert_eq!(
            card.memory_bandwidth_max.unit,
            CanonicalUnit::BytesPerSecond
        );
        // Gen 1 x16 now, Gen 5 x16 at best (per direction).
        assert!(close(&card.pcie_bandwidth, 4e9));
        assert!(close(
            &card.pcie_bandwidth_max,
            32e9 * 128.0 / 130.0 / 8.0 * 16.0
        ));
        // GPU A reports neither: no bandwidth, never zero.
        let view = build(&deep_gpu(), Some(GPU_A), enabled());
        let other = view.cards[0].details.as_deref().unwrap();
        assert_eq!(other.memory_bandwidth_max.value, None);
        assert_eq!(other.pcie_bandwidth.value, None);
        assert!(!other.memory_bandwidth_max.state.has_value());
    }

    #[test]
    fn details_carry_each_fan_in_order_the_power_readings_and_the_topology() {
        let mut fixture = deep_gpu();
        for (scope, speed) in [("fan:10", 30.0), ("fan:2", 47.0), ("fan:0", 45.0)] {
            fixture.add("gpu", sample("gpu.fan.speed", GPU_B, scope).f64(speed));
        }
        fixture.add("gpu", sample("gpu.power.instant", GPU_B, "gpu").f64(262.5));
        fixture.add(
            "gpu",
            sample("gpu.power.limit.requested", GPU_B, "gpu").f64(380.0),
        );
        fixture.add_entity(entity_record(
            GPU_B,
            EntityKind::Gpu,
            "NVIDIA H100 80GB HBM3",
            &[
                (attributes::NVML_INDEX, "1"),
                (attributes::CPU_AFFINITY, "0-31"),
                (attributes::GPU_PEERS, "0:PHB:none"),
            ],
        ));
        let view = build(&fixture, Some(GPU_B), enabled());
        let details = view.cards[1].details.as_deref().unwrap();
        let fans: Vec<(&str, Option<f64>)> = details
            .fans
            .iter()
            .map(|fan| (fan.label.as_str(), fan.cell.value_f64()))
            .collect();
        assert_eq!(
            fans,
            vec![
                ("fan 0", Some(45.0)),
                ("fan 2", Some(47.0)),
                ("fan 10", Some(30.0))
            ]
        );
        assert_eq!(details.power_instant.value_f64(), Some(262.5));
        assert_eq!(details.power_limit_requested.value_f64(), Some(380.0));
        assert!(
            details
                .attributes
                .contains(&("gpu_peers".to_owned(), "0:PHB:none".to_owned()))
        );
        // A GPU that reports neither has no fans and no reading, never 0.
        let view = build(&fixture, Some(GPU_A), enabled());
        let other = view.cards[0].details.as_deref().unwrap();
        assert!(other.fans.is_empty());
        assert_eq!(other.power_instant.value, None);
    }

    #[test]
    fn the_driver_pcie_reading_shows_while_its_unit_is_unverified() {
        let mut fixture = two_gpus();
        fixture.add("gpu", sample("gpu.pcie.tx", GPU_B, "gpu").unavailable());
        let key = metric_key(GPU_B, "gpu.pcie.tx", "gpu");
        fixture.explain(
            "gpu",
            explanation_for(
                &key,
                vec![
                    candidate("nvml:1:pcie_tx", "nvml")
                        .native(Scalar::from_u64(2243), "KB_unverified_per_second")
                        .disposition(CandidateDisposition::Rejected(RejectReason::UnknownUnit))
                        .build(),
                ],
            ),
        );
        let view = build(&fixture, Some(GPU_B), enabled());
        let card = &view.cards[1];
        assert_eq!(card.pcie_tx.value, None, "never converted by a guess");
        let details = card.details.as_deref().unwrap();
        assert_eq!(details.pcie_tx_driver_kb, Some(2243.0));
        assert_eq!(details.pcie_rx_driver_kb, None);
    }

    #[test]
    fn the_memory_trend_follows_the_framebuffer_or_the_unified_allocations() {
        let mut fixture = two_gpus();
        fixture.add(
            "gpu",
            sample("gpu.memory.framebuffer.used", GPU_B, "gpu").u64(5_000_000_000),
        );
        let discrete_used = metric_key(GPU_B, "gpu.memory.framebuffer.used", "gpu");
        let unified_used = metric_key(GPU_A, "gpu.memory.framebuffer.used", "gpu");
        let unified_held = metric_key(GPU_A, "gpu.memory.process_allocated", "gpu");
        fixture.trends.push(&discrete_used, Some(4.0e9));
        fixture.trends.push(&discrete_used, Some(5.0e9));
        fixture.trends.push(&unified_used, Some(1.0));
        fixture.trends.push(&unified_held, Some(4.3e10));
        // The test widths copy trends for the selected card only.
        // GPU_A (GB10) shares the RAM; GPU_B (H100) has its own framebuffer.
        let unified = build(&fixture, Some(GPU_A), enabled());
        assert_eq!(unified.cards[0].memory_trend.points, vec![Some(4.3e10)]);
        let discrete = build(&fixture, Some(GPU_B), enabled());
        assert_eq!(
            discrete.cards[1].memory_trend.points,
            vec![Some(4.0e9), Some(5.0e9)]
        );
    }

    #[test]
    fn reasons_when_gpu_data_is_absent() {
        let empty = Fixture::new();
        assert_eq!(
            build(&empty, None, enabled()).unavailable_reason.as_deref(),
            Some(NO_GPU_REASON)
        );
        let disabled = GpuEnvironment {
            nvml_status: None,
            gpu_enabled: false,
        };
        assert_eq!(
            build(&empty, None, disabled).unavailable_reason.as_deref(),
            Some(GPU_DISABLED_REASON)
        );
        let quarantined = host_status("nvml", HostState::Quarantined, Some("child not reaped"));
        let environment = GpuEnvironment {
            nvml_status: Some(&quarantined),
            gpu_enabled: true,
        };
        let view = build(&two_gpus(), None, environment);
        assert_eq!(view.cards.len(), 2);
        assert_eq!(
            view.unavailable_reason.as_deref(),
            Some("NVML quarantined (requires intervention): child not reaped")
        );
        let starting = host_status("nvml", HostState::Starting, None);
        let environment = GpuEnvironment {
            nvml_status: Some(&starting),
            gpu_enabled: true,
        };
        assert_eq!(
            build(&empty, None, environment)
                .unavailable_reason
                .as_deref(),
            Some("GPU discovery in progress")
        );
    }
}
