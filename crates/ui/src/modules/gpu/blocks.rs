//! What the GPU view says about one GPU (D-50), in blocks of `key value`
//! rows read top to bottom, so values compared sit one above the other
//! (transmit over receive, the thresholds in rising order) and a value now
//! sits beside its maximum:
//!
//! - State: whether the device was removed, the P-state, the throttle
//!   reasons now, how long each cause held the clocks back since the driver
//!   loaded.
//! - Utilization: the memory controller, the encoder and the decoder (the
//!   graphics engine is the status bar above).
//! - Clocks: graphics, SM, memory and video, now and at most.
//! - Memory: total, used, free, reserved, the processes' share, BAR1, the
//!   bus and its theoretical peak.
//! - PCIe: the link and its theoretical capacity now and at most, transmit
//!   over receive, replays.
//! - Thermal: temperature and fans, then the device's thresholds in rising
//!   order.
//! - Power: draw and the instantaneous reading, the limit (and the one
//!   requested when another limiter enforces a lower one), default, the
//!   range the limit may be set within, the energy since the driver loaded.
//! - CUDA: the driver's CUDA version, compute capability, toolkit, nvcc,
//!   other toolkits.
//! - Driver: version, kernel module, the host's Linux kernel, NVML,
//!   persistence, compute mode.
//! - Device: architecture, brand, CUDA cores, board part, serial, UUID,
//!   device node.
//! - PCI & firmware: bus id, device id, subsystem, VBIOS, InfoROM, GSP.
//! - Health: ECC, retired pages, NVLink, on the GPUs that keep them.
//! - Topology (D-54): the NUMA node and CPUs nearest the GPU, and the path
//!   and direct (P2P) access to each other GPU.
//!
//! A value the GPU does not report gets no row; a block without rows is
//! left out.

use dgxtop_core::gpu_state::{GpuLink, GpuPeer, PeerAccess, parse_peers};
use dgxtop_core::naming::attributes;
use dgxtop_core::view::{GpuCard, GpuDetails, MetricCell};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::facts::{attribute, exact_count, has_value, reported, throttle_style, throttle_text};
use crate::format::{
    CellDetail, clean, fixed, format_duration_ms, format_energy, format_percent, format_si_rate,
    value_text,
};
use crate::modules::common::{cell_spans, fan_speed_spans, temperature_spans};
use crate::num::saturate_u16;
use crate::registry::DisplayContext;

/// Throttle causes listed even at zero: the common ones.
const USUAL_THROTTLE_CAUSES: usize = 2;
/// Watts two power limits may differ by and still be one limit.
const SAME_LIMIT_WATTS: f64 = 0.0005;
/// Value columns of a table block.
const NOW_AND_MAX: [&str; 2] = ["now", "max"];

/// A value: its styled spans.
pub(super) type Value = Vec<Span<'static>>;

/// One row of a block: a key and its value, or a table row's values now
/// and at most.
#[derive(Debug, Clone)]
pub(super) struct BlockRow {
    pub(super) key: &'static str,
    pub(super) cells: Vec<Value>,
}

impl BlockRow {
    fn new(key: &'static str, value: Value) -> Self {
        Self {
            key,
            cells: vec![value],
        }
    }
}

/// A titled block of rows.
#[derive(Debug, Clone)]
pub(super) struct Block {
    pub(super) title: &'static str,
    /// Headings of a table's value columns.
    pub(super) headings: Option<[&'static str; 2]>,
    pub(super) rows: Vec<BlockRow>,
}

impl Block {
    /// Rows the block takes: its title and its rows.
    pub(super) fn height(&self) -> u16 {
        saturate_u16(self.rows.len() + 1)
    }
}

/// The blocks of `card`, in reading order; `kernel` is the host's kernel
/// release from the header, which the driver's kernel module is built for.
pub(super) fn blocks(card: &GpuCard, kernel: Option<&str>, context: &DisplayContext) -> Vec<Block> {
    let info = CardInfo {
        card,
        details: card.details.as_deref(),
        kernel,
        context,
    };
    [
        ("State", None, info.state()),
        ("Utilization", None, info.utilization()),
        ("Clocks", Some(NOW_AND_MAX), info.clocks()),
        ("Memory", None, info.memory()),
        ("PCIe", Some(NOW_AND_MAX), info.pcie()),
        ("NVLink-C2C", None, info.c2c()),
        ("Thermal", None, info.thermal()),
        ("Power", None, info.power()),
        ("CUDA", None, info.cuda()),
        ("Driver", None, info.driver()),
        ("Device", None, info.device()),
        ("PCI & firmware", None, info.pci_and_firmware()),
        ("Health", None, info.health()),
        ("Topology", None, info.topology()),
    ]
    .into_iter()
    .map(|(title, headings, rows)| Block {
        title,
        headings,
        rows: rows.into_iter().flatten().collect(),
    })
    .filter(|block| !block.rows.is_empty())
    .collect()
}

/// Plain text of a block, for accessibility labels.
pub(super) fn block_text(block: &Block) -> String {
    let rows: Vec<String> = block
        .rows
        .iter()
        .map(|row| {
            let cells: Vec<String> = row
                .cells
                .iter()
                .map(|cell| Line::from(cell.clone()).to_string())
                .collect();
            let joined = cells.join(" / ");
            if row.key.is_empty() {
                joined
            } else {
                format!("{} {joined}", row.key)
            }
        })
        .collect();
    format!("{}: {}", block.title, rows.join(", "))
}

/// Rows of a block, each present when the GPU reports its value.
type Rows = Vec<Option<BlockRow>>;

/// What the blocks are built from.
struct CardInfo<'a> {
    card: &'a GpuCard,
    details: Option<&'a GpuDetails>,
    /// The host's kernel release (`HeaderView::kernel`).
    kernel: Option<&'a str>,
    context: &'a DisplayContext,
}

