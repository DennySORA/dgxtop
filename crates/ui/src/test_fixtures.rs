//! Deterministic projections for unit tests: every module topic filled with
//! realistic values, missing states, a source conflict, an estimated value,
//! history gaps and (optionally) a pending confirmation.

use std::sync::Arc;

use dgxtop_core::action::{ConfirmationToken, PersistenceKind, PersistenceStatus, Signal};
use dgxtop_core::canonical::GpuUsageKind;
use dgxtop_core::commands::{SortKey, ViewKind};
use dgxtop_core::config::{Theme, ValueOrigin};
use dgxtop_core::identity::{ModuleId, ProcessKey, SessionId};
use dgxtop_core::metric::{CanonicalUnit, Scalar};
use dgxtop_core::quality::{Quality, SampleKind};
pub(crate) use dgxtop_core::testing::entity_id as entity;
pub(crate) use dgxtop_core::testing::metric_key;
use dgxtop_core::testing::test_process_key;
use dgxtop_core::view::{
    ActionsView, CellState, ConfirmationView, ConflictView, CoreView, CpuView, DiskDeviceView,
    DiskHistory, DiskView, FilesystemView, FrequencyGroup, GpuCard, GpuDetails, GpuView,
    HeaderView, HistoryChartKind, HistoryChartView, HistoryPointView, HistoryView,
    HistoryWindowSummary, HostState, HostStatus, InterfaceView, LabeledCell, LastGoodView,
    MemoryView, MetricCell, ModuleViews, NetworkView, OverloadState, ProcessGpuView,
    ProcessRowView, ProcessesView, SessionState, SettingField, SettingsView, Trend, ViewModel,
};

pub(crate) const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Context of a wide overview cell, for module unit tests.
pub(crate) fn display_context() -> crate::registry::DisplayContext {
    crate::registry::DisplayContext {
        theme: crate::theme::ThemePalette::default(),
        viewport: ratatui::layout::Rect::new(0, 0, 160, 50),
        tick_for_animation: 0,
        compact: false,
        slot: crate::registry::Slot::Overview,
        projection_age_ms: 0,
        expanded: false,
        scroll: 0,
        tab: 0,
    }
}

