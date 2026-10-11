//! Projection-level tests: visible topics per view, module enablement,
//! safety banners and the projection byte bounds.

use std::sync::Arc;

use dgxtop_core::action::Signal;
use dgxtop_core::commands::{SortKey, ViewKind};
use dgxtop_core::config::BUILTIN_MODULES;
use dgxtop_core::naming::attributes;
use dgxtop_core::observation::EntityKind;
use dgxtop_core::testing::metric_key;
use dgxtop_core::view::{BannerLevel, ConfirmationView, HistoryPointView, HistoryView, ViewModel};

use super::testkit::{
    Fixture, NOW, candidate, entity_record, explanation_for, gpu_link, history_chart, process_key,
    process_row, sample,
};
use super::{PROCESS_OVERSCAN_ROWS, build_view_model_measured, visible_modules};
use dgxtop_core::canonical::CandidateDisposition;

const ONE_MIB: usize = 1024 * 1024;
const GPU_METRICS: [&str; 25] = [
    "gpu.utilization.compute",
    "gpu.utilization.memory_busy",
    "gpu.temperature",
    "gpu.power.draw",
    "gpu.power.limit",
    "gpu.clock.graphics",
    "gpu.clock.memory",
    "gpu.clock.sm",
    "gpu.clock.video",
    "gpu.fan.speed",
    "gpu.memory.framebuffer.used",
    "gpu.memory.framebuffer.free",
    "gpu.memory.framebuffer.total",
    "gpu.memory.framebuffer.reserved",
    "gpu.memory.process_allocated",
    "gpu.pcie.tx",
    "gpu.pcie.rx",
    "gpu.energy.total",
    "gpu.ecc.corrected",
    "gpu.ecc.uncorrected",
    "gpu.retired_pages.single",
    "gpu.retired_pages.double",
    "gpu.encoder.utilization",
    "gpu.decoder.utilization",
    "gpu.nvlink.active",
];

fn gpu_entity(index: u32) -> String {
    format!("gpu:GPU-{index:08x}-0000-0000-0000-000000000000")
}

/// Reference-load fixture: 512 non-process series, 8 GPUs, `processes`
/// processes linked to two GPUs each, and full trend books.
fn reference_fixture(cores: u32, processes: i64) -> Fixture {
    let mut fixture = Fixture::new();
    fixture.add(
        "cpu",
        sample("cpu.utilization.total", "cpu", "host").f64(41.0),
    );
    let total_key = metric_key("cpu", "cpu.utilization.total", "host");
    for core in 0..cores {
        let entity = format!("cpu.core.{core}");
        let scope = format!("core:{core}");
        fixture.add(
            "cpu",
            sample("cpu.utilization.total", &entity, &scope).f64(12.5),
        );
        fixture.add(
            "cpu",
            sample("cpu.frequency.current", &entity, &scope).f64(2400.0),
        );
    }
    for gpu in 0..8_u32 {
        let entity = gpu_entity(gpu);
        fixture.add_entity(entity_record(
            &entity,
            EntityKind::Gpu,
            "NVIDIA H100 80GB HBM3",
            &[
                (attributes::NVML_INDEX, &gpu.to_string()),
                (attributes::PCI_BUS_ID, "00000000:1B:00.0"),
                (attributes::DRIVER_VERSION, "550.54.15"),
            ],
        ));
        for metric in GPU_METRICS {
            fixture.add(
                "gpu",
                sample(metric, &entity, "gpu")
                    .f64(55.5)
                    .source("nvml:dev:x"),
            );
        }
        let temperature = metric_key(&entity, "gpu.temperature", "gpu");
        fixture.explain(
            "gpu",
            explanation_for(
                &temperature,
                vec![
                    candidate("nvml:dev:temp", "nvml")
                        .value(55.5)
                        .disposition(CandidateDisposition::Selected)
                        .build(),
                    candidate("hwmon:gpu:temp1", "hwmon").value(55.0).build(),
                ],
            ),
        );
        for metric in [
            "gpu.utilization.compute",
            "gpu.temperature",
            "gpu.power.draw",
        ] {
            let key = metric_key(&entity, metric, "gpu");
            for point in 0..120_u32 {
                fixture.trends.push(&key, Some(f64::from(point) * 1.25));
            }
        }
    }
    for metric in [
        "memory.total",
        "memory.available",
        "memory.used",
        "memory.buffers",
        "memory.cached",
        "memory.swap_total",
        "memory.swap_used",
    ] {
        fixture.add("memory", sample(metric, "memory", "system").u64(64 << 30));
    }
    for device in 0..4_u32 {
        let entity = format!("blockdev:259:{device}");
        for metric in [
            "disk.read.bytes_per_second",
            "disk.write.bytes_per_second",
            "disk.read.iops",
            "disk.write.iops",
            "disk.await",
        ] {
            fixture.add("disk", sample(metric, &entity, "device").f64(1.0e6));
        }
    }
    for interface in 0..4_u32 {
        let entity = format!("netif:ns:{}", interface + 2);
        for metric in [
            "network.rx.bytes_per_second",
            "network.tx.bytes_per_second",
            "network.rx.packets",
            "network.tx.packets",
            "network.rx.errors",
            "network.tx.errors",
            "network.rx.drops",
        ] {
            fixture.add("network", sample(metric, &entity, "interface").u64(10));
        }
    }
    for point in 0..120_u32 {
        fixture.trends.push(&total_key, Some(f64::from(point)));
        fixture
            .trends
            .push(&metric_key("memory", "memory.used", "system"), Some(1.0));
    }
    let rows = (1..=processes)
        .map(|pid| {
            let mut row = process_row(pid, &format!("python-worker-{pid}"));
            row.cpu_percent = Some(12.0);
            row.rss_bytes = Some(1 << 30);
            row.gpu = vec![
                gpu_link(&gpu_entity(0), Some(40.0), Some(8 << 30)),
                gpu_link(&gpu_entity(1), Some(60.0), Some(8 << 30)),
            ];
            row
        })
        .collect();
    fixture.set_processes(rows);
    fixture
}