impl CardInfo<'_> {
    /// A reported cell's value with its evidence (`warming up`, `stale`).
    fn value(&self, cell: &MetricCell) -> Option<Value> {
        reported(cell).then(|| cell_spans(cell, CellDetail::GLANCE, self.context))
    }

    fn metric(&self, key: &'static str, cell: &MetricCell) -> Option<BlockRow> {
        Some(BlockRow::new(key, self.value(cell)?))
    }

    /// A row of the GPU's temperature now, in its level color (D-104); its
    /// fixed thresholds stay plain.
    fn temperature(&self, key: &'static str, cell: &MetricCell) -> Option<BlockRow> {
        reported(cell).then(|| {
            BlockRow::new(
                key,
                temperature_spans(cell, CellDetail::GLANCE, self.context),
            )
        })
    }

    /// A fan's speed in its level color (D-126), when the GPU reports it.
    fn fan_speed(&self, cell: &MetricCell) -> Option<Value> {
        reported(cell).then(|| fan_speed_spans(cell, CellDetail::GLANCE, self.context))
    }

    /// A row of the card's details.
    fn detail(
        &self,
        key: &'static str,
        pick: impl FnOnce(&GpuDetails) -> &MetricCell,
    ) -> Option<BlockRow> {
        self.metric(key, pick(self.details?))
    }

    /// A driver attribute, as text.
    fn attribute(&self, key: &'static str, name: &str) -> Option<BlockRow> {
        Some(BlockRow::new(
            key,
            vec![self.text(attribute(self.card, name)?)],
        ))
    }

    fn text(&self, value: impl Into<String>) -> Span<'static> {
        Span::styled(value.into(), self.context.theme.text_style())
    }

    fn muted(&self, value: impl Into<String>) -> Span<'static> {
        Span::styled(value.into(), self.context.theme.muted_style())
    }

    /// Whether the device was removed, the P-state, the throttle reasons,
    /// the throttle time per cause.
    fn state(&self) -> Rows {
        let mut rows = vec![self.removed(), self.performance_state(), self.throttle()];
        rows.extend(self.throttle_time().into_iter().map(Some));
        rows
    }

    /// A removed device keeps its last values, which no longer refresh.
    fn removed(&self) -> Option<BlockRow> {
        self.card.removed.then(|| {
            BlockRow::new(
                "device",
                vec![Span::styled(
                    "removed · values no longer refresh",
                    Style::new().fg(self.context.theme.warning),
                )],
            )
        })
    }

    /// `P8` (P0 is the fastest state).
    fn performance_state(&self) -> Option<BlockRow> {
        let cell = &self.card.performance_state;
        if !has_value(cell) {
            return self.metric("P-state", cell);
        }
        Some(BlockRow::new(
            "P-state",
            vec![self.text(format!("P{}", value_text(cell)))],
        ))
    }

    /// The reasons the driver holds the clocks back now, in the color of
    /// the worst.
    fn throttle(&self) -> Option<BlockRow> {
        let cell = &self.card.throttle_reasons;
        let Some(bits) = exact_count(cell) else {
            return self.metric("throttle", cell);
        };
        Some(BlockRow::new(
            "throttle",
            vec![Span::styled(
                throttle_text(bits),
                throttle_style(bits, self.context),
            )],
        ))
    }

    /// `throttled for  power 3.2 s` with each further cause under it:
    /// power and thermal always, the rarer causes once they did.
    fn throttle_time(&self) -> Vec<BlockRow> {
        let Some(details) = self.details else {
            return Vec::new();
        };
        details
            .throttle_time
            .iter()
            .enumerate()
            .filter_map(|(position, cause)| {
                let time = cause.cell.value_f64().filter(|_| has_value(&cause.cell))?;
                (position < USUAL_THROTTLE_CAUSES || time > 0.0).then(|| {
                    vec![
                        self.muted(format!("{} ", cause.label)),
                        self.text(format_duration_ms(time)),
                    ]
                })
            })
            .enumerate()
            .map(|(index, value)| {
                BlockRow::new(if index == 0 { "throttled for" } else { "" }, value)
            })
            .collect()
    }

    /// The memory controller's and the video engines' busy shares.
    fn utilization(&self) -> Rows {
        let card = self.card;
        vec![
            self.metric("memory busy", &card.memory_busy),
            self.metric("encoder", &card.encoder),
            self.metric("decoder", &card.decoder),
            self.encoder_sessions(),
        ]
    }

    /// `1 · 60 fps · 2.5 ms` while the encoder has sessions.
    fn encoder_sessions(&self) -> Option<BlockRow> {
        let details = self.details?;
        let sessions = details
            .encoder_sessions
            .value_f64()
            .filter(|count| *count > 0.0)?;
        let mut parts = vec![fixed(sessions, 0)];
        if let Some(fps) = details.encoder_fps.value_f64() {
            parts.push(format!("{} fps", fixed(fps, 0)));
        }
        if has_value(&details.encoder_latency) {
            parts.push(value_text(&details.encoder_latency));
        }
        Some(BlockRow::new(
            "encoder sessions",
            vec![self.text(parts.join(" · "))],
        ))
    }

    /// Each clock now and, when reported, at most.
    fn clocks(&self) -> Rows {
        [
            ("graphics", "graphics"),
            ("SM", "sm"),
            ("memory", "memory"),
            ("video", "video"),
        ]
        .into_iter()
        .map(|(key, domain)| self.clock(key, domain))
        .collect()
    }

    fn clock(&self, key: &'static str, domain: &str) -> Option<BlockRow> {
        let clocks = &self.card.clocks;
        let current = &clocks.iter().find(|clock| clock.label == domain)?.cell;
        let maximum_label = format!("{domain} max");
        let maximum = clocks
            .iter()
            .find(|clock| clock.label == maximum_label)
            .map(|clock| &clock.cell)
            .filter(|cell| has_value(cell));
        let mut cells = vec![self.value(current)?];
        if let Some(maximum) = maximum {
            cells.push(vec![self.text(value_text(maximum))]);
        }
        Some(BlockRow { key, cells })
    }

    /// The framebuffer's use (on unified memory, what the processes hold),
    /// BAR1, the bus.
    fn memory(&self) -> Rows {
        let card = self.card;
        let mut rows = if card.unified_memory {
            vec![
                Some(BlockRow::new(
                    "memory",
                    vec![self.text("unified with system RAM")],
                )),
                self.metric("processes", &card.process_allocated),
            ]
        } else {
            vec![
                self.metric("total", &card.framebuffer_total),
                self.used_memory(),
                self.metric("free", &card.framebuffer_free),
                self.metric("reserved", &card.framebuffer_reserved),
                self.metric("processes", &card.process_allocated),
            ]
        };
        rows.extend([self.bar1(), self.bus_width(), self.peak_bandwidth()]);
        rows
    }

    /// `1.2 GiB · 7.5%`.
    fn used_memory(&self) -> Option<BlockRow> {
        let card = self.card;
        let mut row = self.metric("used", &card.framebuffer_used)?;
        let used = card.framebuffer_used.value_f64();
        let total = card
            .framebuffer_total
            .value_f64()
            .filter(|total| has_value(&card.framebuffer_total) && *total > 0.0);
        if let (Some(used), Some(total), Some(value)) = (used, total, row.cells.first_mut()) {
            value.push(self.muted(" · "));
            value.push(self.text(format_percent(used / total * 100.0)));
        }
        Some(row)
    }

    /// `6.1 MiB of 16.0 GiB`: the BAR1 aperture in use.
    fn bar1(&self) -> Option<BlockRow> {
        let details = self.details?;
        let mut row = self.metric("BAR1", &details.bar1_used)?;
        if has_value(&details.bar1_used)
            && has_value(&details.bar1_total)
            && let Some(value) = row.cells.first_mut()
        {
            value.push(self.muted(" of "));
            value.push(self.text(value_text(&details.bar1_total)));
        }
        Some(row)
    }

    /// `256-bit`.
    fn bus_width(&self) -> Option<BlockRow> {
        let width = &self.details?.memory_bus_width;
        has_value(width).then(|| {
            BlockRow::new(
                "bus width",
                vec![self.text(format!("{}-bit", value_text(width)))],
            )
        })
    }

    /// `960 GB/s theoretical`: twice the memory clock's maximum times the
    /// bus width.
    fn peak_bandwidth(&self) -> Option<BlockRow> {
        let peak = &self.details?.memory_bandwidth_max;
        let rate = peak.value_f64().filter(|_| has_value(peak))?;
        Some(BlockRow::new(
            "peak bandwidth",
            vec![self.text(format_si_rate(rate)), self.muted(" theoretical")],
        ))
    }

    /// The link and its capacity now and at most, then the traffic and
    /// the replays.
    fn pcie(&self) -> Rows {
        let card = self.card;
        let details = self.details;
        let link = |generation: &MetricCell, width: &MetricCell| {
            (has_value(generation) && has_value(width)).then(|| {
                vec![self.text(format!(
                    "Gen{} x{}",
                    value_text(generation),
                    value_text(width)
                ))]
            })
        };
        let capacity = |cell: Option<&MetricCell>| {
            let rate = cell
                .filter(|cell| has_value(cell))
                .and_then(MetricCell::value_f64)?;
            Some(vec![self.text(format_si_rate(rate))])
        };
        vec![
            table_row(
                "link",
                link(&card.pcie_generation, &card.pcie_width),
                link(&card.pcie_generation_max, &card.pcie_width_max),
            ),
            table_row(
                "bandwidth",
                capacity(details.map(|details| &details.pcie_bandwidth)),
                capacity(details.map(|details| &details.pcie_bandwidth_max)),
            ),
            self.traffic(
                "transmit",
                &card.pcie_tx,
                details.and_then(|details| details.pcie_tx_driver_kb),
            ),
            self.traffic(
                "receive",
                &card.pcie_rx,
                details.and_then(|details| details.pcie_rx_driver_kb),
            ),
            self.detail("replays", |details| &details.pcie_replays),
        ]
    }

    /// The traffic in bytes per second or, while that unit is unverified,
    /// the driver's own `KB/s` (a note says why).
    fn traffic(
        &self,
        key: &'static str,
        cell: &MetricCell,
        driver_kilobytes: Option<f64>,
    ) -> Option<BlockRow> {
        self.metric(key, cell).or_else(|| {
            let kilobytes = driver_kilobytes?;
            Some(BlockRow::new(
                key,
                vec![self.text(format!("{} KB/s", fixed(kilobytes, 0)))],
            ))
        })
    }

    /// Temperature and fan, then the thresholds in rising order.
    fn thermal(&self) -> Rows {
        let card = self.card;
        let details = self.details;
        let mut thresholds: Vec<(&'static str, &MetricCell)> = vec![
            ("slowdown", &card.temperature_slowdown),
            ("shutdown", &card.temperature_shutdown),
        ];
        if let Some(details) = details {
            thresholds.push(("max operating", &details.temperature_max_operating));
            thresholds.push(("target", &details.temperature_target));
        }
        thresholds.sort_by(|(_, left), (_, right)| {
            let rank = |cell: &MetricCell| cell.value_f64().unwrap_or(f64::INFINITY);
            rank(left).total_cmp(&rank(right))
        });
        let mut rows = vec![
            self.temperature("temperature", &card.temperature),
            self.fans(),
        ];
        rows.extend(
            thresholds
                .into_iter()
                .map(|(key, cell)| self.metric(key, cell)),
        );
        rows
    }

    /// `45.0% · 47.0%`: each fan of a GPU with several, else its one fan,
    /// each in its level color (D-126).
    fn fans(&self) -> Option<BlockRow> {
        let fans = self
            .details
            .map_or(&[][..], |details| details.fans.as_slice());
        let speeds: Vec<Value> = fans
            .iter()
            .filter_map(|fan| self.fan_speed(&fan.cell))
            .collect();
        if speeds.len() < 2 {
            return self
                .fan_speed(&self.card.fan)
                .map(|speed| BlockRow::new("fan", speed));
        }
        let mut value = Vec::new();
        for (position, speed) in speeds.into_iter().enumerate() {
            if position > 0 {
                value.push(self.muted(" · "));
            }
            value.extend(speed);
        }
        Some(BlockRow::new("fans", value))
    }

    /// Draw and the instantaneous reading, the limit (and the requested
    /// one when it differs), its default and range, the energy.
    fn power(&self) -> Rows {
        let card = self.card;
        vec![
            self.metric("draw", &card.power_draw),
            self.detail("instant", |details| &details.power_instant),
            self.metric("limit", &card.power_limit),
            self.requested_limit(),
            self.detail("default limit", |details| &details.power_limit_default),
            self.power_range(),
            self.energy(),
        ]
    }

    /// The limit set for the GPU, when another limiter enforces a lower one
    /// (or the enforced one is unknown).
    fn requested_limit(&self) -> Option<BlockRow> {
        let requested = &self.details?.power_limit_requested;
        let enforced = &self.card.power_limit;
        let same = has_value(requested)
            && has_value(enforced)
            && requested.value_f64().zip(enforced.value_f64()).is_some_and(
                |(requested, enforced)| (requested - enforced).abs() < SAME_LIMIT_WATTS,
            );
        if same {
            return None;
        }
        self.metric("requested limit", requested)
    }

    /// `300 W–390 W`: the range the power limit can be set within.
    fn power_range(&self) -> Option<BlockRow> {
        let details = self.details?;
        let (minimum, maximum) = (&details.power_limit_minimum, &details.power_limit_maximum);
        (has_value(minimum) && has_value(maximum)).then(|| {
            BlockRow::new(
                "limit range",
                vec![self.text(format!("{}–{}", value_text(minimum), value_text(maximum)))],
            )
        })
    }

    /// `47.4 Wh since driver load`.
    fn energy(&self) -> Option<BlockRow> {
        let cell = &self.card.energy;
        let Some(joules) = cell.value_f64().filter(|_| has_value(cell)) else {
            return self.metric("energy", cell);
        };
        Some(BlockRow::new(
            "energy",
            vec![
                self.text(format_energy(joules)),
                self.muted(" since driver load"),
            ],
        ))
    }

    /// The CUDA stack.
    fn cuda(&self) -> Rows {
        let toolkit = attribute(self.card, attributes::CUDA_TOOLKIT);
        vec![
            self.attribute("version", attributes::CUDA_DRIVER_VERSION),
            self.compute_capability(),
            self.toolkit(toolkit.as_deref()),
            self.nvcc(toolkit.is_some()),
            self.other_toolkits(toolkit.as_deref()),
        ]
    }

    /// `12.0 · sm_120`: the compute capability and the target nvcc builds
    /// for it.
    fn compute_capability(&self) -> Option<BlockRow> {
        let capability = attribute(self.card, attributes::COMPUTE_CAPABILITY)?;
        let mut value = vec![self.text(capability.clone())];
        if let Some(target) = sm_target(&capability) {
            value.push(self.muted(" · "));
            value.push(self.text(target));
        }
        Some(BlockRow::new("compute capability", value))
    }

    /// `13.4 /usr/local/cuda-13.4`: the default toolkit and where it is.
    fn toolkit(&self, version: Option<&str>) -> Option<BlockRow> {
        let mut value = vec![self.text(version?)];
        if let Some(path) = attribute(self.card, attributes::CUDA_TOOLKIT_PATH) {
            value.push(self.muted(format!(" {path}")));
        }
        Some(BlockRow::new("toolkit", value))
    }

    /// nvcc's version, or `not installed` beside a toolkit without one.
    fn nvcc(&self, has_toolkit: bool) -> Option<BlockRow> {
        match attribute(self.card, attributes::NVCC_VERSION) {
            Some(version) => Some(BlockRow::new("nvcc", vec![self.text(version)])),
            None => has_toolkit.then(|| BlockRow::new("nvcc", vec![self.muted("not installed")])),
        }
    }

    /// Toolkits besides the default one.
    fn other_toolkits(&self, default: Option<&str>) -> Option<BlockRow> {
        let all = attribute(self.card, attributes::CUDA_TOOLKITS)?;
        let others: Vec<&str> = all
            .split(", ")
            .filter(|version| Some(*version) != default)
            .collect();
        (!others.is_empty())
            .then(|| BlockRow::new("other toolkits", vec![self.text(others.join(", "))]))
    }

    /// The driver and its modes.
    fn driver(&self) -> Rows {
        vec![
            self.attribute("version", attributes::DRIVER_VERSION),
            self.attribute("kernel module", attributes::KERNEL_MODULE),
            self.kernel
                .map(|kernel| BlockRow::new("Linux kernel", vec![self.text(clean(kernel))])),
            self.attribute("NVML", attributes::NVML_VERSION),
            self.attribute("persistence", attributes::PERSISTENCE_MODE),
            self.attribute("compute mode", attributes::COMPUTE_MODE),
        ]
    }

    /// What the GPU is and how the system names it.
    fn device(&self) -> Rows {
        let uuid = self
            .card
            .uuid
            .as_deref()
            .map(clean)
            .or_else(|| attribute(self.card, attributes::UUID));
        vec![
            self.attribute("architecture", attributes::ARCHITECTURE),
            self.attribute("brand", attributes::BRAND),
            self.detail("CUDA cores", |details| &details.cores),
            self.attribute("board part", attributes::BOARD_PART_NUMBER),
            self.attribute("serial", attributes::SERIAL),
            uuid.map(|uuid| BlockRow::new("UUID", vec![self.text(uuid)])),
            attribute(self.card, attributes::MINOR_NUMBER).map(|minor| {
                BlockRow::new(
                    "device node",
                    vec![self.text(format!("/dev/nvidia{minor}"))],
                )
            }),
        ]
    }

    /// Where the GPU sits on the bus and its firmware.
    fn pci_and_firmware(&self) -> Rows {
        vec![
            self.attribute("bus id", attributes::PCI_BUS_ID),
            self.attribute("device id", attributes::PCI_DEVICE_ID),
            self.attribute("subsystem", attributes::PCI_SUBSYSTEM_ID),
            self.attribute("VBIOS", attributes::VBIOS_VERSION),
            self.attribute("InfoROM", attributes::INFOROM_IMAGE),
            self.attribute("GSP firmware", attributes::GSP_FIRMWARE),
        ]
    }

    /// The NUMA node and CPUs nearest the GPU, then each other GPU.
    /// The chip-to-chip links to the CPU that carry the data of a GPU like
    /// GB10 (D-56); the bandwidth is the first link's.
    fn c2c(&self) -> Rows {
        let bandwidth = attribute(self.card, attributes::C2C_LINK_BANDWIDTH)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|rate| rate.is_finite() && *rate > 0.0)
            .map(|rate| {
                BlockRow::new(
                    "bandwidth",
                    vec![self.text(format_si_rate(rate)), self.muted(" per link")],
                )
            });
        vec![self.attribute("links", attributes::C2C_LINKS), bandwidth]
    }

    fn topology(&self) -> Rows {
        let mut rows = vec![
            self.attribute("NUMA node", attributes::NUMA_NODES),
            self.attribute("CPU affinity", attributes::CPU_AFFINITY),
        ];
        let peers = attribute(self.card, attributes::GPU_PEERS)
            .map(|peers| parse_peers(&peers))
            .unwrap_or_default();
        rows.extend(peers.iter().enumerate().map(|(position, peer)| {
            let key = if position == 0 { "peers" } else { "" };
            Some(BlockRow::new(key, self.peer(peer)))
        }));
        rows
    }

    /// `GPU 1 · host bridge (PHB) · no P2P`, the peer in its GPU's color
    /// (D-123); the selector above names each GPU's model (D-122).
    fn peer(&self, peer: &GpuPeer) -> Value {
        let path = peer.link.map_or_else(
            || "path unknown".to_owned(),
            |link: GpuLink| format!("{} ({})", link.label(), link.code()),
        );
        let access = peer.access.map_or("P2P unknown", PeerAccess::label);
        let theme = &self.context.theme;
        let style = theme.text_style().fg(theme.gpu_color(peer.index));
        vec![
            Span::styled(format!("GPU {}", peer.index), style),
            self.muted(" · "),
            self.text(path),
            self.muted(" · "),
            self.text(access),
        ]
    }

    /// ECC errors, retired pages and NVLink links.
    fn health(&self) -> Rows {
        let card = self.card;
        vec![
            self.metric("ECC corrected", &card.ecc_corrected),
            self.metric("ECC uncorrected", &card.ecc_uncorrected),
            self.metric("retired SBE", &card.retired_single),
            self.metric("retired DBE", &card.retired_double),
            self.metric("NVLink links", &card.nvlink_active),
        ]
    }
}

