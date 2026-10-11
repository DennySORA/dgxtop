//! Ratatui `TestBackend` render tests: every view and module at 160×50,
//! 100×30, 80×24, 40×10 and 20×5, plus semantics (missing states, conflicts,
//! tags, banners, registry extension, panic isolation, HitMap).

use std::sync::Arc;

use dgxtop_core::action::Signal;
use dgxtop_core::commands::ViewKind;
use dgxtop_core::identity::ModuleId;
use dgxtop_core::quality::SampleKind;
use dgxtop_core::view::{
    Banner, BannerLevel, DiskHistory, ModuleViews, OVERVIEW_GPU_CHART_ROWS, OverloadState,
    ViewModel,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use super::*;
use crate::registry::{DISPLAY_MODULE_API_VERSION, DisplayModule, RenderOutput, Topic, controls};
use crate::test_fixtures::{self, process_key};

const SIZES: [(u16, u16); 5] = [(160, 50), (100, 30), (80, 24), (40, 10), (20, 5)];

struct Rendered {
    rows: Vec<String>,
    report: FrameReport,
}

impl Rendered {
    fn text(&self) -> String {
        self.rows.join("\n")
    }

    fn contains(&self, needle: &str) -> bool {
        self.rows.iter().any(|row| row.contains(needle))
    }

    fn row_with(&self, needle: &str) -> Option<&String> {
        self.rows.iter().find(|row| row.contains(needle))
    }
}

fn render_with(
    viewer: &Viewer,
    model: Option<&ViewModel>,
    ui: &UiState,
    size: (u16, u16),
) -> Rendered {
    let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
    let mut report = None;
    terminal
        .draw(|frame| {
            report = Some(viewer.render(
                frame,
                &RenderInput {
                    model,
                    ui,
                    projection_age_ms: 0,
                    tick: 0,
                },
            ));
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let rows = (0..size.1)
        .map(|y| {
            (0..size.0)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    Rendered {
        rows,
        report: report.unwrap(),
    }
}

fn render(model: &ViewModel, size: (u16, u16)) -> Rendered {
    render_with(&Viewer::builtin(), Some(model), &UiState::default(), size)
}

fn model(view: ViewKind) -> ViewModel {
    test_fixtures::view_model(view, 7)
}

#[test]
fn every_view_renders_at_every_size_with_a_quit_hint() {
    for view in ViewKind::ALL {
        for size in SIZES {
            let rendered = render(&model(view), size);
            assert!(
                rendered.contains("quit"),
                "{view:?} {size:?}\n{}",
                rendered.text()
            );
            if size == (20, 5) {
                assert!(
                    rendered.contains("terminal too small"),
                    "{}",
                    rendered.text()
                );
                assert!(rendered.contains("(need 40x10)"), "{}", rendered.text());
                assert!(rendered.contains("q quit"));
            } else {
                assert!(
                    rendered.report.module_errors.is_empty(),
                    "{:?}",
                    rendered.report.module_errors
                );
                assert!(
                    rendered.contains("DGXTOP"),
                    "{view:?} {size:?}\n{}",
                    rendered.text()
                );
            }
        }
    }
}

#[test]
fn tiers_are_reported_per_size() {
    let tiers: Vec<SizeTier> = SIZES
        .iter()
        .map(|size| render(&model(ViewKind::Overview), *size).report.tier)
        .collect();
    assert_eq!(
        tiers,
        vec![
            SizeTier::TwoColumn,
            SizeTier::SingleColumn,
            SizeTier::SingleColumn,
            SizeTier::Minimal,
            SizeTier::TooSmall
        ]
    );
}

fn expanded(module: &str) -> UiState {
    let mut ui = UiState::default();
    ui.expanded.insert(ModuleId::new(module).unwrap());
    ui
}

/// Row index of the first line containing `needle`.
fn row_index(rendered: &Rendered, needle: &str) -> usize {
    rendered
        .rows
        .iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("missing {needle:?}\n{}", rendered.text()))
}

#[test]
fn overview_follows_the_reference_layout_and_keeps_every_module_visible_when_wide() {
    let rendered = render(&model(ViewKind::Overview), (160, 50));
    for module in ["cpu", "gpu", "memory", "disk", "network", "processes"] {
        assert!(
            rendered
                .report
                .rendered_modules
                .iter()
                .any(|id| id == module),
            "{module}: {:?}",
            rendered.report.rendered_modules
        );
    }
    for text in [
        "GPUs  2x",
        "GPU 0 │ NVIDIA GB10 │",
        "GPU 1 │ NVIDIA GB10 │",
        // Parts that say different things are set apart (D-66).
        "68°C │ 45.2 W · estimated │ 3000 MHz",
        "RAM 12.3 GiB / 64.0 GiB │ ■ OS 2.3 GiB",
        "[GPU] ■ other 10.0 GiB",
        "spark-01 │ 2x GPU │",
        // Each GPU's memory row (D-40, D-41): the discrete GPU's VRAM, the
        // unified GPU's share of the system RAM.
        "12.3 GiB / 80.0 GiB",
        "10.0 GiB / 64.0 GiB",
        "CPU ▸ Grace ARM Neoverse-V2",
        "(c/click cores)",
        "(j/k/↑/↓ select · s sort · r reverse · g GPU filter · / filter · K terminate · F9 kill)",
        "45.3%",
        "Memory · shared with GPU",
        "Disk I/O [nvme0n1]",
        "Network [enp1s0]",
        "GPU Processes",
        "python3",
    ] {
        assert!(
            rendered.contains(text),
            "missing {text:?}\n{}",
            rendered.text()
        );
    }
    assert!(
        !rendered.contains("unsupported / unsupported"),
        "a unified GPU never shows a framebuffer it does not have\n{}",
        rendered.text()
    );
    assert!(!rendered.contains("GPUGPU"), "{}", rendered.text());
    assert!(
        !rendered.contains("· usable"),
        "at-a-glance cells carry no quality words\n{}",
        rendered.text()
    );
    // CPU and memory side by side on top, then the GPUs, then disk and
    // network side by side, then the GPU processes down to the footer.
    let cpu_top = row_index(&rendered, "CPU ▸ Grace ARM Neoverse-V2");
    assert_eq!(cpu_top, row_index(&rendered, "Memory · shared with GPU"));
    // Memory is the wider of the two (D-40).
    let top = &rendered.rows[cpu_top];
    let memory_x = top[..top.find("╭ Memory").unwrap()].chars().count();
    assert!(memory_x < top.chars().count() / 2, "{top}");
    let gpu_top = row_index(&rendered, "GPUs  2x");
    let disk_top = row_index(&rendered, "Disk I/O [nvme0n1]");
    assert_eq!(disk_top, row_index(&rendered, "Network [enp1s0]"));
    let processes_top = row_index(&rendered, "GPU Processes · sort");
    assert!(cpu_top < gpu_top && gpu_top < disk_top && disk_top < processes_top);
    // Every chart sits under its bar in block columns (D-43): no braille
    // anywhere. The CPU's utilization and the used memory have three chart
    // rows; each GPU's utilization has its chart rows, and its memory bar
    // (VRAM, or RAM on unified memory) none.
    assert!(
        !rendered
            .text()
            .chars()
            .any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)),
        "{}",
        rendered.text()
    );
    let bars = &rendered.rows[cpu_top + 2];
    // Each bar stands three quarters of a row high (D-52); on a RAM shared
    // with a GPU the GPU legend takes the row above the used bar (D-69).
    assert!(bars.starts_with("│util ▆"), "{bars}");
    assert!(
        rendered.rows[cpu_top + 3].contains("│used ▆"),
        "{}",
        rendered.text()
    );
    assert!(
        chart_under(&rendered, cpu_top + 2, 3),
        "{}",
        rendered.text()
    );
    let gpu_rows = &rendered.rows[gpu_top..disk_top];
    let util_rows: Vec<usize> = (0..gpu_rows.len())
        .filter(|row| gpu_rows[*row].starts_with("│util "))
        .collect();
    assert_eq!(util_rows.len(), 2, "{}", rendered.text());
    let chart_rows = usize::from(OVERVIEW_GPU_CHART_ROWS);
    for util in util_rows {
        assert!(
            chart_under(&rendered, gpu_top + util, chart_rows),
            "{}",
            rendered.text()
        );
        let memory = &gpu_rows[util + 1 + chart_rows];
        assert!(
            memory.starts_with("│VRAM ") || memory.starts_with("│RAM  "),
            "{memory}"
        );
        assert!(!has_sparkline(memory), "{memory}");
    }
    // Each value follows its gauge, and the utilizations of both GPUs end
    // on one column.
    let end_column = |value: &str| {
        let row = rendered.row_with(value).unwrap();
        row[..row.find(value).unwrap()].chars().count() + value.chars().count()
    };
    assert_eq!(end_column("90.0%"), end_column("70.0%"));
    let gpu_zero = rendered.row_with("90.0%").unwrap();
    assert!(gpu_zero.contains("▆ 90.0%"), "{gpu_zero}");
}

/// Whether a rendered row holds a partial block of a chart column; a bar
/// (`▆`, D-52) is not one.
fn has_sparkline(row: &str) -> bool {
    row.chars().any(|c| ('▁'..='▇').contains(&c) && c != '▆')
}

/// Whether the `rows` rows under row `bar` draw a chart.
fn chart_under(rendered: &Rendered, bar: usize, rows: usize) -> bool {
    rendered.rows[bar + 1..=bar + rows]
        .iter()
        .any(|row| has_sparkline(row))
}

#[test]
fn processes_view_follows_the_reference_columns_and_marks_the_selection() {
    for size in [(160, 50), (100, 30), (80, 24), (40, 10)] {
        let rendered = render(&model(ViewKind::Processes), size);
        let header = rendered
            .row_with("PID")
            .unwrap_or_else(|| panic!("{size:?}\n{}", rendered.text()));
        let pid = header.find("PID").unwrap();
        if let Some(gpu) = header.find("GPU") {
            assert!(gpu < pid, "{size:?}: the GPU leads (D-66): {header}");
        }
        assert!(rendered.contains("▶"), "{size:?}\n{}", rendered.text());
        assert!(rendered.contains("4242"), "{size:?}\n{}", rendered.text());
        assert!(rendered.report.hit_map.visible_selected_process().is_some());
    }
    let wide = render(&model(ViewKind::Processes), (160, 50));
    for text in [
        "USER",
        "NAME",
        "GPU%",
        "GPU MEM",
        "CPU%",
        "CPU MEM",
        "python3",
        "0,1",
        "45.0%,n/a",
        "rows 1–12 of 12",
        "sort GPU memory ▼",
    ] {
        assert!(wide.contains(text), "missing {text:?}\n{}", wide.text());
    }
}

#[test]
fn processes_window_shows_scroll_position_filters_and_truncation() {
    let mut model = model(ViewKind::Processes);
    let mut processes = test_fixtures::processes_view(40, 360, 412);
    processes.selected = Some(processes.rows[39].key.clone());
    processes.selected_index = Some(399);
    processes.truncated = true;
    processes.filter = "work".into();
    processes.gpu_filter = Some((test_fixtures::entity("gpu:GPU-0"), "GPU0".into()));
    model.modules.processes = Some(Arc::new(processes));
    let rendered = render(&model, (100, 30));
    assert!(rendered.contains("filter \"work\""), "{}", rendered.text());
    assert!(rendered.contains("GPU GPU0"), "{}", rendered.text());
    assert!(rendered.contains("of 412"), "{}", rendered.text());
    assert!(rendered.contains("truncated"), "{}", rendered.text());
    assert!(
        rendered.contains("┃"),
        "scrollbar thumb\n{}",
        rendered.text()
    );
    let selected = rendered.report.hit_map.visible_selected_process().unwrap();
    assert_eq!(
        selected.pid.get(),
        4242 + 360 + 39,
        "selected row stays visible"
    );
}

/// The history view's projection: the Manager sends the history module
/// alone, so the charts take the full width.
fn history_model() -> ViewModel {
    let mut model = model(ViewKind::History);
    model.modules = ModuleViews {
        history: model.modules.history.take(),
        ..ModuleViews::default()
    };
    model
}

/// The rows of `rendered` holding `title`'s rule, which must exist.
fn panel_row(rendered: &Rendered, title: &str) -> usize {
    row_index(rendered, &format!("{title} ─"))
}

#[test]
fn history_says_when_it_loads_fails_is_partly_recorded_or_has_no_series() {
    let mut model = history_model();
    let mut history = test_fixtures::history_view();
    let cpu = &mut history.charts[0];
    cpu.pending = true;
    cpu.points.clear();
    (cpu.mean, cpu.peak) = (None, None);
    history.charts[1].error = Some("QueryTooLarge: narrow the window".into());
    for chart in &mut history.charts {
        chart.coverage_ratio = Some(0.2);
        chart.covered_ns = Some(720 * 1_000_000_000);
    }
    model.modules.history = Some(Arc::new(history.clone()));
    let rendered = render(&model, (213, 41));
    let text = rendered.text();
    let cpu = panel_row(&rendered, "CPU utilization");
    assert!(rendered.rows[cpu + 1].contains("loading…"), "{text}");
    assert!(
        rendered.rows[cpu].contains("query failed: QueryTooLarge: narrow the window"),
        "{text}"
    );
    // Kept on disk: the bar says how much of the window it holds and that
    // the gaps are the times dgxtop was not running (D-51).
    assert!(
        rendered.rows[2].contains("recorded 12m of 1h · only while dgxtop runs"),
        "{text}"
    );
    // Without the archive the history starts with dgxtop; an archive that
    // cannot be read is said so.
    for (disk, why) in [
        (DiskHistory::Off, "history starts with dgxtop"),
        (
            DiskHistory::Failed("locked".into()),
            "disk history unavailable",
        ),
    ] {
        let mut kept = history.clone();
        kept.disk = disk;
        model.modules.history = Some(Arc::new(kept));
        let rendered = render(&model, (213, 41));
        assert!(
            rendered.rows[2].contains(&format!("recorded 12m of 1h · {why}")),
            "{}",
            rendered.text()
        );
    }
    history.error = Some("no history store".into());
    model.modules.history = Some(Arc::new(history.clone()));
    let unavailable = render(&model, (100, 30));
    assert!(
        unavailable.contains("history unavailable: no history store"),
        "{}",
        unavailable.text()
    );
    history.error = None;
    // A short terminal: the bar names the panels left out; nothing claims
    // there is no data.
    model.modules.history = Some(Arc::new(history.clone()));
    let short = render(&model, (100, 10));
    assert!(
        // The keys moved to the title (D-70), so the bar has room for the
        // fuller note.
        short.contains("+7 panels · +2 GPUs' long-term statistics on a taller terminal"),
        "{}",
        short.text()
    );
    assert!(!short.contains("no series"), "{}", short.text());
    history.charts.clear();
    model.modules.history = Some(Arc::new(history));
    let empty = render(&model, (100, 30));
    assert!(empty.contains("no series to chart yet"), "{}", empty.text());
}

#[test]
fn sources_view_explains_arbitration() {
    let rendered = render(&model(ViewKind::Sources), (160, 50));
    for text in [
        "Sources · policy r3",
        "≠ conflict",
        "26.0 > 5.0",
        "conflicting",
        "fresh/unknown",
        "quarantined (requires intervention)",
        "overload x2",
        "2 s",
        "hwmon:nct6775:temp7",
    ] {
        assert!(
            rendered.contains(text),
            "missing {text:?}\n{}",
            rendered.text()
        );
    }
    for size in [(100, 30), (80, 24), (40, 10)] {
        let rendered = render(&model(ViewKind::Sources), size);
        assert!(
            rendered.contains("Sources"),
            "{size:?}\n{}",
            rendered.text()
        );
    }
}

#[test]
fn settings_view_shows_origins_restart_and_pending() {
    let rendered = render(&model(ViewKind::Settings), (160, 50));
    for text in [
        "Settings · revision 7",
        "config healthy",
        "pending: SetSamplingInterval 500 ms",
        "sampling.interval_ms",
        "restart required",
        "CLI override",
        "overload multiplier x2",
        "[ ] history",
    ] {
        assert!(
            rendered.contains(text),
            "missing {text:?}\n{}",
            rendered.text()
        );
    }
    for size in [(100, 30), (80, 24), (40, 10)] {
        assert!(render(&model(ViewKind::Settings), size).contains("Settings"));
    }
}

#[test]
fn actions_view_never_says_terminated() {
    let rendered = render(&model(ViewKind::Actions), (160, 50));
    assert!(
        rendered.contains("signal sent (exit not observed)"),
        "{}",
        rendered.text()
    );
    assert!(rendered.contains("unknown outcome"), "{}", rendered.text());
    assert!(rendered.contains("audit healthy"), "{}", rendered.text());
    assert!(
        rendered.contains("never retried automatically"),
        "{}",
        rendered.text()
    );
    assert!(!rendered.text().contains("terminated"));
    for size in [(100, 30), (80, 24), (40, 10)] {
        assert!(render(&model(ViewKind::Actions), size).contains("Actions"));
    }
}

#[test]
fn confirmation_banner_is_visible_and_never_clipped_at_40x10() {
    let mut model = model(ViewKind::Processes);
    model.modules.actions = Some(Arc::new(test_fixtures::actions_view(true)));
    model.banners = vec![Banner {
        level: BannerLevel::Info,
        text: "informational banner".into(),
    }];
    let ui = UiState {
        mode: ModeView::Confirm {
            target: process_key(4242, 90_100),
            signal: Signal::Sigterm,
            remaining_ms: Some(9_000),
        },
        ..UiState::default()
    };
    for size in [(160, 50), (80, 24), (40, 10)] {
        let rendered = render_with(&Viewer::builtin(), Some(&model), &ui, size);
        let text = rendered.text();
        assert!(text.contains("CONFIRM"), "{size:?}\n{text}");
        assert!(text.contains("pid 4242"), "{size:?}\n{text}");
        assert!(text.contains("y / Enter send"), "{size:?}\n{text}");
        assert!(text.contains("quit"), "{size:?}\n{text}");
        assert!(!text.contains(test_fixtures::token().as_str()));
    }
    let narrow = render_with(&Viewer::builtin(), Some(&model), &ui, (40, 10)).text();
    for detail in [
        "SIGTERM pid 4242 (python3)?",
        "uid 1000",
        "start 90100",
        "cannot be undone",
        "expires in 9s",
        "unsaved work may be lost",
    ] {
        assert!(
            narrow.contains(detail),
            "every confirmation detail fits at 40x10: {detail:?}\n{narrow}"
        );
    }
    let tiny = render_with(&Viewer::builtin(), Some(&model), &ui, (20, 5));
    assert!(tiny.contains("q quit · y send"), "{}", tiny.text());
}

#[test]
fn shed_trends_and_lists_render_text_not_blank() {
    let mut model = model(ViewKind::Overview);
    let mut cpu = test_fixtures::cpu_view();
    cpu.utilization_trend.points.clear();
    model.modules.cpu = Some(Arc::new(cpu));
    let mut network = test_fixtures::network_view();
    network.interfaces.clear();
    model.modules.network = Some(Arc::new(network));
    model.banners = vec![Banner {
        level: BannerLevel::Warning,
        text: "projection shortened to stay under the byte budget".into(),
    }];
    let rendered = render_with(
        &Viewer::builtin(),
        Some(&model),
        &expanded("cpu"),
        (160, 50),
    );
    assert!(rendered.contains("no trend data"), "{}", rendered.text());
    assert!(
        rendered.contains("no network interfaces reported"),
        "{}",
        rendered.text()
    );
    assert!(
        rendered.contains("projection shortened"),
        "{}",
        rendered.text()
    );
}

#[test]
fn simulated_and_replay_values_are_tagged() {
    let mut model = model(ViewKind::Overview);
    let mut cpu = test_fixtures::cpu_view();
    cpu.total.kind = SampleKind::Simulated;
    model.modules.cpu = Some(Arc::new(cpu));
    model.header.namespace_label = Some("replay".into());
    let rendered = render(&model, (160, 50));
    assert!(rendered.contains("simulated"), "{}", rendered.text());
    assert!(
        rendered.contains("[REPLAY · simulated data]"),
        "{}",
        rendered.text()
    );
}

#[test]
fn header_shows_the_host_and_warnings_and_the_footer_the_interval() {
    let mut model = model(ViewKind::Overview);
    model.header.effective_interval_ms = 2000;
    model.header.overload = OverloadState::Throttled;
    model.header.read_only = true;
    let rendered = render(&model, (200, 50));
    let header = &rendered.rows[0];
    for text in [
        "DGXTOP",
        "1 Overview",
        "spark-01",
        "2x GPU",
        "drv:580.65",
        "up 1h 00m",
        "READ-ONLY",
        "overload throttled",
        "hosts 6/7 healthy",
    ] {
        assert!(header.contains(text), "missing {text:?}: {header}");
    }
    let footer = rendered.rows.last().unwrap();
    assert!(
        footer.contains("+/- interval 1 s (effective 2 s)"),
        "{footer}"
    );
    assert!(
        rendered.contains("overload throttled: effective interval 2 s"),
        "{}",
        rendered.text()
    );
}

#[test]
fn missing_topics_render_no_data_instead_of_zero() {
    let mut model = model(ViewKind::GpuDetail);
    model.modules.gpu = None;
    let rendered = render(&model, (100, 30));
    assert!(
        rendered.contains("no gpu data in this projection"),
        "{}",
        rendered.text()
    );
    let mut empty = test_fixtures::view_model(ViewKind::Overview, 3);
    empty.modules = dgxtop_core::view::ModuleViews::default();
    let rendered = render(&empty, (160, 50));
    assert!(
        rendered.contains("no cpu data in this projection"),
        "{}",
        rendered.text()
    );
    assert!(!rendered.contains("0.0%"), "{}", rendered.text());
}

#[test]
fn gpu_unavailable_reason_is_shown() {
    let mut model = model(ViewKind::GpuDetail);
    model.modules.gpu = Some(Arc::new(dgxtop_core::view::GpuView {
        cards: Vec::new(),
        selected: None,
        unavailable_reason: Some("NVML library not found".into()),
    }));
    let rendered = render(&model, (100, 30));
    assert!(
        rendered.contains("GPU unavailable: NVML library not found"),
        "{}",
        rendered.text()
    );
}

#[test]
fn tabs_and_rows_produce_hit_targets_with_the_frame_revision() {
    let rendered = render(&model(ViewKind::Processes), (160, 50));
    let map = &rendered.report.hit_map;
    assert_eq!(map.frame_revision(), 7);
    for tab in crate::views::OFFERED_TABS {
        assert_eq!(map.count(tab.control_id()), 1, "{tab:?}");
    }
    for hidden in [ViewKind::Sources, ViewKind::Processes] {
        assert_eq!(
            map.count(controls::tab(hidden)),
            0,
            "{hidden:?} is not offered"
        );
    }
    assert_eq!(map.visible_process_rows(), 12);
    let row = map
        .targets()
        .iter()
        .find(|target| target.control_id == controls::PROCESS_ROW)
        .unwrap();
    assert!(row.process_key.is_some());
    let hit = map.hit(row.rect.x, row.rect.y).unwrap();
    assert_eq!(hit.process_key, row.process_key);
}

struct FixtureModule {
    manifest: DisplayModuleManifest,
}

impl FixtureModule {
    fn new(id: &str, api_version: u32) -> Self {
        Self {
            manifest: DisplayModuleManifest {
                module_id: ModuleId::new(id).unwrap(),
                api_version,
                title: "Fixture health".into(),
                topics: vec![Topic::Health],
                min_columns: 20,
                min_rows: 3,
                priority: 10,
                slots: vec![Slot::Overview],
                enabled_by_default: true,
                supports_compact: true,
            },
        }
    }
}

impl DisplayModule for FixtureModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn render(
        &self,
        _context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        frame.buffer_mut().set_string(
            area.x,
            area.y,
            format!("fixture module ok r{}", model.frame_revision),
            ratatui::style::Style::new(),
        );
        RenderOutput {
            hit_targets: Vec::new(),
            accessibility_labels: vec!["fixture: ok".into()],
        }
    }
}

struct PanickingModule {
    manifest: DisplayModuleManifest,
}

impl DisplayModule for PanickingModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn render(
        &self,
        _context: &DisplayContext,
        _model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        frame.buffer_mut().set_string(
            area.x,
            area.y,
            "half-drawn garbage",
            ratatui::style::Style::new(),
        );
        panic!("fixture failure");
    }
}

