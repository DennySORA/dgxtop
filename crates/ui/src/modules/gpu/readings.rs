//! The bars of a GPU (D-43, D-49, D-50), each a label, a gauge of the value
//! now and the value right after it: utilization against 100 %, the memory
//! in use (`VRAM` against the framebuffer, or on a unified-memory GPU `RAM`,
//! what its processes hold, against the system RAM), the temperature on a
//! fixed 0–100 °C scale, green up to 50 °C like its text (D-126), and the
//! power against the enforced limit, or on a
//! GPU without one against the highest draw the history recorded (D-118).
//! The overview shows the first two, the utilization's history under its
//! gauge; the GPU view shows all four, live only. The memory bar shows what
//! each tracked process holds in its color (D-64), the rest of the memory
//! in use in the color of the bar's level.

use dgxtop_core::view::{GpuCard, MetricCell, Trend};
use ratatui::Frame;
use ratatui::layout::Rect;

use super::facts::{has_value, reported};
use crate::format::{display_width, format_value, pad_left, value_text};
use crate::modules::common::{BarRow, RowCursor, render_bar_row, render_chart, under_bar};
use crate::registry::DisplayContext;
use crate::theme::LevelScale;
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
    /// How the value takes the level colors where that is not its share of
    /// the scale: a temperature's own (D-126).
    levels: Option<LevelScale>,
}

impl GpuReading<'_> {
    /// Share of the scale now; `None` without a value or a scale.
    pub(super) fn ratio(&self) -> Option<f64> {
        let max = self.max.filter(|max| *max > 0.0)?;
        Some(self.current.value_f64()? / max)
    }

    /// Where the value now sits on the level colors: on its own scale
    /// where it has one, else its share of the bar's.
    pub(super) fn level(&self) -> Option<f64> {
        match self.levels {
            Some(levels) => Some(levels.level(self.current.value_f64()?)),
            None => self.ratio(),
        }
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
        levels: None,
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
            levels: None,
        };
    }
    GpuReading {
        label: "VRAM",
        current: &card.framebuffer_used,
        value: with_total(&card.framebuffer_used, Some(&card.framebuffer_total)),
        trend: &card.memory_trend,
        max: card.framebuffer_total.value_f64(),
        levels: None,
    }
}

fn temperature(card: &GpuCard) -> GpuReading<'_> {
    GpuReading {
        label: "temp",
        current: &card.temperature,
        value: value_text(&card.temperature),
        trend: &card.temperature_trend,
        max: Some(TEMPERATURE_SCALE_C),
        levels: Some(LevelScale::TEMPERATURE),
    }
}