/// The text `module` draws for `model` in a `width` x `height` area, one
/// line a row.
pub(crate) fn render_text(
    module: &dyn crate::registry::DisplayModule,
    model: &ViewModel,
    (width, height): (u16, u16),
) -> String {
    let context = crate::registry::DisplayContext {
        viewport: ratatui::layout::Rect::new(0, 0, width, height),
        ..display_context()
    };
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            module.render(&context, model, frame.area(), frame);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The core's test process key, started at tick `start`.
pub(crate) fn process_key(pid: i64, start: u64) -> ProcessKey {
    ProcessKey {
        start_time_ticks: start,
        ..test_process_key(pid)
    }
}

pub(crate) fn token() -> ConfirmationToken {
    ConfirmationToken::new("ab".repeat(32)).unwrap()
}

pub(crate) fn ok(value: f64, unit: CanonicalUnit) -> MetricCell {
    MetricCell {
        value: Some(Scalar::from_f64(value).unwrap()),
        unit,
        state: CellState::Ok,
        kind: SampleKind::Measured,
        quality: Quality::Verified,
        age_ms: Some(400),
        source_label: None,
        conflict: None,
        last_good: None,
    }
}

pub(crate) fn exact(value: u64, unit: CanonicalUnit) -> MetricCell {
    MetricCell {
        value: Some(Scalar::from_u64(value)),
        ..ok(0.0, unit)
    }
}

pub(crate) fn sourced(
    value: f64,
    unit: CanonicalUnit,
    source: &str,
    quality: Quality,
) -> MetricCell {
    MetricCell {
        source_label: Some(source.to_owned()),
        quality,
        ..ok(value, unit)
    }
}

pub(crate) fn missing(unit: CanonicalUnit, state: CellState) -> MetricCell {
    MetricCell::missing(unit, state)
}

pub(crate) fn conflict(unit: CanonicalUnit, first: (&str, f64), second: (&str, f64)) -> MetricCell {
    let mut cell = MetricCell::missing(unit, CellState::Conflict);
    cell.conflict = Some(ConflictView {
        entries: vec![
            (first.0.to_owned(), Scalar::from_f64(first.1).unwrap()),
            (second.0.to_owned(), Scalar::from_f64(second.1).unwrap()),
        ],
    });
    cell
}

pub(crate) fn stale(unit: CanonicalUnit, last: f64, age_ms: u64) -> MetricCell {
    let mut cell = MetricCell::missing(unit, CellState::Stale);
    cell.last_good = Some(LastGoodView {
        value: Scalar::from_f64(last).unwrap(),
        age_ms,
    });
    cell
}

fn trend(points: Vec<Option<f64>>) -> Trend {
    Trend { points }
}

pub(crate) fn cpu_view() -> CpuView {
    let cores = (0..8)
        .map(|index| CoreView {
            core_id: index.to_string(),
            utilization: ok(f64::from(index) * 10.0 + 5.0, CanonicalUnit::Percent),
            frequency: ok(2400.0, CanonicalUnit::MHz),
            temperature: missing(CanonicalUnit::DegC, CellState::Unsupported),
        })
        .collect();
    CpuView {
        model: Some("Grace ARM Neoverse-V2".into()),
        total: ok(45.3, CanonicalUnit::Percent),
        user: ok(30.1, CanonicalUnit::Percent),
        system: ok(10.2, CanonicalUnit::Percent),
        iowait: ok(0.0, CanonicalUnit::Percent),
        steal: missing(CanonicalUnit::Percent, CellState::Unsupported),
        cores,
        frequencies: vec![FrequencyGroup {
            maximum: ok(3900.0, CanonicalUnit::MHz),
            current: ok(3000.0, CanonicalUnit::MHz),
            cores: 8,
        }],
        package_temperatures: vec![LabeledCell {
            label: "pkg0".into(),
            cell: sourced(
                68.0,
                CanonicalUnit::DegC,
                "package 0 · hwmon",
                Quality::Usable,
            ),
        }],
        soc_temperatures: vec![LabeledCell {
            label: "soc".into(),
            cell: conflict(CanonicalUnit::DegC, ("hwmon", 68.0), ("thermal", 42.0)),
        }],
        package_power: vec![LabeledCell {
            label: "package 0".into(),
            cell: MetricCell {
                kind: SampleKind::Estimated,
                ..ok(45.2, CanonicalUnit::Watts)
            },
        }],
        power: vec![LabeledCell {
            label: "platform".into(),
            cell: ok(61.5, CanonicalUnit::Watts),
        }],
        load: [
            ok(1.02, CanonicalUnit::Count),
            ok(0.98, CanonicalUnit::Count),
            ok(0.75, CanonicalUnit::Count),
        ],
        tasks_running: exact(3, CanonicalUnit::Count),
        tasks_total: exact(412, CanonicalUnit::Count),
        utilization_trend: trend(vec![Some(40.0), Some(42.0), None, Some(45.0)]),
    }
}

fn gpu_card(index: u32) -> GpuCard {
    let unified = index == 1;
    let framebuffer = |value: f64| {
        if unified {
            missing(CanonicalUnit::Bytes, CellState::Unsupported)
        } else {
            ok(value, CanonicalUnit::Bytes)
        }
    };
    GpuCard {
        entity_id: entity(&format!("gpu:GPU-{index}")),
        label: format!("GPU {index}"),
        name: "NVIDIA GB10".into(),
        uuid: Some(format!("GPU-{index}-uuid")),
        unified_memory: unified,
        utilization: ok(90.0 - f64::from(index) * 20.0, CanonicalUnit::Percent),
        memory_busy: ok(20.0, CanonicalUnit::Percent),
        temperature: sourced(71.0, CanonicalUnit::DegC, "nvml", Quality::Verified),
        power_draw: ok(120.5, CanonicalUnit::Watts),
        power_limit: ok(250.0, CanonicalUnit::Watts),
        clocks: vec![
            LabeledCell {
                label: "graphics".into(),
                cell: ok(1980.0, CanonicalUnit::MHz),
            },
            LabeledCell {
                label: "graphics max".into(),
                cell: ok(2100.0, CanonicalUnit::MHz),
            },
        ],
        fan: missing(CanonicalUnit::Percent, CellState::Unsupported),
        framebuffer_used: framebuffer(12.3 * GIB),
        framebuffer_free: framebuffer(67.7 * GIB),
        framebuffer_total: framebuffer(80.0 * GIB),
        framebuffer_reserved: framebuffer(0.5 * GIB),
        process_allocated: ok(10.0 * GIB, CanonicalUnit::Bytes),
        system_ram_total: if unified {
            ok(64.0 * GIB, CanonicalUnit::Bytes)
        } else {
            missing(CanonicalUnit::Bytes, CellState::Unsupported)
        },
        // The unified GPU's driver reports PCIe traffic in an unverified
        // unit: its details carry the driver's reading instead.
        pcie_tx: if unified {
            missing(CanonicalUnit::BytesPerSecond, CellState::Unavailable)
        } else {
            ok(1.25 * 1024.0 * 1024.0, CanonicalUnit::BytesPerSecond)
        },
        pcie_rx: if unified {
            missing(CanonicalUnit::BytesPerSecond, CellState::Unavailable)
        } else {
            missing(CanonicalUnit::BytesPerSecond, CellState::Warmup)
        },
        energy: ok(4_560_000.0, CanonicalUnit::Joules),
        ecc_corrected: exact(0, CanonicalUnit::Count),
        ecc_uncorrected: missing(CanonicalUnit::Count, CellState::PermissionDenied),
        retired_single: exact(0, CanonicalUnit::Count),
        retired_double: missing(CanonicalUnit::Count, CellState::ReadFailed),
        encoder: ok(0.0, CanonicalUnit::Percent),
        decoder: ok(0.0, CanonicalUnit::Percent),
        nvlink_active: exact(0, CanonicalUnit::Count),
        performance_state: exact(2, CanonicalUnit::Count),
        // Power cap: the clocks are held back by the power budget.
        throttle_reasons: exact(0x4, CanonicalUnit::Count),
        temperature_slowdown: ok(90.0, CanonicalUnit::DegC),
        temperature_shutdown: ok(93.0, CanonicalUnit::DegC),
        // Trained down at idle, in a slot narrower than the card.
        pcie_generation: exact(1, CanonicalUnit::Count),
        pcie_width: exact(4, CanonicalUnit::Count),
        pcie_generation_max: exact(4, CanonicalUnit::Count),
        pcie_width_max: exact(16, CanonicalUnit::Count),
        details: Some(Box::new(gpu_details(unified))),
        removed: false,
        metadata: vec![
            ("uuid".into(), format!("GPU-{index}-uuid")),
            ("pci_bus_id".into(), format!("00000000:0{index}:00.0")),
            ("driver_version".into(), "580.65".into()),
            ("serial".into(), "1320000000001".into()),
            ("compute_mode".into(), "default".into()),
            ("persistence_mode".into(), "enabled".into()),
        ],
        utilization_trend: trend(vec![Some(80.0), None, Some(90.0)]),
        temperature_trend: trend(vec![Some(70.0), Some(71.0)]),
        power_trend: trend(vec![Some(120.0), Some(121.0)]),
        memory_trend: trend(vec![Some(11.0 * GIB), Some(12.3 * GIB)]),
    }
}

/// What the GPU view shows beyond the card: a Blackwell GeForce with its
/// toolkit, or the unified GB10, whose bus and BAR1 the driver does not
/// report.
fn gpu_details(unified: bool) -> GpuDetails {
    let exact_count = |value: u64| exact(value, CanonicalUnit::Count);
    let unsupported = |unit| missing(unit, CellState::Unsupported);
    let derived = |value: f64| MetricCell {
        kind: SampleKind::Derived,
        ..ok(value, CanonicalUnit::BytesPerSecond)
    };
    let attributes: Vec<(String, String)> = [
        ("architecture", "Blackwell"),
        ("brand", "GeForce"),
        ("compute_capability", "12.0"),
        ("vbios_version", "98.03.3B.00.01"),
        ("inforom_image", "G005.0000.98.01"),
        ("gsp_firmware", "595.104.02"),
        ("board_part_number", "900-1G144-2545-000"),
        ("minor_number", "0"),
        ("pci_device_id", "10de:2c02"),
        ("pci_subsystem_id", "10de:2095"),
        ("nvml_version", "13.595.104.02"),
        ("cuda_driver_version", "13.2"),
        ("kernel_module", "open · gcc 14.2.0"),
        ("cuda_toolkit", "13.4"),
        ("cuda_toolkit_path", "/usr/local/cuda-13.4"),
        ("nvcc_version", "13.4.92"),
        ("cuda_toolkits", "13.4, 12.8"),
        ("numa_nodes", "0"),
        ("cpu_affinity", "0-31"),
        ("gpu_peers", "1:PHB:none"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    GpuDetails {
        pcie_bandwidth: derived(1e9),
        pcie_bandwidth_max: derived(31.5e9),
        pcie_tx_driver_kb: unified.then_some(2243.0),
        pcie_rx_driver_kb: unified.then_some(1767.0),
        pcie_replays: exact_count(0),
        cores: exact_count(if unified { 6144 } else { 10_752 }),
        memory_bus_width: if unified {
            unsupported(CanonicalUnit::Count)
        } else {
            exact_count(256)
        },
        memory_bandwidth_max: if unified {
            unsupported(CanonicalUnit::BytesPerSecond)
        } else {
            derived(960.064e9)
        },
        bar1_used: ok(6.0 * 1024.0 * 1024.0, CanonicalUnit::Bytes),
        bar1_total: ok(16.0 * GIB, CanonicalUnit::Bytes),
        power_limit_default: ok(250.0, CanonicalUnit::Watts),
        power_limit_minimum: ok(150.0, CanonicalUnit::Watts),
        power_limit_maximum: ok(300.0, CanonicalUnit::Watts),
        power_limit_requested: ok(250.0, CanonicalUnit::Watts),
        power_instant: ok(123.0, CanonicalUnit::Watts),
        // Like the card's fan: none reported.
        fans: Vec::new(),
        temperature_max_operating: ok(88.0, CanonicalUnit::DegC),
        temperature_target: unsupported(CanonicalUnit::DegC),
        encoder_sessions: exact_count(0),
        encoder_fps: ok(0.0, CanonicalUnit::CountPerSecond),
        encoder_latency: ok(0.0, CanonicalUnit::Milliseconds),
        throttle_time: [
            ("power", ok(219.152, CanonicalUnit::Milliseconds)),
            ("thermal", ok(0.0, CanonicalUnit::Milliseconds)),
            ("sync boost", ok(0.0, CanonicalUnit::Milliseconds)),
            ("board limit", ok(0.0, CanonicalUnit::Milliseconds)),
            ("reliability", unsupported(CanonicalUnit::Milliseconds)),
        ]
        .into_iter()
        .map(|(label, cell)| LabeledCell {
            label: label.to_owned(),
            cell,
        })
        .collect(),
        attributes,
    }
}

pub(crate) fn gpu_view() -> GpuView {
    GpuView {
        cards: vec![gpu_card(0), gpu_card(1)],
        selected: None,
        unavailable_reason: None,
    }
}

/// Whole nanoseconds of `seconds` (a fixture's covered time).
fn seconds_to_ns(seconds: f64) -> u64 {
    format!("{:.0}", seconds * 1e9).parse().unwrap_or(0)
}

/// Each GPU over the last 1 h to 24 h: busier the shorter the window, with
/// the history covering half of the last day.
pub(crate) fn gpu_window_stats() -> Vec<HistoryWindowSummary> {
    let mut stats = Vec::new();
    for subject in ["GPU 0 · NVIDIA GB10", "GPU 1 · NVIDIA GB10"] {
        for (kind, unit) in [
            (HistoryChartKind::GpuUtilization, CanonicalUnit::Percent),
            (HistoryChartKind::GpuMemory, CanonicalUnit::Bytes),
            (HistoryChartKind::GpuTemperature, CanonicalUnit::DegC),
            (HistoryChartKind::GpuPower, CanonicalUnit::Watts),
        ] {
            for (window, hours, busy, coverage) in [
                ("1h", 1.0, 1.0, 1.0),
                ("6h", 6.0, 0.8, 1.0),
                ("12h", 12.0, 0.6, 1.0),
                ("24h", 24.0, 0.5, 0.5),
            ] {
                let (mean, peak) = match kind {
                    HistoryChartKind::GpuUtilization => (12.0 * busy, 90.0),
                    HistoryChartKind::GpuMemory => (2.0 * GIB * busy, 3.0 * GIB),
                    HistoryChartKind::GpuTemperature => (45.0, 70.0),
                    _ => (80.0 * busy, 250.0),
                };
                stats.push(HistoryWindowSummary {
                    kind,
                    subject: subject.to_owned(),
                    window_label: window.to_owned(),
                    span_ns: seconds_to_ns(hours * 3600.0),
                    unit,
                    mean: Some(mean),
                    peak: Some(peak),
                    covered_ns: Some(seconds_to_ns(hours * 3600.0 * coverage)),
                    integral: (kind == HistoryChartKind::GpuPower)
                        .then_some(mean * hours * 3600.0 * coverage),
                    pending: false,
                });
            }
        }
    }
    stats
}

pub(crate) fn memory_view() -> MemoryView {
    MemoryView {
        total: ok(64.0 * GIB, CanonicalUnit::Bytes),
        available: ok(51.7 * GIB, CanonicalUnit::Bytes),
        used: ok(12.3 * GIB, CanonicalUnit::Bytes),
        buffers: ok(1.2 * GIB, CanonicalUnit::Bytes),
        cached: ok(20.1 * GIB, CanonicalUnit::Bytes),
        swap_total: ok(8.0 * GIB, CanonicalUnit::Bytes),
        swap_used: ok(0.0, CanonicalUnit::Bytes),
        gpu_resident: Some(ok(10.0 * GIB, CanonicalUnit::Bytes)),
        used_trend: trend(vec![Some(12.0 * GIB), Some(12.3 * GIB)]),
        gpu_resident_trend: trend(vec![Some(9.5 * GIB), Some(10.0 * GIB)]),
        gpu_processes: Vec::new(),
        gpu_process_trends: Vec::new(),
    }
}

pub(crate) fn disk_view() -> DiskView {
    DiskView {
        devices: vec![DiskDeviceView {
            entity_id: entity("disk:nvme0n1"),
            name: "nvme0n1".into(),
            read_bps: ok(1.25 * 1024.0 * 1024.0, CanonicalUnit::BytesPerSecond),
            write_bps: ok(0.0, CanonicalUnit::BytesPerSecond),
            read_iops: ok(120.0, CanonicalUnit::CountPerSecond),
            write_iops: missing(CanonicalUnit::CountPerSecond, CellState::Warmup),
            await_ms: ok(0.4, CanonicalUnit::Milliseconds),
            temperature: ok(58.85, CanonicalUnit::DegC),
        }],
        filesystems: vec![FilesystemView {
            entity_id: entity("fs:/"),
            mount_point: "/".into(),
            device: "/dev/nvme0n1p2".into(),
            block_device: Some(entity("disk:nvme0n1")),
            fs_type: "ext4".into(),
            total: ok(1800.0 * GIB, CanonicalUnit::Bytes),
            used: ok(812.0 * GIB, CanonicalUnit::Bytes),
            free: ok(988.0 * GIB, CanonicalUnit::Bytes),
            available: ok(900.0 * GIB, CanonicalUnit::Bytes),
            use_percent: ok(45.1, CanonicalUnit::Percent),
        }],
        read_trend: trend(vec![Some(0.0), Some(1.25 * 1024.0 * 1024.0)]),
        write_trend: trend(vec![Some(0.0), Some(0.0)]),
    }
}

pub(crate) fn network_view() -> NetworkView {
    let interface = |name: &str, state: Option<&str>| InterfaceView {
        entity_id: entity(&format!("net:{name}")),
        name: name.into(),
        state: state.map(str::to_owned),
        rx_bps: ok(2.5 * 1024.0 * 1024.0, CanonicalUnit::BytesPerSecond),
        tx_bps: ok(512.0, CanonicalUnit::BytesPerSecond),
        rx_packets: exact(123_456, CanonicalUnit::Count),
        tx_packets: exact(654_321, CanonicalUnit::Count),
        rx_errors: exact(0, CanonicalUnit::Count),
        tx_errors: exact(0, CanonicalUnit::Count),
        rx_drops: missing(CanonicalUnit::Count, CellState::Unavailable),
        tx_drops: exact(2, CanonicalUnit::Count),
    };
    NetworkView {
        interfaces: vec![interface("enp1s0", Some("up")), interface("wlan0", None)],
        max_visible: 3,
        rx_trend: trend(vec![Some(1024.0), Some(2.5 * 1024.0 * 1024.0)]),
        tx_trend: trend(vec![Some(256.0), Some(512.0)]),
    }
}

/// `count` projected rows starting at absolute index `window_start`.
pub(crate) fn processes_view(count: usize, window_start: u32, total_rows: u32) -> ProcessesView {
    let rows = (0..count)
        .map(|index| {
            // Keys follow the absolute position so windows over the same
            // list share identities, like the real projection.
            let absolute = u64::from(window_start) + u64::try_from(index).unwrap();
            let pid = 4242 + i64::try_from(absolute).unwrap();
            let gpus = if index == 0 {
                vec![
                    ProcessGpuView {
                        gpu_entity_id: entity("gpu:GPU-0"),
                        gpu_label: "GPU 0".into(),
                        usage: GpuUsageKind::Compute,
                        utilization_percent: Some(45.0),
                        memory_bytes: Some(10 * 1024 * 1024 * 1024),
                    },
                    ProcessGpuView {
                        gpu_entity_id: entity("gpu:GPU-1"),
                        gpu_label: "GPU 1".into(),
                        usage: GpuUsageKind::ComputeAndGraphics,
                        utilization_percent: None,
                        memory_bytes: Some(2 * 1024 * 1024 * 1024),
                    },
                ]
            } else {
                vec![ProcessGpuView {
                    gpu_entity_id: entity("gpu:GPU-0"),
                    gpu_label: "GPU 0".into(),
                    usage: GpuUsageKind::Graphics,
                    utilization_percent: Some(5.0),
                    memory_bytes: Some(512 * 1024 * 1024),
                }]
            };
            ProcessRowView {
                key: process_key(pid, 90_100 + absolute),
                name: if index == 0 {
                    "python3".into()
                } else {
                    format!("worker-{index}")
                },
                user: "1000".into(),
                state: Some('R'),
                cpu_percent: if index == 1 { None } else { Some(12.5) },
                rss_bytes: Some(1024 * 1024 * 1024),
                missing_reason: if index == 1 {
                    Some("cpu: permission denied".into())
                } else {
                    None
                },
                gpus,
                color_slot: None,
            }
        })
        .collect::<Vec<_>>();
    let selected = rows.first().map(|row| row.key.clone());
    ProcessesView {
        selected_missing: None,
        rows,
        window_start,
        total_rows,
        truncated: false,
        sort: SortKey::GpuMemory,
        ascending: false,
        filter: String::new(),
        gpu_filter: None,
        gpu_choices: vec![
            (entity("gpu:GPU-0"), "GPU0".into()),
            (entity("gpu:GPU-1"), "GPU1".into()),
        ],
        selected,
        selected_index: Some(window_start),
    }
}

/// One hour of minute buckets ending at `HISTORY_END`: the CPU has a gap
/// from minute 40 to 44 and a spike to 95 % inside minute 30, whose mean
/// stays at 20 %.
pub(crate) fn history_view() -> HistoryView {
    const MINUTE: u64 = 60 * 1_000_000_000;
    let start = HISTORY_END - 60 * MINUTE;
    let minutes = |value: &dyn Fn(u64) -> Option<(f64, f64)>| -> Vec<HistoryPointView> {
        (0..60)
            .map(|minute| {
                let sample = value(minute);
                HistoryPointView {
                    start_utc_ns: start + minute * MINUTE,
                    end_utc_ns: start + (minute + 1) * MINUTE,
                    mean: sample.map(|(mean, _)| mean),
                    min: None,
                    max: sample.map(|(_, max)| max),
                }
            })
            .collect()
    };
    let chart =
        |kind, subject: &str, key, unit, scale_max, points, (mean, peak)| HistoryChartView {
            kind,
            subject: subject.into(),
            series: key,
            unit,
            scale_max,
            points,
            mean: Some(mean),
            peak: Some(peak),
            covered_ns: Some(60 * MINUTE),
            display_downsampled: true,
            pending: false,
            error: None,
            parts: Vec::new(),
        };
    let mut cpu = chart(
        HistoryChartKind::CpuUtilization,
        "",
        metric_key("cpu", "cpu.utilization.total", "host"),
        CanonicalUnit::Percent,
        Some(100.0),
        minutes(&|minute| match minute {
            40..45 => None,
            30 => Some((20.0, 95.0)),
            _ => Some((20.0, 25.0)),
        }),
        (20.0, 95.0),
    );
    cpu.covered_ns = Some(55 * MINUTE);
    let rate = |minute: u64| Some((f64::from(u32::try_from(minute).unwrap()) * 1024.0, 65_536.0));
    let half_rate =
        |minute: u64| Some((f64::from(u32::try_from(minute).unwrap()) * 512.0, 16_384.0));
    // GPU 0 runs a job from minute 50: busy, its memory, heat and power up.
    let job = |idle: f64, busy: f64, peak: f64| {
        move |minute: u64| {
            Some(if minute >= 50 {
                (busy, peak)
            } else {
                (idle, idle)
            })
        }
    };
    let gpu = |kind, index: u64, metric: &str, unit, scale, points, stats| {
        chart(
            kind,
            &format!("GPU {index} · NVIDIA GB10"),
            metric_key(&format!("gpu:GPU-{index}"), metric, "gpu"),
            unit,
            scale,
            points,
            stats,
        )
    };
    HistoryView {
        window_label: "1h".into(),
        requested_span_ns: 60 * MINUTE,
        start_utc_ns: HISTORY_END - 60 * MINUTE,
        end_utc_ns: HISTORY_END,
        utc_offset_s: 0,
        charts: vec![
            cpu,
            chart(
                HistoryChartKind::MemoryUsed,
                "",
                metric_key("memory", "memory.used", "system"),
                CanonicalUnit::Bytes,
                Some(64.0 * GIB),
                minutes(&|_| Some((12.0 * GIB, 12.5 * GIB))),
                (12.0 * GIB, 12.5 * GIB),
            ),
            gpu(
                HistoryChartKind::GpuUtilization,
                0,
                "gpu.utilization.compute",
                CanonicalUnit::Percent,
                Some(100.0),
                minutes(&|minute| Some((if minute >= 50 { 90.0 } else { 5.0 }, 98.0))),
                (19.2, 98.0),
            ),
            gpu(
                HistoryChartKind::GpuMemory,
                0,
                "gpu.memory.framebuffer.used",
                CanonicalUnit::Bytes,
                Some(80.0 * GIB),
                minutes(&job(2.0 * GIB, 40.0 * GIB, 41.0 * GIB)),
                (8.3 * GIB, 41.0 * GIB),
            ),
            gpu(
                HistoryChartKind::GpuTemperature,
                0,
                "gpu.temperature",
                CanonicalUnit::DegC,
                Some(100.0),
                minutes(&job(40.0, 75.0, 78.0)),
                (45.8, 78.0),
            ),
            gpu(
                HistoryChartKind::GpuPower,
                0,
                "gpu.power.draw",
                CanonicalUnit::Watts,
                Some(250.0),
                minutes(&job(60.0, 220.0, 240.0)),
                (86.7, 240.0),
            ),
            gpu(
                HistoryChartKind::GpuUtilization,
                1,
                "gpu.utilization.compute",
                CanonicalUnit::Percent,
                Some(100.0),
                minutes(&|_| Some((30.0, 60.0))),
                (30.0, 60.0),
            ),
            gpu(
                HistoryChartKind::GpuMemory,
                1,
                "gpu.memory.process_allocated",
                CanonicalUnit::Bytes,
                Some(64.0 * GIB),
                minutes(&|_| Some((10.0 * GIB, 10.0 * GIB))),
                (10.0 * GIB, 10.0 * GIB),
            ),
            gpu(
                HistoryChartKind::GpuTemperature,
                1,
                "gpu.temperature",
                CanonicalUnit::DegC,
                Some(100.0),
                minutes(&|_| Some((45.0, 47.0))),
                (45.0, 47.0),
            ),
            gpu(
                HistoryChartKind::GpuPower,
                1,
                "gpu.power.draw",
                CanonicalUnit::Watts,
                Some(250.0),
                minutes(&|_| Some((80.0, 95.0))),
                (80.0, 95.0),
            ),
            chart(
                HistoryChartKind::DiskRead,
                "nvme0n1",
                metric_key("blockdev:259:0", "disk.read.bytes_per_second", "device"),
                CanonicalUnit::BytesPerSecond,
                None,
                minutes(&rate),
                (30_208.0, 65_536.0),
            ),
            chart(
                HistoryChartKind::DiskWrite,
                "nvme0n1",
                metric_key("blockdev:259:0", "disk.write.bytes_per_second", "device"),
                CanonicalUnit::BytesPerSecond,
                None,
                minutes(&half_rate),
                (15_104.0, 16_384.0),
            ),
            chart(
                HistoryChartKind::NetworkReceive,
                "enp1s0",
                metric_key("netif:ns:2", "network.rx.bytes_per_second", "interface"),
                CanonicalUnit::BytesPerSecond,
                None,
                minutes(&rate),
                (30_208.0, 65_536.0),
            ),
            chart(
                HistoryChartKind::NetworkTransmit,
                "enp1s0",
                metric_key("netif:ns:2", "network.tx.bytes_per_second", "interface"),
                CanonicalUnit::BytesPerSecond,
                None,
                minutes(&half_rate),
                (15_104.0, 16_384.0),
            ),
        ],
        summaries: gpu_window_stats(),
        disk: DiskHistory::On,
        error: None,
    }
}

/// End of the fixture's history window.
pub(crate) const HISTORY_END: u64 = 10_000 * 1_000_000_000;

/// Two adapter hosts: nvml backing off, procfs healthy.
pub(crate) fn host_statuses() -> Vec<HostStatus> {
    vec![
        HostStatus {
            adapter_id: "nvml".into(),
            state: HostState::Quarantined,
            generation: 4,
            pid: None,
            last_success_boot_ns: None,
            timeouts_total: 3,
            consecutive_failures: 3,
            restarts_total: 2,
            // 7 s after the projection was generated.
            backoff_until_boot_ns: Some(12_000_000_000),
            detail: Some("child not reaped".into()),
        },
        HostStatus {
            adapter_id: "procfs".into(),
            state: HostState::Healthy,
            generation: 1,
            pid: Some(5000),
            // 3 s before the projection was generated.
            last_success_boot_ns: Some(2_000_000_000),
            timeouts_total: 0,
            consecutive_failures: 0,
            restarts_total: 0,
            backoff_until_boot_ns: None,
            detail: None,
        },
    ]
}

pub(crate) fn settings_view() -> SettingsView {
    SettingsView {
        revision: 7,
        pending: Some("SetSamplingInterval 500 ms".into()),
        last_result: Some("interval applied: configured 1 s, effective 1 s".into()),
        fields: vec![
            SettingField {
                path: "sampling.interval_ms".into(),
                saved: "1000".into(),
                effective: "2000".into(),
                origin: ValueOrigin::Saved,
                restart_required: false,
                overload_note: Some("overload multiplier x2".into()),
            },
            SettingField {
                path: "store.memory_limit_mib".into(),
                saved: "256".into(),
                effective: "256".into(),
                origin: ValueOrigin::Default,
                restart_required: true,
                overload_note: None,
            },
            SettingField {
                path: "viewer.theme".into(),
                saved: "dark".into(),
                effective: "amber".into(),
                origin: ValueOrigin::CliOverride,
                restart_required: false,
                overload_note: None,
            },
        ],
        modules: dgxtop_core::config::BUILTIN_MODULES
            .iter()
            .map(|id| (ModuleId::new(*id).unwrap(), *id != "history"))
            .collect(),
        config_status: PersistenceStatus::healthy(PersistenceKind::Config),
        hosts: host_statuses(),
    }
}

pub(crate) fn actions_view(with_confirmation: bool) -> ActionsView {
    ActionsView {
        confirmation: with_confirmation.then(|| ConfirmationView {
            request_id: TEST_REQUEST_ID.try_into().unwrap(),
            target: process_key(4242, 90_100),
            uid: 1000,
            basename: "python3".into(),
            signal: Signal::Sigterm,
            expires_in_ms: 9_000,
            warning: "SIGTERM asks the process to exit; unsaved work may be lost".into(),
        }),
        audit: PersistenceStatus::healthy(PersistenceKind::Audit),
        unknown_outcomes: 1,
    }
}

/// Request id used by fixtures. The `uuid` crate is not a dependency of
/// this crate, so values are built through core-visible trait conversions.
pub(crate) const TEST_REQUEST_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";

pub(crate) fn header() -> HeaderView {
    HeaderView {
        host_name: "spark-01".into(),
        kernel: Some("6.12.0".into()),
        uptime_s: Some(3600),
        version: "0.2.0".into(),
        namespace_label: None,
        overload: OverloadState::Normal,
        configured_interval_ms: 1000,
        effective_interval_ms: 1000,
        hosts_healthy: 6,
        hosts_total: 7,
        read_only: false,
        theme: Theme::Dark,
        total_power: missing(CanonicalUnit::Watts, CellState::Unavailable),
    }
}

pub(crate) fn full_modules(with_confirmation: bool) -> ModuleViews {
    ModuleViews {
        cpu: Some(Arc::new(cpu_view())),
        gpu: Some(Arc::new(gpu_view())),
        memory: Some(Arc::new(memory_view())),
        disk: Some(Arc::new(disk_view())),
        network: Some(Arc::new(network_view())),
        processes: Some(Arc::new(processes_view(12, 0, 12))),
        history: Some(Arc::new(history_view())),
        process_history: Some(Arc::new(process_history_view())),
        settings: Some(Arc::new(settings_view())),
        actions: Some(Arc::new(actions_view(with_confirmation))),
    }
}

pub(crate) fn view_model(view: ViewKind, frame_revision: u64) -> ViewModel {
    ViewModel {
        frame_revision,
        snapshot_revision: frame_revision,
        generated_boot_ns: 5_000_000_000,
        header: header(),
        session: SessionState {
            session_id: SessionId::new("tui").unwrap(),
            view,
            selected_entity_id: None,
            selected_process: Some(process_key(4242, 90_100)),
            filter: String::new(),
            sort: SortKey::GpuMemory,
            ascending: false,
            overview_bar: dgxtop_core::tiers::BarSize::OneMinute,
            process_bar: dgxtop_core::tiers::BarSize::Auto,
        },
        modules: full_modules(false),
        banners: Vec::new(),
        enabled_modules: Vec::new(),
        permit: None,
    }
}

/// 2026-10-04T00:00:00Z, the start of the process history fixture's runs.
pub(crate) const RUNS_UTC: i64 = 1_791_072_000_000_000_000;

/// Two runs, the newest first: `ComfyUI/main` (pid 42), killed by dgxtop
/// after 30 min and opened with its charts, and `acme_llm.train` (pid 41),
/// running for 2 h in color 0.
pub(crate) fn process_history_view() -> dgxtop_core::view::ProcessHistoryView {
    use dgxtop_core::action::Signal;
    use dgxtop_core::process_history::{EndCause, RunDetails, RunEnd, RunInfo, RunMetric};
    use dgxtop_core::tiers::BarSize;
    use dgxtop_core::view::{
        HistoryPointView, ProcessHistoryView, RunChartView, RunDetailView, RunRowView,
    };
    const MINUTE: i64 = 60 * 1_000_000_000;
    let ended = RunEnd {
        utc_ns: RUNS_UTC + 90 * MINUTE,
        cause: EndCause::SignaledByDgxtop(Signal::Sigkill),
    };
    let killed = RunRowView {
        key: process_key(42, 9_000),
        name: "ComfyUI/main".to_owned(),
        started_utc_ns: RUNS_UTC + 60 * MINUTE,
        runtime_ns: 30 * 60 * 1_000_000_000,
        live: false,
        end: Some(ended),
        gpus: vec!["GPU 0".to_owned()],
        peak_gpu_memory: Some(9.0 * GIB),
        mean_gpu_utilization: Some(35.0),
        mean_cpu: Some(110.0),
        peak_memory: Some(6.5 * GIB),
        peak_children: Some(3.0),
        slot: None,
    };
    let training = RunRowView {
        key: process_key(41, 100),
        name: "acme_llm.train".to_owned(),
        started_utc_ns: RUNS_UTC,
        runtime_ns: 2 * 3600 * 1_000_000_000,
        live: true,
        end: None,
        gpus: vec!["GPU 0".to_owned(), "GPU 1".to_owned()],
        peak_gpu_memory: Some(61.8 * GIB),
        mean_gpu_utilization: Some(97.0),
        mean_cpu: Some(100.0),
        peak_memory: Some(12.0 * GIB),
        peak_children: None,
        slot: Some(0),
    };
    // A bar a minute over the 30 minutes it ran, in an 80-minute window.
    let point = |minute: i64, value: f64| HistoryPointView {
        start_utc_ns: u64::try_from(killed.started_utc_ns + minute * MINUTE).unwrap(),
        end_utc_ns: u64::try_from(killed.started_utc_ns + (minute + 1) * MINUTE).unwrap(),
        mean: Some(value),
        min: None,
        max: Some(value),
    };
    let charts = RunMetric::ALL
        .iter()
        .map(|metric| {
            let (unit, scale_max) = match metric {
                RunMetric::Cpu | RunMetric::ChildrenCpu => (CanonicalUnit::Percent, Some(2_000.0)),
                RunMetric::GpuUtilization => (CanonicalUnit::Percent, Some(100.0)),
                RunMetric::Memory | RunMetric::ChildrenMemory | RunMetric::Swap => {
                    (CanonicalUnit::Bytes, Some(128.0 * GIB))
                }
                RunMetric::GpuMemory => (CanonicalUnit::Bytes, Some(120.0 * GIB)),
                RunMetric::Threads | RunMetric::Children | RunMetric::OpenFiles => {
                    (CanonicalUnit::Count, None)
                }
                _ => (CanonicalUnit::BytesPerSecond, None),
            };
            let top = scale_max.unwrap_or(29.0);
            RunChartView {
                metric: *metric,
                unit,
                scale_max,
                points: (0..30)
                    .map(|minute| {
                        point(
                            minute,
                            top * f64::from(u8::try_from(minute).unwrap()) / 29.0,
                        )
                    })
                    .collect(),
                mean: Some(top / 2.0),
                peak: Some(top),
                latest: Some(top),
            }
        })
        .collect();
    let opened = RunDetailView {
        info: RunInfo {
            key: killed.key.clone(),
            name: killed.name.clone(),
            command: "/venv/bin/python main.py --listen 0.0.0.0 --port 8188 --output-directory /srv/comfy/out --preview-method auto"
                .to_owned(),
            cwd: "/home/u/code/ComfyUI".to_owned(),
            uid: Some(1000),
            gpus: killed.gpus.clone(),
            started_utc_ns: killed.started_utc_ns,
            last_seen_utc_ns: ended.utc_ns,
            end: Some(ended),
            details: RunDetails {
                parent_pid: Some(7),
                parent_name: "bash".to_owned(),
                executable: "/venv/bin/python3.12".to_owned(),
                cgroup: "/user.slice/user-1000.slice/session-3.scope".to_owned(),
                nice: Some(0),
            },
        },
        live: false,
        slot: None,
        bar: BarSize::OneMinute,
        window_start_utc_ns: u64::try_from(ended.utc_ns - 80 * MINUTE).unwrap(),
        window_end_utc_ns: u64::try_from(ended.utc_ns).unwrap(),
        charts,
        loading: false,
    };
    ProcessHistoryView {
        runs: vec![killed, training],
        more_runs: 0,
        opened: Some(opened),
        kept_on_disk: true,
        utc_offset_s: 8 * 3600,
        bar: BarSize::Auto,
        cpu_scale: Some(2_000.0),
    }
}