fn series_count(fixture: &Fixture) -> usize {
    fixture
        .snapshot
        .domains
        .values()
        .map(|domain| domain.samples.len())
        .sum()
}

fn filled(model: &ViewModel) -> Vec<&'static str> {
    let modules = &model.modules;
    [
        ("cpu", modules.cpu.is_some()),
        ("gpu", modules.gpu.is_some()),
        ("memory", modules.memory.is_some()),
        ("disk", modules.disk.is_some()),
        ("network", modules.network.is_some()),
        ("processes", modules.processes.is_some()),
        ("history", modules.history.is_some()),
        ("settings", modules.settings.is_some()),
        ("actions", modules.actions.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect()
}

fn confirmation() -> ConfirmationView {
    ConfirmationView {
        request_id: uuid::Uuid::nil(),
        target: process_key(4242, 1000),
        uid: 1000,
        basename: "python3".to_owned(),
        signal: Signal::Sigterm,
        expires_in_ms: 10_000,
        warning: "SIGTERM cannot be undone".to_owned(),
    }
}

/// D-87: the header carries the effective redline, so a configuration
/// applied at runtime recolors the next frame's gauges.
#[test]
fn the_header_follows_the_effective_redline() {
    let mut fixture = Fixture::new();
    let model = build_view_model_measured(&fixture.context()).0;
    assert_eq!(
        model.header.redline_threshold_percent,
        fixture.effective.viewer.redline_threshold_percent
    );
    fixture.effective.viewer.redline_threshold_percent = 92.5;
    let applied = build_view_model_measured(&fixture.context()).0;
    assert_eq!(applied.header.redline_threshold_percent, 92.5);
}

#[test]
fn each_view_fills_only_its_visible_topics() {
    let fixture_base = Fixture::new();
    for view in ViewKind::ALL {
        let mut fixture = Fixture::new();
        fixture.session.view = view;
        let model = build_view_model_measured(&fixture.context()).0;
        // The process actions' state rides on every view (D-91).
        let mut expected = visible_modules(view).to_vec();
        expected.push("actions");
        assert_eq!(filled(&model), expected, "{view:?}");
    }
    let model = build_view_model_measured(&fixture_base.context()).0;
    assert_eq!(model.frame_revision, 3);
    assert_eq!(model.snapshot_revision, 7);
    assert_eq!(model.generated_boot_ns, NOW);
    assert_eq!(model.header.host_name, "dgx-test");
    assert_eq!(model.session, fixture_base.session);
    assert!(model.permit.is_none());
    let enabled: Vec<&str> = model
        .enabled_modules
        .iter()
        .map(|module| module.as_str())
        .collect();
    assert_eq!(enabled, BUILTIN_MODULES.to_vec());
    assert!(model.banners.is_empty());
}

#[test]
fn pending_confirmation_is_first_and_carries_action_details_on_any_view() {
    let mut fixture = Fixture::new();
    fixture.session.view = ViewKind::Overview;
    let mut actions = (*fixture.actions).clone();
    actions.confirmation = Some(confirmation());
    fixture.actions = Arc::new(actions);
    fixture.overload = dgxtop_core::view::OverloadState::Pressured;
    let model = build_view_model_measured(&fixture.context()).0;
    assert_eq!(model.banners[0].level, BannerLevel::Confirmation);
    assert_eq!(
        model.banners[0].text,
        "Confirm SIGTERM to python3 (pid 4242)? y = confirm · n/Esc = cancel · expires in 10s"
    );
    assert_eq!(model.banners[1].level, BannerLevel::Warning);
    let actions = model.modules.actions.as_ref().unwrap();
    assert_eq!(actions.confirmation.as_ref().unwrap().uid, 1000);
    assert!(model.modules.processes.is_some());
}

#[test]
fn reference_load_projection_stays_under_one_mebibyte() {
    let fixture = reference_fixture(128, 256);
    assert!(series_count(&fixture) >= 512);
    let (model, measured) = build_view_model_measured(&fixture.context());
    assert_eq!(measured, model.accounted_bytes(ONE_MIB));
    assert!(measured >= model.estimated_bytes());
    assert!(measured < ONE_MIB, "{measured} bytes");
    assert!(model.banners.is_empty(), "{:?}", model.banners);
    let cpu = model.modules.cpu.as_ref().unwrap();
    assert_eq!(cpu.cores.len(), 128);
    assert_eq!(cpu.utilization_trend.points.len(), 120);
    let gpu = model.modules.gpu.as_ref().unwrap();
    assert_eq!(gpu.cards.len(), 8);
    assert_eq!(gpu.cards[0].label, "GPU 0");
    let processes = model.modules.processes.as_ref().unwrap();
    assert_eq!(processes.total_rows, 256);
    // Only the estimated window plus overscan is copied, never all rows.
    let window = super::processes::visible_process_rows(50, 8);
    assert!(processes.rows.len() <= window + PROCESS_OVERSCAN_ROWS);
    assert!(processes.rows.len() < 256);
}

#[test]
fn the_process_table_copies_only_the_window() {
    let mut fixture = reference_fixture(4, 256);
    fixture.session.view = ViewKind::Overview;
    fixture.session.sort = SortKey::Pid;
    fixture.session.ascending = true;
    fixture.viewport = (120, 30);
    fixture.session.selected_process = Some(process_key(256, 1000));
    let model = build_view_model_measured(&fixture.context()).0;
    let processes = model.modules.processes.as_ref().unwrap();
    let visible = super::processes::visible_process_rows(30, 4);
    assert!(processes.rows.len() <= visible + PROCESS_OVERSCAN_ROWS);
    assert_eq!(processes.selected_index, Some(255));
    assert_eq!(processes.rows.last().unwrap().key, process_key(256, 1000));
    assert_eq!(processes.rows[0].gpus[1].gpu_label, "GPU 1");
    let table_bytes = serde_json::to_vec(processes.as_ref()).unwrap().len();
    assert!(table_bytes < 64 * 1024, "{table_bytes}");
}

/// The bound replaces writing the JSON only where that changes nothing: a
/// projection is published at most at `max_projection_bytes` of real JSON,
/// trimmed exactly as the exact length would trim it, for every budget and
/// view of the reference load.
#[test]
fn the_accounted_size_never_exceeds_the_budget_nor_trims_differently() {
    let views = [
        ViewKind::Overview,
        ViewKind::Overview,
        ViewKind::GpuDetail,
        ViewKind::Settings,
    ];
    for view in views {
        for max_bytes in [8_192_u32, 32_768, 65_536, 131_072, 262_144, 1_048_576] {
            let mut fixture = reference_fixture(256, 256);
            fixture.session.view = view;
            fixture.effective.runtime.max_projection_bytes = max_bytes;
            let limit = usize::try_from(max_bytes).unwrap();
            let (model, measured) = build_view_model_measured(&fixture.context());
            let exact = model.estimated_bytes();
            assert!(
                measured >= exact,
                "{view:?} {max_bytes}: {measured} < {exact}"
            );
            assert!(model.bytes_bound() >= exact, "{view:?} {max_bytes}");
            assert_eq!(
                measured <= limit,
                exact <= limit,
                "{view:?} {max_bytes}: published exactly when the JSON fits"
            );
            let reduced = model
                .banners
                .iter()
                .any(|banner| banner.text.starts_with("view reduced to fit"));
            let exceeded = model
                .banners
                .iter()
                .any(|banner| banner.text.starts_with("view exceeds"));
            assert!(!(reduced && exceeded), "{view:?} {max_bytes}");
            if exceeded {
                assert!(exact > limit.saturating_sub(1024), "{view:?} {max_bytes}");
            }
        }
    }
}

/// A projection well inside the budget is accounted at its bound: no JSON
/// is written for it, and the bound stays close to the real size.
#[test]
fn a_projection_inside_the_budget_is_accounted_at_its_bound() {
    let fixture = reference_fixture(128, 256);
    let (model, measured) = build_view_model_measured(&fixture.context());
    let exact = model.estimated_bytes();
    assert_eq!(measured, model.bytes_bound());
    assert!(measured >= exact);
    assert!(measured <= exact * 2, "{measured} vs {exact}");
}

#[test]
fn small_budget_is_respected_and_announced() {
    let mut fixture = reference_fixture(256, 256);
    fixture.effective.runtime.max_projection_bytes = 65_536;
    let (model, measured) = build_view_model_measured(&fixture.context());
    assert!(measured <= 65_536, "{measured} bytes");
    assert_eq!(measured, model.accounted_bytes(65_536));
    assert!(measured >= model.estimated_bytes());
    assert!(
        model.banners.iter().any(
            |banner| banner.text == "view reduced to fit runtime.max_projection_bytes (64 KiB)"
        )
    );
    let gpu = model.modules.gpu.as_ref().unwrap();
    assert_eq!(gpu.cards.len(), 8);
    assert!(model.modules.cpu.as_ref().unwrap().cores.len() < 256);
}

/// The HistoryService asks one bar per plot column (D-50), so the
/// projection shares its view as it is, however many bars it holds; only
/// the byte budget may shrink it.
#[test]
fn history_is_shared_as_the_service_built_it() {
    let mut fixture = Fixture::new();
    fixture.session.view = ViewKind::History;
    fixture.viewport = (100, 30);
    let small = build_view_model_measured(&fixture.context()).0;
    assert!(Arc::ptr_eq(
        small.modules.history.as_ref().unwrap(),
        &fixture.history
    ));
    let points = (0..2000_u64)
        .map(|second| HistoryPointView {
            start_utc_ns: second * 1_000_000_000,
            end_utc_ns: (second + 1) * 1_000_000_000,
            mean: Some(1.0),
            min: Some(0.5),
            max: Some(2.0),
        })
        .collect();
    fixture.history = Arc::new(HistoryView {
        charts: vec![history_chart(points), history_chart(Vec::new())],
        ..(*fixture.history).clone()
    });
    let large = build_view_model_measured(&fixture.context()).0;
    let history = large.modules.history.as_ref().unwrap();
    assert!(Arc::ptr_eq(history, &fixture.history));
    assert!(!history.charts[0].display_downsampled);
    assert_eq!(history.charts[0].points.len(), 2000);
}
