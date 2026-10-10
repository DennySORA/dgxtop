//! Sparkline where `None` points are gaps (blank columns), so a missing
//! sample is never drawn as a zero bar. A real zero still shows the lowest
//! bar to stay distinguishable from a gap. One column is one sample, the
//! newest at the right edge; columns can take the gauge colors of their
//! level, so a busy stretch reads red at a glance (D-41). Optional peaks
//! (each column's maximum) draw dimmed above the column, so a short spike
//! inside a long bucket stays visible (D-46), or in a style of their own,
//! a neutral band behind the colored means (D-50). The top quarter of the
//! top row stays empty, so a chart under load never touches the bar or the
//! chart above it (D-52).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;

use crate::num::{saturate_u16, scaled_round};
use crate::theme::ThemePalette;

const LEVELS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// Eighths of a row left empty above a full-scale column.
const HEADROOM_EIGHTHS: u16 = 2;

/// Right-aligned (newest at the right edge) multi-row sparkline.
#[derive(Debug, Clone)]
pub struct GapSparkline<'a> {
    points: &'a [Option<f64>],
    peaks: Option<&'a [Option<f64>]>,
    peak_style: Option<Style>,
    max: Option<f64>,
    style: Style,
    levels: Option<&'a ThemePalette>,
}

impl<'a> GapSparkline<'a> {
    pub fn new(points: &'a [Option<f64>], style: Style) -> Self {
        Self {
            points,
            peaks: None,
            peak_style: None,
            max: None,
            style,
            levels: None,
        }
    }

    /// Draws the peaks in `style` instead of their column's dimmed color.
    pub fn peak_style(mut self, style: Style) -> Self {
        self.peak_style = Some(style);
        self
    }

    /// Fixed scale maximum (e.g. 100 for percentages); otherwise the visible maximum.
    pub fn max(mut self, max: Option<f64>) -> Self {
        self.max = max.filter(|value| value.is_finite() && *value > 0.0);
        self
    }

    /// Colors every column by its share of the scale, like a gauge.
    pub fn level_colors(mut self, palette: &'a ThemePalette) -> Self {
        self.levels = Some(palette);
        self
    }

    /// Each point's maximum, aligned with the points, drawn dimmed above the
    /// point's column.
    pub fn peaks(mut self, peaks: &'a [Option<f64>]) -> Self {
        self.peaks = Some(peaks);
        self
    }

    /// The largest finite visible point or peak (the scale when none is
    /// fixed).
    pub fn peak(&self, width: u16) -> Option<f64> {
        let peaks = self
            .peaks
            .map(|peaks| visible(peaks, width))
            .unwrap_or_default();
        visible(self.points, width)
            .iter()
            .chain(peaks)
            .flatten()
            .copied()
            .filter(|value| value.is_finite())
            .reduce(f64::max)
    }

    fn scale(&self, width: u16) -> f64 {
        self.max.unwrap_or_else(|| {
            let observed = self.peak(width).unwrap_or(0.0);
            if observed > 0.0 { observed } else { 1.0 }
        })
    }

    /// The column's style for `ratio` of the scale.
    fn style_for(&self, ratio: f64) -> Style {
        self.levels.map_or(self.style, |palette| {
            self.style.fg(palette.level_color(ratio))
        })
    }
}

/// The newest `width` items of `items`.
fn visible<T>(items: &[T], width: u16) -> &[T] {
    let skip = items.len().saturating_sub(usize::from(width));
    items.get(skip..).unwrap_or_default()
}

impl Widget for GapSparkline<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let points = visible(self.points, area.width);
        let peaks = self
            .peaks
            .map(|peaks| visible(peaks, area.width))
            .unwrap_or_default();
        let scale = self.scale(area.width);
        let offset = area.width.saturating_sub(saturate_u16(points.len()));
        let ratio_of = |value: f64| (value / scale).clamp(0.0, 1.0);
        for (index, point) in points.iter().enumerate() {
            let Some(value) = point.filter(|value| value.is_finite()) else {
                continue;
            };
            let x = area.x + offset + saturate_u16(index);
            let peak = peaks
                .get(index)
                .copied()
                .flatten()
                .filter(|peak| peak.is_finite() && *peak > value);
            if let Some(peak) = peak {
                let style = self
                    .peak_style
                    .unwrap_or_else(|| self.style_for(ratio_of(peak)).add_modifier(Modifier::DIM));
                draw_column(buf, area, x, ratio_of(peak), style);
            }
            draw_column(
                buf,
                area,
                x,
                ratio_of(value),
                self.style_for(ratio_of(value)),
            );
        }
    }
}

/// Block glyphs from the bottom of `area` up to `ratio` of its height less
/// the headroom.
fn draw_column(buf: &mut Buffer, area: Rect, x: u16, ratio: f64, style: Style) {
    let total_eighths = area
        .height
        .saturating_mul(8)
        .saturating_sub(HEADROOM_EIGHTHS)
        .max(1);
    let eighths = scaled_round(ratio * f64::from(total_eighths), total_eighths).max(1);
    for row in 0..area.height {
        let level = eighths.saturating_sub(row.saturating_mul(8)).min(8);
        if level == 0 {
            break;
        }
        let symbol = LEVELS.get(usize::from(level - 1)).copied().unwrap_or("█");
        buf.set_string(x, area.bottom() - 1 - row, symbol, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(buffer: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buffer[(x, y)].symbol().to_owned())
            .collect()
    }

    #[test]
    fn gaps_are_blank_and_zero_is_lowest_bar() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = Buffer::empty(area);
        let points = [Some(0.0), None, Some(100.0), Some(50.0)];
        GapSparkline::new(&points, Style::new())
            .max(Some(100.0))
            .render(area, &mut buffer);
        assert_eq!(row(&buffer, 0, 4), "▁ ▆▃");
    }

    #[test]
    fn the_peak_is_the_largest_visible_point() {
        let points = [Some(9.0), Some(2.0), None, Some(5.0)];
        let line = GapSparkline::new(&points, Style::new());
        assert_eq!(line.peak(4), Some(9.0));
        assert_eq!(line.peak(2), Some(5.0), "older points scrolled out");
        assert_eq!(GapSparkline::new(&[None], Style::new()).peak(4), None);
    }

    #[test]
    fn multi_row_fill_and_non_finite_points() {
        let area = Rect::new(0, 0, 2, 2);
        let mut buffer = Buffer::empty(area);
        let points = [Some(1.0), Some(f64::NAN)];
        GapSparkline::new(&points, Style::new())
            .max(Some(1.0))
            .render(area, &mut buffer);
        assert_eq!(row(&buffer, 0, 2), "▆ ");
        assert_eq!(row(&buffer, 1, 2), "█ ");
    }

    #[test]
    fn a_full_scale_column_leaves_the_top_quarter_of_its_top_row_empty() {
        // Two charts stacked row on row, both at full scale: the lower one's
        // top row stops a quarter row short, so the two never join.
        let area = Rect::new(0, 0, 1, 3);
        let mut buffer = Buffer::empty(area);
        GapSparkline::new(&[Some(100.0)], Style::new())
            .max(Some(100.0))
            .render(Rect::new(0, 1, 1, 2), &mut buffer);
        GapSparkline::new(&[Some(100.0)], Style::new())
            .max(Some(100.0))
            .render(Rect::new(0, 0, 1, 1), &mut buffer);
        let column: Vec<&str> = (0..3).map(|y| buffer[(0, y)].symbol()).collect();
        assert_eq!(column, vec!["▆", "▆", "█"]);
    }
}
