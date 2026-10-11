//! PresentationService view building.
//!
//! Contract between the Manager control loop (which owns state and calls
//! [`build_view_model`]) and the projection builders (pure functions over
//! immutable inputs). Builders never perform I/O, never block and only copy
//! the visible window of large tables (processes) plus a small overscan.

mod banners;
mod budget;
mod cell;
mod cpu;
mod disk;
mod gpu;
mod gpu_derived;
pub(crate) mod gpu_directory;
mod history;
mod history_charts;
mod index;
mod labels;
mod memory;
mod network;
mod ordering;
mod plan;
mod process_parts;
mod processes;
mod reader;
mod run_history;
mod run_inputs;
mod settings;
mod sources;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;

pub use cell::build_metric_cell;
pub(crate) use history_charts::history_charts;
pub use plan::visible_modules;
pub use processes::PROCESS_OVERSCAN_ROWS;
pub(crate) use run_inputs::run_inputs;

pub use crate::process_memory::ProcessMemoryBook;

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use dgxtop_core::action::PersistenceStatus;
use dgxtop_core::canonical::{CatalogSnapshot, ImmutableSnapshot};
use dgxtop_core::commands::ViewKind;
use dgxtop_core::config::{Config, ValueOrigin};
use dgxtop_core::identity::{MetricKey, ModuleId};
use dgxtop_core::naming::adapters;
use dgxtop_core::process_history::{BarSize, HistoryFilter};
use dgxtop_core::view::{
    ActionsView, GpuView, HeaderView, HistoryView, HostStatus, IntervalView, ModuleViews,
    OverloadState, SessionState, Trend, ViewModel,
};

use banners::BannerFacts;
use budget::BudgetHints;
use gpu::{GpuEnvironment, GpuTrendWidths};
use gpu_directory::GpuDirectory;
use plan::{ViewPlan, ViewportLayout};
use reader::MetricReader;
use sources::SourcesInputs;

/// Bounded recent values per trend series (sparklines), fed by the Manager
/// from committed snapshots. Gaps are kept as `None`.
#[derive(Debug, Clone, Default)]
pub struct TrendBook {
    capacity: usize,
    step_ms: u64,
    series: BTreeMap<MetricKey, VecDeque<Option<f64>>>,
}

impl TrendBook {
    /// `capacity` points per series at `step_ms` spacing.
    pub fn new(capacity: usize, step_ms: u64) -> Self {
        Self {
            capacity: capacity.max(1),
            step_ms,
            series: BTreeMap::new(),
        }
    }

    /// Appends one point (or a gap) to a series, evicting the oldest.
    pub fn push(&mut self, key: &MetricKey, value: Option<f64>) {
        let points = self.series.entry(key.clone()).or_default();
        if points.len() == self.capacity {
            points.pop_front();
        }
        points.push_back(value.filter(|number| number.is_finite()));
    }

    /// Whether a series has points.
    pub fn contains(&self, key: &MetricKey) -> bool {
        self.series.contains_key(key)
    }

    /// Drops series that are no longer wanted (bounded key set).
    pub fn retain(&mut self, mut keep: impl FnMut(&MetricKey) -> bool) {
        self.series.retain(|key, _| keep(key));
    }

    /// Most recent `width` points of a series (oldest first).
    pub fn trend(&self, key: &MetricKey, width: usize) -> Trend {
        let points = self
            .series
            .get(key)
            .map(|points| {
                let skip = points.len().saturating_sub(width);
                points.iter().skip(skip).copied().collect()
            })
            .unwrap_or_default();
        Trend {
            points,
            step_ms: self.step_ms,
        }
    }

    pub fn len(&self) -> usize {
        self.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }
}

/// Static host facts shown in the header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostFacts {
    pub host_name: String,
    pub kernel: Option<String>,
    pub cpu_model: Option<String>,
    pub version: String,
    /// `Some("replay")` when running a replay fixture (simulated data).
    pub namespace_label: Option<String>,
    /// Local time's offset from UTC at startup, seconds east, for the
    /// times of the Process History (D-76).
    pub utc_offset_s: i32,
}

/// Settings presentation inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsInputs<'a> {
    pub saved: &'a Config,
    pub effective: &'a Config,
    pub origins: &'a BTreeMap<String, ValueOrigin>,
    pub restart_pending: &'a [String],
    pub pending: Option<String>,
    pub last_result: Option<String>,
    pub status: &'a PersistenceStatus,
}

