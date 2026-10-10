//! The bars of a GPU (D-43, D-49, D-50), each a label, a gauge of the value
//! now and the value right after it: utilization against 100 %, the memory
//! in use (`VRAM` against the framebuffer, or on a unified-memory GPU `RAM`,
//! what its processes hold, against the system RAM), the temperature on a
//! fixed 0–100 °C scale and the power against the enforced limit. The
//! overview shows the first two, the utilization's history under its
//! gauge; the GPU view shows all four, live only.

use dgxtop_core::view::{GpuCard, MetricCell, Trend};
use ratatui::Frame;
use ratatui::layout::Rect;

use super::facts::{has_value, reported};
use crate::format::{display_width, format_value, pad_left, value_text};
use crate::modules::common::{BarRow, RowCursor, render_bar_row, render_chart, under_bar};
use crate::registry::DisplayContext;
use crate::widgets::Gauge;

/// Fixed top of the temperature scale, so small changes stay small.
const TEMPERATURE_SCALE_C: f64 = 100.0;
/// Bars of a GPU in the overview: utilization and memory.
pub(super) const OVERVIEW_READINGS: usize = 2;

/// One bar of a GPU.
pub(super) struct GpuReading<'a> {
    pub(super) label: &'static str,
    /// The value now, which fills the bar.
    current: &'a MetricCell,
    /// `14.0%`, `1.2 GiB / 15.9 GiB`: the value as the bar shows it.
    pub(super) value: String,
    /// What the chart under the bar draws.
    trend: &'a Trend,
    /// Top of the bar's and the chart's scale.
    max: Option<f64>,
}

impl GpuReading<'_> {
    /// Share of the scale now; `None` without a value or a scale.
    pub(super) fn ratio(&self) -> Option<f64> {
        let max = self.max.filter(|max| *max > 0.0)?;
        Some(self.current.value_f64()? / max)
    }

    /// Whether the GPU reports the value or its history has a point.
    fn is_known(&self) -> bool {
        reported(self.current)
            || self
                .trend
                .points
                .iter()
                .flatten()
                .any(|value| value.is_finite())
    }
}

/// The overview's bars: utilization and memory.
pub(super) fn overview_readings<'a>(card: &'a GpuCard) -> [GpuReading<'a>; OVERVIEW_READINGS] {
    [utilization(card), memory(card)]
}

/// The GPU view's status: utilization, memory, temperature and power; a
/// bar the GPU neither reports nor charted is left out.
pub(super) fn status_readings<'a>(card: &'a GpuCard) -> Vec<GpuReading<'a>> {
    [
        utilization(card),
        memory(card),
        temperature(card),
        power(card),
    ]
    .into_iter()
    .filter(GpuReading::is_known)
    .collect()
}

fn utilization(card: &GpuCard) -> GpuReading<'_> {
    GpuReading {
        label: "util",
        current: &card.utilization,
        value: value_text(&card.utilization),
        trend: &card.utilization_trend,
        max: Some(100.0),
    }
}

/// `VRAM` against the framebuffer; on a unified-memory GPU `RAM`, what its
/// processes hold, against the system RAM when the card carries its total.
fn memory(card: &GpuCard) -> GpuReading<'_> {
    if card.unified_memory {
        return GpuReading {
            label: "RAM",
            current: &card.process_allocated,
            value: with_total(&card.process_allocated, Some(&card.system_ram_total)),
            trend: &card.memory_trend,
            max: card.system_ram_total.value_f64(),
        };
    }
    GpuReading {
        label: "VRAM",
        current: &card.framebuffer_used,
        value: with_total(&card.framebuffer_used, Some(&card.framebuffer_total)),
        trend: &card.memory_trend,
        max: card.framebuffer_total.value_f64(),
    }
}

fn temperature(card: &GpuCard) -> GpuReading<'_> {
    GpuReading {
        label: "temp",
        current: &card.temperature,
        value: value_text(&card.temperature),
        trend: &card.temperature_trend,
        max: Some(TEMPERATURE_SCALE_C),
    }
}

/// Power against the enforced limit; without one (GB10), against the
/// highest draw seen, which the value prints (D-41).
fn power(card: &GpuCard) -> GpuReading<'_> {
    if has_value(&card.power_limit) {
        return GpuReading {
            label: "pwr",
            current: &card.power_draw,
            value: with_total(&card.power_draw, Some(&card.power_limit)),
            trend: &card.power_trend,
            max: card.power_limit.value_f64(),
        };
    }
    let peak = card
        .power_trend
        .points
        .iter()
        .flatten()
        .copied()
        .chain(card.power_draw.value_f64())
        .filter(|value| value.is_finite())
        .reduce(f64::max);
    let value = match peak {
        Some(peak) => format!(
            "{} · peak {}",
            value_text(&card.power_draw),
            format_value(peak, card.power_draw.unit)
        ),
        None => value_text(&card.power_draw),
    };
    GpuReading {
        label: "pwr",
        current: &card.power_draw,
        value,
        trend: &card.power_trend,
        max: peak,
    }
}

/// `value / total` when the total is known.
fn with_total(current: &MetricCell, total: Option<&MetricCell>) -> String {
    match total.filter(|total| has_value(total)) {
        Some(total) => format!("{} / {}", value_text(current), value_text(total)),
        None => value_text(current),
    }
}