/// Power against the enforced limit; without one (GB10), against the
/// highest draw the history recorded over the last days, or drawn since,
/// which the value prints (D-118). The few minutes the card's trend holds
/// would make any steady draw its own peak. Without a record the bar has
/// no scale to guess.
fn power(card: &GpuCard) -> GpuReading<'_> {
    if has_value(&card.power_limit) {
        return GpuReading {
            label: "pwr",
            current: &card.power_draw,
            value: with_total(&card.power_draw, Some(&card.power_limit)),
            trend: &card.power_trend,
            max: card.power_limit.value_f64(),
            levels: None,
        };
    }
    let peak = card.recorded_power_peak.map(|recorded| {
        card.power_trend
            .points
            .iter()
            .flatten()
            .copied()
            .chain(card.power_draw.value_f64())
            .filter(|value| value.is_finite())
            .fold(recorded, f64::max)
    });
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
        levels: None,
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
    let (ratio, level) = (reading.ratio(), reading.level());
    let padded = usize::from(kind_width).max(display_width(&reading.value));
    let gauge = render_bar_row(
        frame,
        row,
        &BarRow {
            label: reading.label,
            ratio: level,
            value: &pad_left(&reading.value, padded),
            value_width: column,
        },
        context,
    );
    // Every GPU bar, its memory too, is green and turns as it fills (D-73);
    // the temperature only past 50 °C (D-126).
    frame.render_widget(Gauge::new(ratio, "", &context.theme).level(level), gauge);
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

    /// The temperature bar is as long as its share of 100 °C, but it and
    /// its value are colored like the temperature's text: green up to 50 °C,
    /// amber at 75 °C (D-126).
    #[test]
    fn the_temperature_bar_turns_only_past_fifty_degrees() {
        let context = crate::test_fixtures::display_context();
        let theme = &context.theme;
        let mut card = gpu_view().cards.remove(0);
        let width = 5 + 20 + 1 + 6;
        let colors = |card: &GpuCard| {
            let readings = status_readings(card);
            let temperature = readings
                .iter()
                .find(|reading| reading.label == "temp")
                .unwrap();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 1)).unwrap();
            terminal
                .draw(|frame| {
                    render_bar(
                        frame,
                        Rect::new(0, 0, width, 1),
                        temperature,
                        (6, 6),
                        &context,
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (buffer[(5, 0)].fg, buffer[(width - 1, 0)].fg)
        };
        for (celsius, color) in [(45.0, theme.gauge_low), (75.0, theme.gauge_mid)] {
            card.temperature = crate::test_fixtures::ok(celsius, CanonicalUnit::DegC);
            assert_eq!(colors(&card), (color, color), "{celsius} °C");
        }
    }

    /// A GPU's memory bar turns from green through amber to red as it
    /// fills, like its other bars (D-73), one step for each whole percent
    /// (D-104).
    #[test]
    fn the_memory_bar_turns_with_its_level() {
        let context = crate::test_fixtures::display_context();
        let theme = &context.theme;
        let mut card = gpu_view().cards.remove(1);
        let color_at = |card: &GpuCard| {
            let [_, memory] = overview_readings(card);
            let width = 5 + 64 + 1 + 19;
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 1)).unwrap();
            terminal
                .draw(|frame| {
                    render_bar(
                        frame,
                        Rect::new(0, 0, width, 1),
                        &memory,
                        (19, 19),
                        &context,
                    );
                })
                .unwrap();
            terminal.backend().buffer()[(5, 0)].fg
        };
        let quiet = color_at(&card);
        assert_eq!(quiet, theme.level_color(10.0 / 64.0));
        card.process_allocated =
            crate::test_fixtures::ok(32.0 * crate::test_fixtures::GIB, CanonicalUnit::Bytes);
        assert_eq!(color_at(&card), theme.gauge_mid, "half full is amber");
        card.process_allocated =
            crate::test_fixtures::ok(64.0 * crate::test_fixtures::GIB, CanonicalUnit::Bytes);
        assert_eq!(color_at(&card), theme.gauge_high, "only a full bar is red");
        card.process_allocated =
            crate::test_fixtures::ok(60.0 * crate::test_fixtures::GIB, CanonicalUnit::Bytes);
        let busy = color_at(&card);
        assert_eq!(busy, theme.level_color(60.0 / 64.0));
        assert!(![quiet, theme.gauge_mid, theme.gauge_high].contains(&busy));
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
    /// the history recorded over the last days (D-118), which it prints, so
    /// a GPU idle for minutes reads low; without a record the bar has no
    /// scale and the draw stands alone.
    #[test]
    fn power_without_a_limit_is_scaled_to_its_recorded_peak() {
        let mut card = gpu_view().cards.remove(1);
        card.power_limit = missing(CanonicalUnit::Watts, CellState::Unsupported);
        // Idle in P0 for minutes: the last four peak at 13.0 W.
        card.power_draw = crate::test_fixtures::ok(12.4, CanonicalUnit::Watts);
        card.power_trend.points = vec![Some(12.8), None, Some(13.0), Some(12.4)];
        card.recorded_power_peak = Some(80.0);
        let power = |card: &GpuCard| {
            let power = status_readings(card)
                .into_iter()
                .find(|reading| reading.label == "pwr")
                .unwrap();
            (power.ratio(), power.value)
        };
        let (ratio, value) = power(&card);
        assert_eq!(ratio, Some(12.4 / 80.0), "idle reads low: {value}");
        assert_eq!(value, "12.4 W · peak 80.0 W");
        // A draw above the record, whose minute is not recorded yet, is the
        // peak.
        card.power_draw = crate::test_fixtures::ok(95.0, CanonicalUnit::Watts);
        let (ratio, value) = power(&card);
        assert_eq!(ratio, Some(1.0));
        assert_eq!(value, "95.0 W · peak 95.0 W");
        // Without a record: no scale, so no gauge, and the draw alone.
        card.power_draw = crate::test_fixtures::ok(12.4, CanonicalUnit::Watts);
        card.recorded_power_peak = None;
        assert_eq!(power(&card), (None, "12.4 W".to_owned()));
    }
}