/// A table row: the value now and the maximum, when the value now is known.
fn table_row(key: &'static str, now: Option<Value>, max: Option<Value>) -> Option<BlockRow> {
    let mut cells = vec![now?];
    cells.extend(max);
    Some(BlockRow { key, cells })
}

/// `sm_120` of compute capability `12.0`.
fn sm_target(capability: &str) -> Option<String> {
    let (major, minor) = capability.split_once('.')?;
    let major: u32 = major.parse().ok()?;
    let minor: u32 = minor.parse().ok()?;
    Some(format!("sm_{major}{minor}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{display_context, exact, gpu_view, missing, ok};
    use dgxtop_core::metric::CanonicalUnit;
    use dgxtop_core::view::{CellState, LabeledCell};

    fn texts(card: &GpuCard) -> Vec<String> {
        blocks(card, None, &display_context())
            .iter()
            .map(block_text)
            .collect()
    }

    fn block<'a>(texts: &'a [String], title: &str) -> &'a str {
        texts
            .iter()
            .find(|text| text.starts_with(&format!("{title}: ")))
            .map_or("", String::as_str)
    }

    /// The Thermal block writes the temperature now in its level color
    /// (D-104) and the device's fixed thresholds plainly.
    #[test]
    fn the_temperature_now_takes_its_level_color_and_thresholds_stay_plain() {
        let context = display_context();
        let card = gpu_view().cards.remove(0);
        let shown = blocks(&card, None, &context);
        let thermal = shown.iter().find(|block| block.title == "Thermal").unwrap();
        let style_of = |key: &str| {
            thermal
                .rows
                .iter()
                .find(|row| row.key == key)
                .map(|row| row.cells[0][0].style)
                .unwrap()
        };
        assert_eq!(
            style_of("temperature").fg,
            Some(context.theme.temperature_color(71.0))
        );
        assert_eq!(style_of("slowdown"), context.theme.text_style());
    }

    #[test]
    fn a_removed_gpu_says_so_first_in_its_state() {
        let mut card = gpu_view().cards.remove(0);
        assert!(block(&texts(&card), "State").starts_with("State: P-state P2"));
        card.removed = true;
        assert!(
            block(&texts(&card), "State")
                .starts_with("State: device removed · values no longer refresh, P-state P2"),
            "{:#?}",
            texts(&card)
        );
    }

    #[test]
    fn a_unified_gpu_shows_the_ram_it_holds_and_the_driver_traffic() {
        let card = gpu_view().cards.remove(1);
        let texts = texts(&card);
        assert!(
            block(&texts, "Memory")
                .starts_with("Memory: memory unified with system RAM, processes 10.0 GiB, BAR1"),
            "{texts:#?}"
        );
        assert!(
            block(&texts, "PCIe").contains("transmit 2243 KB/s, receive 1767 KB/s"),
            "{texts:#?}"
        );
    }

    /// GB10 moves its data over NVLink-C2C, not its PCIe link: both are
    /// shown, the C2C links in a block of their own after the PCIe block.
    #[test]
    fn a_gpu_with_chip_to_chip_links_shows_them_beside_pcie() {
        let mut card = gpu_view().cards.remove(1);
        let details = card.details.as_deref_mut().unwrap();
        details.attributes.extend([
            ("c2c_links".to_owned(), "8".to_owned()),
            ("c2c_link_bandwidth".to_owned(), "43125000000".to_owned()),
        ]);
        let shown = texts(&card);
        assert_eq!(
            block(&shown, "NVLink-C2C"),
            "NVLink-C2C: links 8, bandwidth 43.1 GB/s per link"
        );
        let titles: Vec<&str> = shown
            .iter()
            .map(|text| text.split(':').next().unwrap_or_default())
            .collect();
        let pcie = titles.iter().position(|title| *title == "PCIe").unwrap();
        assert_eq!(titles.get(pcie + 1), Some(&"NVLink-C2C"));
        // A GPU without C2C links has no such block.
        let discrete = texts(&gpu_view().cards.remove(0));
        assert!(
            !discrete.iter().any(|text| text.starts_with("NVLink-C2C")),
            "{discrete:#?}"
        );
    }

    #[test]
    fn power_fans_and_topology_add_rows_only_with_something_to_say() {
        let mut card = gpu_view().cards.remove(0);
        let shown = texts(&card);
        assert!(
            block(&shown, "Power")
                .starts_with("Power: draw 120 W, instant 123 W, limit 250 W, default"),
            "{shown:#?}"
        );
        assert_eq!(
            block(&shown, "Topology"),
            "Topology: NUMA node 0, CPU affinity 0-31, peers GPU 1 · host bridge (PHB) · no P2P"
        );
        // The peer is in its GPU's color (D-123).
        let context = display_context();
        let peer = blocks(&card, None, &context)
            .into_iter()
            .flat_map(|block| block.rows)
            .flat_map(|row| row.cells)
            .flatten()
            .find(|span| span.content == "GPU 1")
            .unwrap();
        assert_eq!(peer.style.fg, Some(context.theme.gpu_color(1)));
        let details = card.details.as_deref_mut().unwrap();
        // A lower enforced limit shows the requested one beside it.
        details.power_limit_requested = ok(300.0, CanonicalUnit::Watts);
        details.fans = [45.0, 47.0]
            .into_iter()
            .enumerate()
            .map(|(fan, speed)| LabeledCell {
                label: format!("fan {fan}"),
                cell: ok(speed, CanonicalUnit::Percent),
            })
            .collect();
        details.attributes.retain(|(key, _)| key != "gpu_peers");
        details
            .attributes
            .push(("gpu_peers".into(), "1:?:rw 2:SYS:? 3".into()));
        let shown = texts(&card);
        assert!(
            block(&shown, "Power").contains("limit 250 W, requested limit 300 W, default"),
            "{shown:#?}"
        );
        assert!(
            block(&shown, "Thermal").starts_with("Thermal: temperature 71°C, fans 45.0% · 47.0%"),
            "{shown:#?}"
        );
        // Each fan in its level color: green up to 50 % (D-126).
        let speeds: Vec<_> = blocks(&card, None, &context)
            .into_iter()
            .flat_map(|block| block.rows)
            .filter(|row| row.key == "fans")
            .flat_map(|row| row.cells)
            .flatten()
            .filter(|span| span.content.ends_with('%'))
            .map(|span| span.style.fg)
            .collect();
        assert_eq!(speeds, vec![Some(context.theme.gauge_low); 2]);
        // What the driver could not name says so; a cut entry is left out.
        assert!(
            block(&shown, "Topology").ends_with(
                "peers GPU 1 · path unknown · P2P read/write, GPU 2 · across NUMA nodes (SYS) · P2P unknown"
            ),
            "{shown:#?}"
        );
    }

    #[test]
    fn unreported_values_get_no_row_and_empty_blocks_are_left_out() {
        let mut card = gpu_view().cards.remove(0);
        for cell in [
            &mut card.ecc_corrected,
            &mut card.ecc_uncorrected,
            &mut card.retired_single,
            &mut card.retired_double,
            &mut card.nvlink_active,
        ] {
            *cell = missing(CanonicalUnit::Count, CellState::Unsupported);
        }
        let details = card.details.as_deref_mut().unwrap();
        details.encoder_sessions = exact(1, CanonicalUnit::Count);
        details.encoder_fps = ok(59.6, CanonicalUnit::CountPerSecond);
        details.encoder_latency = ok(2.5, CanonicalUnit::Milliseconds);
        details.throttle_time[3].cell = ok(5_000.0, CanonicalUnit::Milliseconds);
        details.temperature_target = ok(83.0, CanonicalUnit::DegC);
        details.attributes.retain(|(key, _)| key != "nvcc_version");
        let shown = texts(&card);
        assert!(block(&shown, "Health").is_empty(), "{shown:#?}");
        assert!(
            block(&shown, "State").ends_with("thermal 0 ms, board limit 5.0 s"),
            "{shown:#?}"
        );
        assert!(
            block(&shown, "Utilization").ends_with("encoder sessions 1 · 60 fps · 2.5 ms"),
            "{shown:#?}"
        );
        // The target joins the thresholds in its place.
        assert!(
            block(&shown, "Thermal").contains("target 83°C, max operating 88°C, slowdown 90°C"),
            "{shown:#?}"
        );
        assert!(!block(&shown, "Thermal").contains("fan"), "{shown:#?}");
        assert!(
            block(&shown, "CUDA").contains("nvcc not installed"),
            "{shown:#?}"
        );
        // Without details only the card's own values remain.
        card.details = None;
        let bare = texts(&card);
        assert!(!block(&bare, "PCIe").contains("replays"), "{bare:#?}");
        assert!(
            block(&bare, "Device").contains("UUID GPU-0-uuid"),
            "{bare:#?}"
        );
    }

    /// The driver block names the host's kernel next to the kernel module
    /// (D-91); without a kernel release the row is left out.
    #[test]
    fn the_driver_block_shows_the_host_kernel() {
        let card = gpu_view().cards.remove(0);
        let with_kernel: Vec<String> = blocks(&card, Some("6.12.0-acme"), &display_context())
            .iter()
            .map(block_text)
            .collect();
        let driver = block(&with_kernel, "Driver");
        assert!(driver.contains("Linux kernel 6.12.0-acme"), "{driver}");
        let module = driver.find("kernel module").unwrap();
        assert!(module < driver.find("Linux kernel").unwrap(), "{driver}");
        assert!(!block(&texts(&card), "Driver").contains("Linux kernel"));
        let hostile: Vec<String> = blocks(&card, Some("6.1\u{1b}[31m"), &display_context())
            .iter()
            .map(block_text)
            .collect();
        assert!(block(&hostile, "Driver").contains("Linux kernel 6.1"));
        assert!(!block(&hostile, "Driver").contains('\u{1b}'));
    }
}