/// A bar on the first row of `area`, its value padded on the left to
/// `kind_width` (never cut) in a value column `column` wide (bars sharing
/// the column line their gauges up), and its chart under the gauge in the
/// rows below.
pub(super) fn render_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    reading: &GpuReading<'_>,
    (kind_width, column): (u16, u16),
    context: &DisplayContext,
) {
    let mut rows = RowCursor::new(area);
    let Some(row) = rows.next_row() else { return };
    let ratio = reading.ratio();
    let padded = usize::from(kind_width).max(display_width(&reading.value));
    let gauge = render_bar_row(
        frame,
        row,
        &BarRow {
            label: reading.label,
            ratio,
            value: &pad_left(&reading.value, padded),
            value_width: column,
        },
        context,
    );
    frame.render_widget(Gauge::new(ratio, "", &context.theme), gauge);
    if let Some(chart) = rows.rest() {
        render_chart(
            frame,
            under_bar(chart),
            &reading.trend.points,
            reading.max,
            context,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{gpu_view, missing};
    use dgxtop_core::metric::CanonicalUnit;
    use dgxtop_core::view::CellState;

    fn labels_and_values(readings: &[GpuReading<'_>]) -> Vec<(&'static str, String)> {
        readings
            .iter()
            .map(|reading| (reading.label, reading.value.clone()))
            .collect()
    }

    #[test]
    fn memory_reads_the_framebuffer_or_the_ram_a_unified_gpu_holds() {
        let gpu = gpu_view();
        let [_, discrete] = overview_readings(&gpu.cards[0]);
        assert_eq!(
            (discrete.label, discrete.value.as_str()),
            ("VRAM", "12.3 GiB / 80.0 GiB")
        );
        assert!((discrete.ratio().unwrap() - 12.3 / 80.0).abs() < 1e-12);
        let [_, unified] = overview_readings(&gpu.cards[1]);
        assert_eq!(
            (unified.label, unified.value.as_str(), unified.ratio()),
            ("RAM", "10.0 GiB / 64.0 GiB", Some(10.0 / 64.0))
        );
        // The card carries its RAM total: the GPU view, which projects no
        // memory module, still scales the bar.
        let ram = &status_readings(&gpu.cards[1])[1];
        assert_eq!(
            (ram.label, ram.value.as_str(), ram.ratio()),
            ("RAM", "10.0 GiB / 64.0 GiB", Some(10.0 / 64.0))
        );
        // Without the RAM total what the GPU's processes hold stays, with no
        // bar.
        let mut card = gpu.cards[1].clone();
        card.system_ram_total = missing(CanonicalUnit::Bytes, CellState::Unavailable);
        let [_, alone] = overview_readings(&card);
        assert_eq!((alone.value.as_str(), alone.ratio()), ("10.0 GiB", None));
        // A total the driver does not report is left out, never shown as
        // its state.
        let mut card = gpu.cards[0].clone();
        card.framebuffer_total = missing(CanonicalUnit::Bytes, CellState::Unsupported);
        let [_, no_total] = overview_readings(&card);
        assert_eq!(
            (no_total.value.as_str(), no_total.ratio()),
            ("12.3 GiB", None)
        );
    }

    #[test]
    fn the_status_shows_four_bars_and_leaves_out_one_never_known() {
        let mut card = gpu_view().cards.remove(0);
        assert_eq!(
            labels_and_values(&status_readings(&card)),
            vec![
                ("util", "90.0%".to_owned()),
                ("VRAM", "12.3 GiB / 80.0 GiB".to_owned()),
                ("temp", "71°C".to_owned()),
                ("pwr", "120 W / 250 W".to_owned()),
            ]
        );
        assert_eq!(status_readings(&card)[2].ratio(), Some(0.71));
        // Unsupported now but charted before: the bar stays, empty.
        card.power_draw = missing(CanonicalUnit::Watts, CellState::Unsupported);
        assert_eq!(status_readings(&card).len(), 4);
        card.power_trend.points.clear();
        let labels: Vec<&str> = status_readings(&card)
            .iter()
            .map(|reading| reading.label)
            .collect();
        assert_eq!(labels, vec!["util", "VRAM", "temp"]);
    }

    /// GB10 reports no power limit: the bar is scaled to the highest draw
    /// it has seen, which it prints, like the disk and network rates (D-41).
    #[test]
    fn power_without_a_limit_is_scaled_to_its_peak() {
        let mut card = gpu_view().cards.remove(1);
        card.power_limit = missing(CanonicalUnit::Watts, CellState::Unsupported);
        card.power_draw = crate::test_fixtures::ok(60.0, CanonicalUnit::Watts);
        card.power_trend.points = vec![Some(40.0), None, Some(80.0), Some(60.0)];
        let readings = status_readings(&card);
        let power = readings
            .iter()
            .find(|reading| reading.label == "pwr")
            .unwrap();
        assert_eq!(power.ratio(), Some(0.75));
        assert!(power.value.ends_with("peak 80.0 W"), "{}", power.value);
        assert!(!power.value.contains('/'), "{}", power.value);
        // The value now is part of the peak before the chart has it.
        card.power_trend.points.clear();
        let readings = status_readings(&card);
        let power = readings
            .iter()
            .find(|reading| reading.label == "pwr")
            .unwrap();
        assert_eq!(power.ratio(), Some(1.0));
    }
}
