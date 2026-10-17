//! Immutable view projections produced by the Manager's PresentationService
//! and consumed by the Viewer. A projection is a small, bounded, already
//! arbitrated sub-view: the Viewer only formats it and never recomputes
//! authoritative values, queries a store or samples hardware.

use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::action::{PersistenceStatus, Signal};
use crate::budget::BytePermit;
use crate::canonical::GpuUsageKind;
use crate::commands::{SortKey, ViewKind};
use crate::config::{Theme, ValueOrigin};
use crate::identity::{EntityId, MetricKey, ModuleId, ProcessKey, SessionId};
use crate::metric::{CanonicalUnit, Scalar};
use crate::process_history::{RunEnd, RunInfo, RunMetric};
use crate::quality::{Quality, SampleKind};
use crate::tiers::BarSize;

/// Why a cell has (or lacks) a value; always rendered as text, never only color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellState {
    /// Selected healthy source.
    Ok,
    /// Valid fallback source in use.
    Degraded,
    /// Capability reported as unsupported.
    Unsupported,
    /// Read failed (timeout/error/removed).
    ReadFailed,
    PermissionDenied,
    /// Independent sources disagree beyond tolerance.
    Conflict,
    /// Last value is stale; not authoritative.
    Stale,
    /// Counter-derived metric waiting for a second sample.
    Warmup,
    /// No eligible candidate / not reported.
    Unavailable,
}

impl CellState {
    pub const fn has_value(self) -> bool {
        matches!(self, Self::Ok | Self::Degraded)
    }

    /// Human label used when there is no value.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "fallback",
            Self::Unsupported => "unsupported",
            Self::ReadFailed => "read failed",
            Self::PermissionDenied => "permission denied",
            Self::Conflict => "source conflict",
            Self::Stale => "stale",
            Self::Warmup => "warming up",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Conflict details for one cell (e.g. "A 68°C / B 42°C").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConflictView {
    pub entries: Vec<(String, Scalar)>,
}

/// Last good annotation, displayed next to (not instead of) the current state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastGoodView {
    pub value: Scalar,
    pub age_ms: u64,
}

/// Top of every temperature's scale, as a gauge's 100 % (D-45, D-104): the
/// GPU's temperature bar and History chart, and the color of a temperature
/// wherever it is written.
pub const TEMPERATURE_SCALE_C: f64 = 100.0;

/// Rows of each overview GPU's utilization chart (D-43).
pub const OVERVIEW_GPU_CHART_ROWS: u16 = 2;
/// Overview GPU panel geometry (D-38, D-43), shared so the Viewer lays the
/// panel out and the Manager sizes the copied process window from the same
/// rows: per GPU a headline, the utilization bar with its chart under it,
/// and the memory bar.
pub const OVERVIEW_GPU_ROWS: u16 = 3 + OVERVIEW_GPU_CHART_ROWS;

/// Rows `count` GPUs take inside the overview GPU panel: each GPU's rows
/// and a divider between two GPUs (D-66).
pub const fn overview_gpus_rows(count: u16) -> u16 {
    count
        .saturating_mul(OVERVIEW_GPU_ROWS)
        .saturating_add(count.saturating_sub(1))
}

/// One displayable metric value with its evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricCell {
    pub value: Option<Scalar>,
    pub unit: CanonicalUnit,
    pub state: CellState,
    pub kind: SampleKind,
    pub quality: Quality,
    pub age_ms: Option<u64>,
    /// Short source label, e.g. "package 0 · hwmon".
    pub source_label: Option<String>,
    pub conflict: Option<ConflictView>,
    pub last_good: Option<LastGoodView>,
}

impl MetricCell {
    /// A cell with no value in the given state.
    pub fn missing(unit: CanonicalUnit, state: CellState) -> Self {
        Self {
            value: None,
            unit,
            state,
            kind: SampleKind::Measured,
            quality: Quality::Unknown,
            age_ms: None,
            source_label: None,
            conflict: None,
            last_good: None,
        }
    }

    pub fn value_f64(&self) -> Option<f64> {
        if self.state.has_value() {
            self.value.map(Scalar::to_f64_lossy)
        } else {
            None
        }
    }
}