#[test]
fn an_eleventh_fixture_module_is_registered_and_laid_out() {
    let mut viewer = Viewer::builtin();
    viewer
        .registry_mut()
        .register(Box::new(FixtureModule::new(
            "fixture",
            DISPLAY_MODULE_API_VERSION,
        )))
        .unwrap();
    assert_eq!(viewer.registry().len(), 11);
    let rendered = render_with(
        &viewer,
        Some(&model(ViewKind::Overview)),
        &UiState::default(),
        (160, 50),
    );
    assert!(
        rendered.contains("fixture module ok r7"),
        "{}",
        rendered.text()
    );
    assert!(
        rendered
            .report
            .rendered_modules
            .contains(&"fixture".to_owned())
    );
    assert!(
        rendered
            .report
            .accessibility_labels
            .contains(&"fixture: ok".to_owned())
    );
}

#[test]
fn a_panicking_module_is_isolated_as_a_module_error() {
    let mut viewer = Viewer::builtin();
    let mut manifest = FixtureModule::new("crashy", DISPLAY_MODULE_API_VERSION).manifest;
    manifest.priority = 100;
    viewer
        .registry_mut()
        .register(Box::new(PanickingModule { manifest }))
        .unwrap();
    let rendered = render_with(
        &viewer,
        Some(&model(ViewKind::Overview)),
        &UiState::default(),
        (160, 50),
    );
    assert!(rendered.contains("module error"), "{}", rendered.text());
    assert!(rendered.contains("fixture failure"), "{}", rendered.text());
    assert!(
        !rendered.contains("half-drawn garbage"),
        "{}",
        rendered.text()
    );
    assert!(
        rendered.contains("CPU ▸ Grace ARM Neoverse-V2"),
        "other modules keep rendering\n{}",
        rendered.text()
    );
    assert_eq!(rendered.report.module_errors.len(), 1);
    assert!(rendered.report.rendered_modules.contains(&"cpu".to_owned()));
}

#[test]
fn stale_values_show_the_last_good_value_with_its_age() {
    let mut model = model(ViewKind::Overview);
    let mut cpu = test_fixtures::cpu_view();
    cpu.package_temperatures[0].cell =
        test_fixtures::stale(dgxtop_core::metric::CanonicalUnit::DegC, 72.0, 12_000);
    model.modules.cpu = Some(Arc::new(cpu));
    let rendered = render_with(
        &Viewer::builtin(),
        Some(&model),
        &expanded("cpu"),
        (160, 50),
    );
    assert!(
        rendered.contains("stale · last 72°C 12s ago"),
        "{}",
        rendered.text()
    );
}
