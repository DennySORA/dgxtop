//! PresentationService view building.
//!
//! Contract between the Manager control loop (which owns state and calls
//! [`build_view_model_measured`]) and the projection builders (pure functions over
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
mod settings;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;

pub use cell::build_metric_cell;
pub(crate) use history_charts::history_charts;
pub use plan::visible_modules;
pub use processes::PROCESS_OVERSCAN_ROWS;
pub(crate) use run_history::run_request;

use std::collections::BTreeMap;
use std::sync::Arc;

use dgxtop_core::action::PersistenceStatus;
use dgxtop_core::canonical::{CatalogSnapshot, ImmutableSnapshot};
use dgxtop_core::commands::ViewKind;
use dgxtop_core::config::{BUILTIN_MODULES, Config, ValueOrigin};
use dgxtop_core::identity::ModuleId;
use dgxtop_core::naming::adapters;
use dgxtop_core::ports::{ProcessMemoryReader, TrendReader};
use dgxtop_core::process_model::ProcessHistoryModel;
use dgxtop_core::view::{
    ActionsView, GpuView, HeaderView, HistoryView, HostStatus, IntervalView, ModuleViews,
    OverloadState, SessionState, ViewModel,
};

use banners::BannerFacts;
use budget::BudgetHints;
use gpu::{GpuEnvironment, GpuTrendWidths};
use gpu_directory::GpuDirectory;
use plan::{ViewPlan, ViewportLayout};
use reader::MetricReader;

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
#[derive(Clone)]
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
    /// The state's short trends (D-99).
    pub trends: &'a dyn TrendReader,
    /// The GPU process runs and colors the history's process actor
    /// published (D-64, D-76, D-79).
    pub processes: &'a ProcessHistoryModel,
    /// What each GPU process held over time, for the history's memory
    /// charts (D-64).
    pub process_memory: &'a dyn ProcessMemoryReader,
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
        redline_threshold_percent: effective.viewer.redline_threshold_percent,
    }
}

/// Builds one immutable projection for one session: only the modules
/// visible in the session's view (and enabled) are filled, large tables are
/// windowed, and the result is kept within `runtime.max_projection_bytes`.
/// Returns it with the size it is accounted at
/// ([`ViewModel::accounted_bytes`]), so the caller can size its byte permit
/// without measuring the projection a second time.
pub fn build_view_model_measured(context: &PresentationContext<'_>) -> (ViewModel, usize) {
    let plan = ViewPlan::new(context.session.view, context.enabled_modules);
    let layout = ViewportLayout::new(context.viewport);
    let reader = MetricReader::new(context.snapshot, context.catalog, context.now_boot_ns)
        .with_trends(context.trends);
    let directory = GpuDirectory::new(context.catalog, reader.index());
    let gpu_count = if plan.shows(plan::GPU) {
        directory.gpus().len()
    } else {
        0
    };
    let visible_process_rows = processes::visible_process_rows(layout.rows, gpu_count);
    let inputs = ModuleInputs {
        reader: &reader,
        directory: &directory,
        processes: context.processes,
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
    /// The GPU processes' colors and runs.
    processes: &'a ProcessHistoryModel,
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
            &inputs.processes.tracked,
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
            inputs.processes,
            inputs.visible_process_rows,
        )));
    }
    if plan.shows(plan::HISTORY) {
        modules.history = Some(process_parts::with_process_parts(
            Arc::clone(&context.history),
            &process_parts::ProcessHistoryInputs {
                tracked: &inputs.processes.tracked,
                memory: context.process_memory,
                directory: inputs.directory,
                cells: layout.history_chart_points(),
            },
        ));
        let opened = run_history::run_request(context.session, context.viewport);
        modules.process_history = Some(Arc::new(run_history::build_process_history_view(
            &run_history::RunHistoryInputs {
                model: inputs.processes,
                opened: opened.as_ref(),
                utc_offset_s: context.facts.utc_offset_s,
                bar: context.session.process_bar,
                scales: &run_history::RunScales::read(reader, inputs.directory),
            },
        )));
    }
    if plan.shows(plan::SETTINGS) {
        modules.settings = Some(Arc::new(settings::build_settings_view(
            &context.settings,
            &settings::SettingsRuntime {
                intervals: context.intervals,
                overload: context.overload,
                enabled_modules: context.enabled_modules,
                hosts: context.hosts,
            },
        )));
    }
    // The process actions' state is the session's, not a module's: every
    // projection carries it, so a pending confirmation is a banner in
    // every view (D-91, UI.md §2, §6).
    modules.actions = Some(Arc::clone(&context.actions));
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

/// The enabled built-in modules; a retired id a saved file still enables
/// is left out (D-91).
fn enabled_module_ids(enabled: &[String]) -> Vec<ModuleId> {
    enabled
        .iter()
        .filter(|module| BUILTIN_MODULES.contains(&module.as_str()))
        .filter_map(|module| ModuleId::new(module.as_str()).ok())
        .collect()
}