/// Trend points for sparklines; `None` is a gap and is drawn as a break.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Trend {
    pub points: Vec<Option<f64>>,
}

/// Colors of GPU processes (D-64, D-65): at most this many processes listed
/// at once have a color of their own, the ones holding the most GPU memory;
/// any further process counts among the other processes. The Viewer's
/// process palette has this many colors.
pub const PROCESS_COLOR_SLOTS: usize = 8;

/// What one GPU process holds of a memory bar now (D-64). `slot`
/// (`0..PROCESS_COLOR_SLOTS`) picks its color: it stays with the process
/// while it is tracked and follows from its name when it can (D-65), so the
/// process has the same color on every bar, chart and table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessShare {
    pub slot: u8,
    pub pid: i32,
    pub name: String,
    pub bytes: u64,
}

/// A tracked GPU process's part of every point of a trend, aligned with
/// the trend at its newest end (D-64); `None` where the process held none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessTrend {
    pub slot: u8,
    pub trend: Trend,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoreView {
    pub core_id: String,
    pub utilization: MetricCell,
    pub frequency: MetricCell,
    pub temperature: MetricCell,
}

/// The cores of one kind: the logical CPUs sharing one maximum frequency
/// (`cpuinfo_max_freq`), such as the efficiency or the performance cores of
/// a hybrid CPU.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrequencyGroup {
    /// The group's maximum frequency; missing when cpufreq reports none.
    pub maximum: MetricCell,
    /// The mean current frequency of the group's cores reporting one now
    /// (derived, D-67).
    pub current: MetricCell,
    /// Logical CPUs in the group.
    pub cores: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabeledCell {
    pub label: String,
    pub cell: MetricCell,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuView {
    pub model: Option<String>,
    pub total: MetricCell,
    pub user: MetricCell,
    pub system: MetricCell,
    pub iowait: MetricCell,
    pub steal: MetricCell,
    pub cores: Vec<CoreView>,
    /// Current and maximum frequency per kind of core, by ascending
    /// maximum; empty without cpufreq.
    pub frequencies: Vec<FrequencyGroup>,
    pub package_temperatures: Vec<LabeledCell>,
    pub soc_temperatures: Vec<LabeledCell>,
    /// Each CPU package's power: the CPU line's `⚡` and the CPU's part of
    /// the host's total power (D-106).
    pub package_power: Vec<LabeledCell>,
    /// The platform's and the CPU clusters' power, listed when the panel
    /// is expanded.
    pub power: Vec<LabeledCell>,
    pub load: [MetricCell; 3],
    pub tasks_running: MetricCell,
    pub tasks_total: MetricCell,
    pub utilization_trend: Trend,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuCard {
    pub entity_id: EntityId,
    /// Display label (NVML index as shown by the driver); identity is `entity_id`.
    pub label: String,
    pub name: String,
    pub uuid: Option<String>,
    /// The GPU allocates from system RAM (unified memory, e.g. DGX Spark):
    /// it has no framebuffer of its own, and its allocations are RAM use.
    pub unified_memory: bool,
    pub utilization: MetricCell,
    pub memory_busy: MetricCell,
    pub temperature: MetricCell,
    pub power_draw: MetricCell,
    pub power_limit: MetricCell,
    pub clocks: Vec<LabeledCell>,
    pub fan: MetricCell,
    pub framebuffer_used: MetricCell,
    pub framebuffer_free: MetricCell,
    pub framebuffer_total: MetricCell,
    pub framebuffer_reserved: MetricCell,
    pub process_allocated: MetricCell,
    /// On a unified-memory GPU the system RAM its allocations come from,
    /// the scale of its memory bar; unsupported on a GPU with a framebuffer.
    pub system_ram_total: MetricCell,
    pub pcie_tx: MetricCell,
    pub pcie_rx: MetricCell,
    pub energy: MetricCell,
    pub ecc_corrected: MetricCell,
    pub ecc_uncorrected: MetricCell,
    pub retired_single: MetricCell,
    pub retired_double: MetricCell,
    pub encoder: MetricCell,
    pub decoder: MetricCell,
    pub nvlink_active: MetricCell,
    /// Performance state, 0 (maximum performance) to 15 (D-47).
    pub performance_state: MetricCell,
    /// Clock-limit reason bits ([`crate::gpu_state::THROTTLE_REASONS`]).
    pub throttle_reasons: MetricCell,
    /// The device's own slowdown and shutdown temperatures.
    pub temperature_slowdown: MetricCell,
    pub temperature_shutdown: MetricCell,
    /// The PCIe link now (it trains down at idle) and at its best.
    pub pcie_generation: MetricCell,
    pub pcie_width: MetricCell,
    pub pcie_generation_max: MetricCell,
    pub pcie_width_max: MetricCell,
    /// Deep data of the card the GPU view shows (D-48); `None` elsewhere,
    /// so the overview carries none of it.
    pub details: Option<Box<GpuDetails>>,
    /// The device was removed: its values are the last ones read and no
    /// longer refresh.
    pub removed: bool,
    pub metadata: Vec<(String, String)>,
    pub utilization_trend: Trend,
    pub temperature_trend: Trend,
    pub power_trend: Trend,
    /// Used framebuffer bytes, or on a unified-memory GPU the RAM its
    /// processes hold (the VRAM chart, D-40).
    pub memory_trend: Trend,
    /// The highest power draw the Overview History recorded over the last
    /// days, for a GPU without a power limit on the GPU view, whose power
    /// bar it scales (D-118); `None` elsewhere and until the history
    /// answers.
    pub recorded_power_peak: Option<f64>,
}

/// What the GPU view shows of one GPU beyond its card (D-48): its
/// capabilities, slow health, bandwidths, identity and software stack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuDetails {
    /// Per-direction capacity of the PCIe link now and at its best, from
    /// its generation and width (derived, theoretical).
    pub pcie_bandwidth: MetricCell,
    pub pcie_bandwidth_max: MetricCell,
    /// The driver's PCIe throughput in its own "KB/s" while that unit is
    /// not verified (D-11); `None` once the canonical value exists.
    pub pcie_tx_driver_kb: Option<f64>,
    pub pcie_rx_driver_kb: Option<f64>,
    pub pcie_replays: MetricCell,
    /// CUDA cores and the width of the memory bus in bits.
    pub cores: MetricCell,
    pub memory_bus_width: MetricCell,
    /// Peak device-memory bandwidth from the maximum memory clock and the
    /// bus width (derived, theoretical; never a measurement of traffic).
    pub memory_bandwidth_max: MetricCell,
    pub bar1_used: MetricCell,
    pub bar1_total: MetricCell,
    pub power_limit_default: MetricCell,
    pub power_limit_minimum: MetricCell,
    pub power_limit_maximum: MetricCell,
    /// The limit set for the GPU; other limiters may lower it to the
    /// enforced one (D-54).
    pub power_limit_requested: MetricCell,
    /// The driver's instantaneous reading beside the averaged draw (D-54).
    pub power_instant: MetricCell,
    /// Each fan's speed, in fan order (`fan 0`, `fan 1`, ...).
    pub fans: Vec<LabeledCell>,
    /// Hottest the GPU runs before it may drop below its base clock, and
    /// the temperature its fans aim for.
    pub temperature_max_operating: MetricCell,
    pub temperature_target: MetricCell,
    pub encoder_sessions: MetricCell,
    pub encoder_fps: MetricCell,
    pub encoder_latency: MetricCell,
    /// Time the clocks were held back since the driver loaded, per cause
    /// (`power`, `thermal`, ...).
    pub throttle_time: Vec<LabeledCell>,
    /// Identity, firmware and the driver and CUDA stack, by attribute key
    /// (`dgxtop_core::naming::attributes`).
    pub attributes: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuView {
    pub cards: Vec<GpuCard>,
    pub selected: Option<EntityId>,
    /// Why no GPU data is shown (disabled, NVML unavailable, quarantined ...).
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryView {
    pub total: MetricCell,
    pub available: MetricCell,
    pub used: MetricCell,
    pub buffers: MetricCell,
    pub cached: MetricCell,
    pub swap_total: MetricCell,
    pub swap_used: MetricCell,
    /// RAM held by GPU allocations on unified-memory GPUs (part of `used`);
    /// `None` when no GPU shares system RAM.
    pub gpu_resident: Option<MetricCell>,
    pub used_trend: Trend,
    /// What unified-memory GPUs held at each point of `used_trend`; empty
    /// when no GPU shares system RAM.
    pub gpu_resident_trend: Trend,
    /// What each tracked process holds of `gpu_resident` now, in slot order
    /// (D-64); the rest of `gpu_resident` is other GPU processes.
    pub gpu_processes: Vec<ProcessShare>,
    /// Each tracked process's part of the points of `used_trend`.
    pub gpu_process_trends: Vec<ProcessTrend>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskDeviceView {
    pub entity_id: EntityId,
    pub name: String,
    pub read_bps: MetricCell,
    pub write_bps: MetricCell,
    pub read_iops: MetricCell,
    pub write_iops: MetricCell,
    pub await_ms: MetricCell,
    /// The drive's temperature (D-92); a partition shows its disk's.
    pub temperature: MetricCell,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilesystemView {
    pub entity_id: EntityId,
    pub mount_point: String,
    pub device: String,
    /// The listed block device this filesystem lives on (same device
    /// number), so the disk table can show capacity beside its I/O.
    pub block_device: Option<EntityId>,
    pub fs_type: String,
    pub total: MetricCell,
    pub used: MetricCell,
    pub free: MetricCell,
    pub available: MetricCell,
    pub use_percent: MetricCell,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskView {
    pub devices: Vec<DiskDeviceView>,
    pub filesystems: Vec<FilesystemView>,
    /// Read rate trend of the first (busiest) device, for the panel chart.
    pub read_trend: Trend,
    /// Write rate trend of the first (busiest) device.
    pub write_trend: Trend,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterfaceView {
    pub entity_id: EntityId,
    pub name: String,
    pub state: Option<String>,
    pub rx_bps: MetricCell,
    pub tx_bps: MetricCell,
    pub rx_packets: MetricCell,
    pub tx_packets: MetricCell,
    pub rx_errors: MetricCell,
    pub tx_errors: MetricCell,
    pub rx_drops: MetricCell,
    pub tx_drops: MetricCell,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkView {
    pub interfaces: Vec<InterfaceView>,
    pub max_visible: u32,
    /// Receive rate trend of the first (busiest) interface, for the panel chart.
    pub rx_trend: Trend,
    /// Transmit rate trend of the first (busiest) interface.
    pub tx_trend: Trend,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessGpuView {
    pub gpu_entity_id: EntityId,
    pub gpu_label: String,
    /// Compute, graphics or both contexts on the GPU (D-54).
    pub usage: GpuUsageKind,
    pub utilization_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessRowView {
    pub key: ProcessKey,
    pub name: String,
    /// Numeric uid, or a cached user name when resolution already happened off the hot path.
    pub user: String,
    pub state: Option<char>,
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub missing_reason: Option<String>,
    pub gpus: Vec<ProcessGpuView>,
    /// The process's color slot when it is tracked (D-64), so the table
    /// names the colors of the memory bars and charts.
    pub color_slot: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessesView {
    /// Visible window (plus small overscan), not the full table.
    pub rows: Vec<ProcessRowView>,
    pub window_start: u32,
    pub total_rows: u32,
    pub truncated: bool,
    pub sort: SortKey,
    pub ascending: bool,
    pub filter: String,
    pub gpu_filter: Option<(EntityId, String)>,
    pub gpu_choices: Vec<(EntityId, String)>,
    pub selected: Option<ProcessKey>,
    pub selected_index: Option<u32>,
    /// The session's selected process is no longer in the process table (it
    /// exited): its values are unavailable (AT09). A selection hidden by a
    /// filter is not missing.
    pub selected_missing: Option<ProcessKey>,
}

/// One point of a history chart on the wall clock (D-51): this run's
/// points come from the boot clock, earlier runs' and boots' from the
/// archive's UTC stamps, so every boot shares the chart's time axis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryPointView {
    pub start_utc_ns: u64,
    pub end_utc_ns: u64,
    pub mean: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// Columns of the history view its panels do not take (the panel border)
/// (D-46).
pub const HISTORY_CHART_MARGIN_COLUMNS: u16 = 2;
/// Width inside the border from which the history view's panels stand two
/// side by side (D-50).
pub const HISTORY_TWO_COLUMN_WIDTH: u16 = 150;
/// Columns between two history panels side by side.
pub const HISTORY_PANEL_GAP: u16 = 3;
/// The label gutter at the left of a history chart (`GPU 0`, `100%`).
pub const HISTORY_GUTTER_COLUMNS: u16 = 7;
/// The legend at the right of a history chart (`max 98.0%`), with the space
/// before it.
pub const HISTORY_LEGEND_COLUMNS: u16 = 17;

/// Columns of a history chart's plot on a terminal `columns` wide (D-50):
/// the panel's width, half the inside when panels stand side by side, less
/// the gutter and the legend. Shared so the Manager queries as many points
/// as a plot has columns.
pub fn history_plot_columns(columns: u16) -> u16 {
    let inside = columns.saturating_sub(HISTORY_CHART_MARGIN_COLUMNS);
    let panel = if inside >= HISTORY_TWO_COLUMN_WIDTH {
        (inside - HISTORY_PANEL_GAP) / 2
    } else {
        inside
    };
    panel.saturating_sub(HISTORY_GUTTER_COLUMNS + HISTORY_LEGEND_COLUMNS)
}

/// What a history chart shows (D-46); the Viewer picks its words and colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryChartKind {
    CpuUtilization,
    MemoryUsed,
    GpuUtilization,
    DiskRead,
    DiskWrite,
    NetworkReceive,
    NetworkTransmit,
    /// Each GPU's memory, temperature and power: charted (D-50) and
    /// summarized over 1 h to 24 h (D-48).
    GpuMemory,
    GpuTemperature,
    GpuPower,
    /// What the CPU packages and the GPUs draw together, and each CPU
    /// package's power, charted beside the GPUs' power (D-106).
    TotalPower,
    CpuPower,
}

impl HistoryChartKind {
    /// Whether the chart draws a power, whose mean over the time it holds
    /// is an energy.
    pub const fn is_power(self) -> bool {
        matches!(self, Self::GpuPower | Self::TotalPower | Self::CpuPower)
    }
}

/// One chart of the history view: a series over the requested window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryChartView {
    pub kind: HistoryChartKind,
    /// The device charted (`GPU 0 · NVIDIA GB10`, `nvme0n1`); empty for the
    /// host's CPU and memory.
    pub subject: String,
    pub series: MetricKey,
    pub unit: CanonicalUnit,
    /// Fixed top of the scale (100 %, the RAM total); `None` scales the
    /// chart to its peak.
    pub scale_max: Option<f64>,
    pub points: Vec<HistoryPointView>,
    /// Time-weighted mean over the covered part of the window.
    pub mean: Option<f64>,
    /// Largest value in the window (from the bucket maxima when buckets
    /// merge several samples).
    pub peak: Option<f64>,
    /// Time the history holds in the window: the time its aligned buckets
    /// recorded (D-50, D-82), of the view's
    /// [`HistoryView::requested_span_ns`].
    pub covered_ns: Option<u64>,
    /// Set when points were merged to fit the chart's width: the mark of
    /// a display-only compression.
    pub display_downsampled: bool,
    pub pending: bool,
    pub error: Option<String>,
    /// The tracked processes' parts of a memory chart, in slot order
    /// (D-64); empty for other charts.
    pub parts: Vec<HistoryPart>,
}

/// A tracked process's part of a history chart (D-64): its mean in each
/// cell of an even grid over the view's window. A time it was not listed
/// counts as 0; `None` where no process list was recorded (before this run).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryPart {
    pub slot: u8,
    pub means: Vec<Option<f64>>,
}

/// The panels a frontend may fold away (D-114), saved in `viewer.folded`:
/// the Overview's disk I/O and network modules, then the Overview
/// History's panels.
pub const FOLDABLE_PANELS: [&str; 10] = [
    "disk",
    "network",
    "history.cpu",
    "history.memory",
    "history.gpu_utilization",
    "history.gpu_memory",
    "history.gpu_temperature",
    "history.power",
    "history.disk",
    "history.network",
];

/// Whether `panels` are distinct foldable panels (D-114).
pub fn are_foldable(panels: &[String]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    panels
        .iter()
        .all(|panel| FOLDABLE_PANELS.contains(&panel.as_str()) && seen.insert(panel.as_str()))
}

/// Rows of runs a Process History projection carries at most: the newest
/// its session's filter keeps (D-113).
pub const MAX_RUN_ROWS: usize = 200;
/// Bars a run chart carries at most (the widest plot).
pub const MAX_RUN_CHART_POINTS: usize = 1_000;

/// The Process History tab (D-76, D-77): GPU process runs, the newest
/// start first, and the run the session opened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessHistoryView {
    pub runs: Vec<RunRowView>,
    /// Runs the filter keeps beyond [`MAX_RUN_ROWS`].
    pub more_runs: usize,
    pub opened: Option<RunDetailView>,
    /// Runs outlive dgxtop (its stores are on).
    pub kept_on_disk: bool,
    /// Local time's offset from UTC, seconds east.
    pub utc_offset_s: i32,
    /// The bar size the session chose (`Auto` among them).
    pub bar: BarSize,
    /// The CPU's whole: 100 % for each of the host's logical CPUs.
    pub cpu_scale: Option<f64>,
}

/// One run in the list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRowView {
    pub key: ProcessKey,
    pub name: String,
    pub started_utc_ns: i64,
    pub runtime_ns: u64,
    pub live: bool,
    pub end: Option<RunEnd>,
    pub gpus: Vec<String>,
    pub peak_gpu_memory: Option<f64>,
    pub mean_gpu_utilization: Option<f64>,
    pub mean_cpu: Option<f64>,
    pub peak_memory: Option<f64>,
    /// The most descendants it had at once.
    pub peak_children: Option<f64>,
    /// Its color while it holds one (D-64).
    pub slot: Option<u8>,
}

/// The run opened: everything known of it, and its charts as bars of one
/// size (D-77), the latest bar at the right.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunDetailView {
    pub info: RunInfo,
    pub live: bool,
    pub slot: Option<u8>,
    /// The bar size drawn: the session's, or the one `Auto` chose.
    pub bar: BarSize,
    /// The time the charts span, UTC: as many bars as a plot has columns,
    /// ending with the bar of the run's latest sample.
    pub window_start_utc_ns: u64,
    pub window_end_utc_ns: u64,
    pub charts: Vec<RunChartView>,
    /// Its buckets are being read from the long-term store.
    pub loading: bool,
}

/// One chart of a run: a point per bar, and the run's mean and peak.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunChartView {
    pub metric: RunMetric,
    pub unit: CanonicalUnit,
    /// Top of the scale: the host's CPUs or RAM, the GPUs' memory or
    /// utilization; `None` scales the chart to what it draws.
    pub scale_max: Option<f64>,
    /// The bars with a sample, oldest first; a bar is the mean of its time.
    pub points: Vec<HistoryPointView>,
    /// Over the whole run.
    pub mean: Option<f64>,
    pub peak: Option<f64>,
    /// The latest bar's mean.
    pub latest: Option<f64>,
}

/// Where the history before this run comes from (D-51, D-82).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiskHistory {
    /// No archive: the history starts with dgxtop.
    Off,
    /// Earlier runs and boots are kept on disk in every bar size.
    On,
    /// The history's store could not keep or read them; why.
    Failed(String),
}

/// The history view (D-46): the overview's charts over a longer window, one
/// below the other on one time axis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryView {
    pub window_label: String,
    pub requested_span_ns: u64,
    /// The time axis every chart shares, on the wall clock (D-51):
    /// `[start_utc_ns, end_utc_ns)`, a bar for each plot column on the
    /// local clock, ending with the bar being filled now (D-82).
    pub start_utc_ns: u64,
    pub end_utc_ns: u64,
    /// Local time's offset from UTC, seconds east: the axis reads the
    /// local clock the bars follow (D-82).
    #[serde(default)]
    pub utc_offset_s: i32,
    pub charts: Vec<HistoryChartView>,
    pub disk: DiskHistory,
    /// Why there is nothing to chart (no history service, no series yet).
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    Starting,
    Healthy,
    Degraded,
    CircuitOpen,
    Quarantined,
    Disabled,
    Unavailable,
}

impl HostState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::CircuitOpen => "circuit open",
            Self::Quarantined => "quarantined (requires intervention)",
            Self::Disabled => "disabled",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Runtime status of one adapter host (also used by the supervisor port).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    pub adapter_id: String,
    pub state: HostState,
    pub generation: u32,
    pub pid: Option<i32>,
    pub last_success_boot_ns: Option<u64>,
    pub timeouts_total: u64,
    pub consecutive_failures: u32,
    pub restarts_total: u64,
    pub backoff_until_boot_ns: Option<u64>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntervalView {
    pub adapter_id: String,
    pub read_group: String,
    pub configured_ms: u32,
    pub effective_ms: u32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingField {
    pub path: String,
    pub saved: String,
    pub effective: String,
    pub origin: ValueOrigin,
    pub restart_required: bool,
    /// Overload multiplier note for sampling fields, if any.
    pub overload_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    pub revision: u32,
    pub pending: Option<String>,
    pub last_result: Option<String>,
    pub fields: Vec<SettingField>,
    pub modules: Vec<(ModuleId, bool)>,
    pub config_status: PersistenceStatus,
    /// Each adapter host's health, last success and backoff, for the
    /// Settings tab's data sources block (D-91).
    pub hosts: Vec<HostStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfirmationView {
    pub request_id: uuid::Uuid,
    pub target: ProcessKey,
    pub uid: u32,
    pub basename: String,
    pub signal: Signal,
    pub expires_in_ms: u64,
    pub warning: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionsView {
    pub confirmation: Option<ConfirmationView>,
    pub audit: PersistenceStatus,
    pub unknown_outcomes: u32,
}

/// Overload state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverloadState {
    Normal,
    Pressured,
    Throttled,
    Recovering,
}

impl OverloadState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Pressured => "pressured",
            Self::Throttled => "throttled",
            Self::Recovering => "recovering",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BannerLevel {
    Info,
    Warning,
    Error,
    /// Pending destructive confirmation: never clipped.
    Confirmation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Banner {
    pub level: BannerLevel,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeaderView {
    pub host_name: String,
    pub kernel: Option<String>,
    pub uptime_s: Option<u64>,
    pub version: String,
    pub namespace_label: Option<String>,
    pub overload: OverloadState,
    pub configured_interval_ms: u32,
    pub effective_interval_ms: u32,
    pub hosts_healthy: u32,
    pub hosts_total: u32,
    pub read_only: bool,
    pub theme: Theme,
    /// What the CPU packages and every GPU draw together
    /// (`host.power.total`, D-106); missing while any of them does not
    /// report, and then left out of the header.
    pub total_power: MetricCell,
}

/// Per-session view state; the single authoritative copy lives in the Manager.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: SessionId,
    pub view: ViewKind,
    pub selected_entity_id: Option<EntityId>,
    pub selected_process: Option<ProcessKey>,
    pub filter: String,
    pub sort: SortKey,
    pub ascending: bool,
    /// The bar sizes of the Overview History (never `Auto`) and of the
    /// Process History (D-78, D-80).
    #[serde(default = "BarSize::overview_default")]
    pub overview_bar: BarSize,
    #[serde(default)]
    pub process_bar: BarSize,
}

/// Module-level sub-projections (only subscribed/visible topics are filled).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModuleViews {
    pub cpu: Option<Arc<CpuView>>,
    pub gpu: Option<Arc<GpuView>>,
    pub memory: Option<Arc<MemoryView>>,
    pub disk: Option<Arc<DiskView>>,
    pub network: Option<Arc<NetworkView>>,
    pub processes: Option<Arc<ProcessesView>>,
    pub history: Option<Arc<HistoryView>>,
    /// Built beside `history`, for the Process History tab (D-76).
    pub process_history: Option<Arc<ProcessHistoryView>>,
    pub settings: Option<Arc<SettingsView>>,
    pub actions: Option<Arc<ActionsView>>,
}

/// One immutable projection for one session.
#[derive(Debug, Serialize)]
pub struct ViewModel {
    pub frame_revision: u64,
    pub snapshot_revision: u64,
    pub generated_boot_ns: u64,
    pub header: HeaderView,
    pub session: SessionState,
    pub modules: ModuleViews,
    pub banners: Vec<Banner>,
    pub enabled_modules: Vec<ModuleId>,
    /// The panels folded away, as saved (`viewer.folded`, D-114).
    #[serde(default)]
    pub folded: Vec<String>,
    /// Bytes accounted in the presentation partition while this projection lives.
    #[serde(skip)]
    pub permit: Option<BytePermit>,
}

impl ViewModel {
    /// The projection's exact JSON length (`usize::MAX` when it has none).
    pub fn estimated_bytes(&self) -> usize {
        let mut counter = ByteCounter(0);
        serde_json::to_writer(&mut counter, self).map_or(usize::MAX, |()| counter.0)
    }

    /// An upper bound on [`Self::estimated_bytes`] found without writing
    /// the JSON (see [`crate::json_size`]).
    pub fn bytes_bound(&self) -> usize {
        crate::json_size::json_length_bound(self).unwrap_or(usize::MAX)
    }

    /// The size a projection is accounted at against `max_bytes`: the
    /// bound when it fits, otherwise the exact length. Either way the
    /// result fits `max_bytes` exactly when the JSON does.
    pub fn accounted_bytes(&self, max_bytes: usize) -> usize {
        let bound = self.bytes_bound();
        if bound <= max_bytes {
            bound
        } else {
            self.estimated_bytes()
        }
    }
}

/// Counts what `serde_json` writes instead of keeping it.
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Latest-only projection slot: at most one pending projection per session;
/// the renderer holds the active one. Publishing replaces the pending one.
#[derive(Debug, Default)]
pub struct ProjectionSlot {
    pending: Mutex<Option<Arc<ViewModel>>>,
}

impl ProjectionSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the pending projection; the older pending one is dropped
    /// (its bytes are released when the last reference goes away).
    pub fn publish(&self, projection: Arc<ViewModel>) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        *pending = Some(projection);
    }

    /// Takes the pending projection, if any.
    pub fn take(&self) -> Option<Arc<ViewModel>> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    pub fn has_pending(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{ByteLedger, Partition};

    fn view_model(revision: u64, permit: Option<BytePermit>) -> ViewModel {
        ViewModel {
            frame_revision: revision,
            snapshot_revision: revision,
            generated_boot_ns: 0,
            header: HeaderView {
                host_name: "h".into(),
                kernel: None,
                uptime_s: None,
                version: "0".into(),
                namespace_label: None,
                overload: OverloadState::Normal,
                configured_interval_ms: 1000,
                effective_interval_ms: 1000,
                hosts_healthy: 0,
                hosts_total: 0,
                read_only: false,
                theme: Theme::Dark,
                total_power: MetricCell::missing(CanonicalUnit::Watts, CellState::Unavailable),
            },
            session: SessionState {
                session_id: SessionId::new("s").unwrap(),
                view: ViewKind::Overview,
                selected_entity_id: None,
                selected_process: None,
                filter: String::new(),
                sort: SortKey::GpuMemory,
                ascending: false,
                overview_bar: crate::tiers::BarSize::OneMinute,
                process_bar: crate::tiers::BarSize::Auto,
            },
            modules: ModuleViews::default(),
            banners: Vec::new(),
            enabled_modules: Vec::new(),
            folded: Vec::new(),
            permit,
        }
    }

    #[test]
    fn slot_keeps_only_latest_and_releases_bytes() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let slot = ProjectionSlot::new();
        for revision in 0..10u64 {
            let permit = ledger.try_reserve(Partition::Presentation, 1000).unwrap();
            slot.publish(Arc::new(view_model(revision, Some(permit))));
        }
        // Only the newest pending projection is alive.
        assert_eq!(ledger.used(Partition::Presentation), 1000);
        let active = slot.take().unwrap();
        assert_eq!(active.frame_revision, 9);
        assert!(slot.take().is_none());
        // A slow renderer still holding the active projection keeps it accounted.
        assert_eq!(ledger.used(Partition::Presentation), 1000);
        drop(active);
        assert_eq!(ledger.used(Partition::Presentation), 0);
    }

    #[test]
    fn missing_cells_have_no_value() {
        let cell = MetricCell::missing(CanonicalUnit::DegC, CellState::Unsupported);
        assert_eq!(cell.value_f64(), None);
        assert_eq!(cell.state.label(), "unsupported");
    }
}