/// Everything a projection is built from (all immutable).
#[derive(Debug, Clone)]
pub struct PresentationContext<'a> {
    pub frame_revision: u64,
    pub now_boot_ns: u64,
    pub snapshot: &'a ImmutableSnapshot,
    pub catalog: &'a CatalogSnapshot,
    pub session: &'a SessionState,
    /// Terminal size reported by the session (columns, rows).
    pub viewport: (u16, u16),
    pub settings: SettingsInputs<'a>,
    pub hosts: &'a [HostStatus],
    pub intervals: &'a [IntervalView],
    pub overload: OverloadState,
    /// Built by the ProcessService.
    pub actions: Arc<ActionsView>,
    /// Built by the HistoryService for this session.
    pub history: Arc<HistoryView>,
    pub trends: &'a TrendBook,
    /// GPU processes' colors and memory over time (D-64).
    pub process_memory: &'a ProcessMemoryBook,
    /// The GPU process runs (D-76).
    pub process_history: &'a crate::process_history::ProcessHistoryBook,
    /// Whether runs outlive dgxtop.
    pub process_history_kept: bool,
    /// Wall clock minus boot clock when the projection is built: places the
    /// process memory record on the history view's time axis.
    pub boot_to_utc_ns: i128,
    pub facts: &'a HostFacts,
    /// Module ids enabled in the effective configuration.
    pub enabled_modules: &'a [String],
    /// Status of the optional history archive (`None` until it reports).
    pub archive: Option<&'a PersistenceStatus>,
}

/// Builds the header shown on every view.
pub fn build_header(context: &PresentationContext<'_>) -> HeaderView {
    let effective = context.settings.effective;
    let healthy = context
        .hosts
        .iter()
        .filter(|host| host.state == dgxtop_core::view::HostState::Healthy)
        .count();
    HeaderView {
        host_name: context.facts.host_name.clone(),
        kernel: context.facts.kernel.clone(),
        uptime_s: Some(context.now_boot_ns / 1_000_000_000),
        version: context.facts.version.clone(),
        namespace_label: context.facts.namespace_label.clone(),
        overload: context.overload,
        configured_interval_ms: effective.sampling.interval_ms,
        effective_interval_ms: context
            .intervals
            .iter()
            .map(|interval| interval.effective_ms)
            .min()
            .unwrap_or(effective.sampling.interval_ms),
        hosts_healthy: u32::try_from(healthy).unwrap_or(u32::MAX),
        hosts_total: u32::try_from(context.hosts.len()).unwrap_or(u32::MAX),
        read_only: effective.actions.read_only || !effective.actions.enabled,
        theme: effective.viewer.theme,
    }
}

/// Builds one immutable projection for one session: only the modules
/// visible in the session's view (and enabled) are filled, large tables are
/// windowed, and the result is kept within `runtime.max_projection_bytes`.
pub fn build_view_model(context: &PresentationContext<'_>) -> ViewModel {
    build_view_model_measured(context).0
}

/// [`build_view_model`] plus the final [`ViewModel::estimated_bytes`] of the
/// projection, so the caller can size its byte permit without serializing
/// the projection a second time.
pub fn build_view_model_measured(context: &PresentationContext<'_>) -> (ViewModel, usize) {
    let plan = ViewPlan::new(
        context.session.view,
        context.enabled_modules,
        context.actions.confirmation.is_some(),
    );
    let layout = ViewportLayout::new(context.viewport);
    let reader = MetricReader::new(
        context.snapshot,
        context.catalog,
        context.trends,
        context.now_boot_ns,
    );
    let directory = GpuDirectory::new(context.catalog, reader.index());
    let gpu_count = if plan.shows(plan::GPU) {
        directory.gpus().len()
    } else {
        0
    };
    let visible_process_rows =
        processes::visible_process_rows(context.session.view, layout.rows, gpu_count);
    let inputs = ModuleInputs {
        reader: &reader,
        directory: &directory,
        process_memory: context.process_memory,
        layout,
        visible_process_rows,
    };
    let facts = BannerFacts {
        disabled_in_view: plan.disabled(),
        conflict_count: banners::count_conflicts(context.snapshot),
    };
    let mut model = ViewModel {
        frame_revision: context.frame_revision,
        snapshot_revision: context.snapshot.revision,
        generated_boot_ns: context.now_boot_ns,
        header: build_header(context),
        session: context.session.clone(),
        modules: build_modules(context, &plan, &inputs),
        banners: banners::build_banners(context, &facts),
        enabled_modules: enabled_module_ids(context.enabled_modules),
        permit: None,
    };
    let max_bytes = usize::try_from(context.settings.effective.runtime.max_projection_bytes)
        .unwrap_or(usize::MAX);
    let size = budget::fit_projection(
        &mut model,
        max_bytes,
        &BudgetHints {
            visible_process_rows,
        },
    );
    (model, size)
}

/// Shared, read-only inputs of the module builders of one projection.
struct ModuleInputs<'a> {
    reader: &'a MetricReader<'a>,
    directory: &'a GpuDirectory<'a>,
    process_memory: &'a ProcessMemoryBook,
    layout: ViewportLayout,
    visible_process_rows: usize,
}

fn build_modules(
    context: &PresentationContext<'_>,
    plan: &ViewPlan,
    inputs: &ModuleInputs<'_>,
) -> ModuleViews {
    let reader = inputs.reader;
    let layout = inputs.layout;
    let effective = context.settings.effective;
    let mut modules = ModuleViews::default();
    if plan.shows(plan::CPU) {
        let model = context.facts.cpu_model.as_deref();
        modules.cpu = Some(Arc::new(cpu::build_cpu_view(
            reader,
            model,
            layout.chart_points(),
        )));
    }
    if plan.shows(plan::GPU) {
        modules.gpu = Some(Arc::new(build_gpu(context, inputs)));
    }
    if plan.shows(plan::MEMORY) {
        // The used chart spans the wider top panel (D-43).
        modules.memory = Some(Arc::new(memory::build_memory_view(
            reader,
            inputs.directory,
            inputs.process_memory,
            layout.chart_points(),
        )));
    }
    if plan.shows(plan::DISK) {
        modules.disk = Some(Arc::new(disk::build_disk_view(
            reader,
            layout.half_chart_points(),
        )));
    }
    if plan.shows(plan::NETWORK) {
        modules.network = Some(Arc::new(network::build_network_view(
            reader,
            effective.viewer.network_max_visible,
            layout.half_chart_points(),
        )));
    }
    if plan.shows(plan::PROCESSES) {
        modules.processes = Some(Arc::new(processes::build_processes_view(
            &context.snapshot.process_table,
            context.session,
            inputs.directory,
            inputs.process_memory,
            inputs.visible_process_rows,
        )));
    }
    if plan.shows(plan::HISTORY) {
        let charts = history::history_for_display(&context.history, layout.history_chart_points());
        modules.history = Some(process_parts::with_process_parts(
            charts,
            &process_parts::ProcessHistoryInputs {
                book: inputs.process_memory,
                directory: inputs.directory,
                boot_to_utc_ns: context.boot_to_utc_ns,
                cells: layout.history_chart_points(),
            },
        ));
        let bar = HistoryFilter::parse(&context.session.filter)
            .map_or(BarSize::Auto, |filter| filter.bar);
        modules.process_history = Some(Arc::new(run_history::build_process_history_view(
            &run_history::RunHistoryInputs {
                book: context.process_history,
                colors: inputs.process_memory,
                opened: context.session.selected_process.as_ref(),
                kept_on_disk: context.process_history_kept,
                utc_offset_s: context.facts.utc_offset_s,
                bar,
                columns: layout.run_chart_bars(),
                scales: &run_history::RunScales::read(reader, inputs.directory),
            },
        )));
    }
    if plan.shows(plan::SOURCES) {
        modules.sources = Some(Arc::new(sources::build_sources_view(&SourcesInputs {
            reader,
            directory: inputs.directory,
            session: context.session,
            hosts: context.hosts,
            intervals: context.intervals,
        })));
    }
    if plan.shows(plan::SETTINGS) {
        modules.settings = Some(Arc::new(settings::build_settings_view(
            &context.settings,
            context.intervals,
            context.overload,
            context.enabled_modules,
        )));
    }
    if plan.shows(plan::ACTIONS) {
        modules.actions = Some(Arc::clone(&context.actions));
    }
    modules
}

/// Overview shows every card as a summary with the utilization chart it
/// draws; the GPU view gives the selected card full-width trends of every
/// charted series and copies no trends for the others.
fn build_gpu(context: &PresentationContext<'_>, inputs: &ModuleInputs<'_>) -> GpuView {
    let widths = if context.session.view == ViewKind::GpuDetail {
        GpuTrendWidths {
            selected: inputs.layout.chart_points(),
            others: 0,
            detail: true,
        }
    } else {
        let summary = inputs
            .layout
            .gpu_summary_points(inputs.directory.gpus().len());
        GpuTrendWidths {
            selected: summary,
            others: summary,
            detail: false,
        }
    };
    let environment = GpuEnvironment {
        nvml_status: context
            .hosts
            .iter()
            .find(|host| host.adapter_id == adapters::NVML),
        gpu_enabled: context.settings.effective.sampling.gpu_enabled,
    };
    gpu::build_gpu_view(
        inputs.reader,
        inputs.directory,
        context.session.selected_entity_id.as_ref(),
        environment,
        widths,
    )
}

fn enabled_module_ids(enabled: &[String]) -> Vec<ModuleId> {
    enabled
        .iter()
        .filter_map(|module| ModuleId::new(module.as_str()).ok())
        .collect()
}
