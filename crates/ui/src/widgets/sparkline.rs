//! Sparkline where `None` points are gaps (blank columns), so a missing
//! sample is never drawn as a zero bar. A real zero still shows the lowest
//! bar to stay distinguishable from a gap. One column is one sample, the
//! newest at the right edge; columns can take the gauge colors of their
//! level, so a busy stretch reads red at a glance (D-41). The top quarter of
//! the top row stays empty, so a chart under load never touches the bar or
//! the chart above it (D-52). Parts of a column (what each GPU process
//! holds of the used RAM, D-64) stack from its foot in their own colors.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

use crate::num::{saturate_u16, scaled_round};
use crate::theme::ThemePalette;

const LEVELS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// Eighths of a row left empty above a full-scale column.
const HEADROOM_EIGHTHS: u16 = 2;

/// A part of every point and the style it is drawn in.
pub type Part<'a> = (&'a [Option<f64>], Style);

/// A layer of one column: where it ends, in eighths from the column's
/// foot, and its style.
type Layer = (u16, Style);

/// Right-aligned (newest at the right edge) multi-row sparkline.
#[derive(Debug, Clone)]
pub struct GapSparkline<'a> {
    points: &'a [Option<f64>],
    max: Option<f64>,
    style: Style,
    levels: Option<&'a ThemePalette>,
    parts: Vec<Part<'a>>,
}

impl<'a> GapSparkline<'a> {
    pub fn new(points: &'a [Option<f64>], style: Style) -> Self {
        Self {
            points,
            max: None,
            style,
            levels: None,
            parts: Vec::new(),
        }
    }

    /// Parts of each point (what each GPU process holds of the used RAM),
    /// each aligned with the points at the newest end, stacked from the
    /// foot of the column in order in its style; together capped at the
    /// column, whose own style shows above them.
    pub fn parts(mut self, parts: Vec<Part<'a>>) -> Self {
        self.parts = parts;
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

    /// The largest finite visible point (the scale when none is fixed).
    pub fn peak(&self, width: u16) -> Option<f64> {
        visible(self.points, width)
            .iter()
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
        let scale = self.scale(area.width);
        let offset = area.width.saturating_sub(saturate_u16(points.len()));
        let ratio_of = |value: f64| (value / scale).clamp(0.0, 1.0);
        for (index, point) in points.iter().enumerate() {
            let Some(value) = point.filter(|value| value.is_finite()) else {
                continue;
            };
            let x = area.x + offset + saturate_u16(index);
            let whole = column_eighths(area, ratio_of(value));
            let mut layers: Vec<Layer> = Vec::with_capacity(self.parts.len() + 1);
            let mut stacked = 0.0;
            for (part, style) in &self.parts {
                let Some(share) = aligned(part, points.len() - index) else {
                    continue;
                };
                stacked = (stacked + share).min(value);
                let end = part_eighths(area, ratio_of(stacked)).min(whole);
                layers.push((end, *style));
            }
            layers.push((whole, self.style_for(ratio_of(value))));
            draw_layers(buf, area, x, &layers);
        }
    }
}

/// The point of `part` `from_end` points before its newest end, when it
/// is a finite share above zero.
fn aligned(part: &[Option<f64>], from_end: usize) -> Option<f64> {
    part.len()
        .checked_sub(from_end)
        .and_then(|position| part.get(position).copied().flatten())
        .filter(|share| share.is_finite() && *share > 0.0)
}

/// Eighths of `area`'s height (less the headroom) a part of `ratio` fills.
fn part_eighths(area: Rect, ratio: f64) -> u16 {
    let total_eighths = area
        .height
        .saturating_mul(8)
        .saturating_sub(HEADROOM_EIGHTHS)
        .max(1);
    scaled_round(ratio * f64::from(total_eighths), total_eighths)
}

/// Eighths a column of `ratio` fills: a real zero still shows one.
fn column_eighths(area: Rect, ratio: f64) -> u16 {
    part_eighths(area, ratio).max(1)
}

/// A column of `layers` (each ending where the next begins, the last at the
/// column's top) in block glyphs from the bottom of `area`. A cell holds
/// two colors: where a layer ends inside a cell that the column fills to
/// its top, the layer above it filling most of the rest shows behind the
/// glyph; where the column ends inside the cell, the layer filling most of
/// it takes the cell, so the column keeps its height.
fn draw_layers(buf: &mut Buffer, area: Rect, x: u16, layers: &[Layer]) {
    let Some(&(top, _)) = layers.last() else {
        return;
    };
    for row in 0..area.height {
        let floor = row.saturating_mul(8);
        if top <= floor {
            break;
        }
        let ceiling = floor.saturating_add(8);
        let Some((reach, style)) = cell_glyph(layers, floor, top.min(ceiling), ceiling) else {
            break;
        };
        let level = reach.saturating_sub(floor).clamp(1, 8);
        let symbol = LEVELS.get(usize::from(level - 1)).copied().unwrap_or("█");
        buf.set_string(x, area.bottom() - 1 - row, symbol, style);
    }
}

/// How far up the glyph of the cell `[floor, ceiling)` reaches and its
/// style, for a column whose top in the cell is `cell_top`.
fn cell_glyph(layers: &[Layer], floor: u16, cell_top: u16, ceiling: u16) -> Option<(u16, Style)> {
    let mut start = 0;
    let mut inside: Vec<(u16, u16, Style)> = Vec::new();
    for &(end, style) in layers {
        let (low, high) = (start.max(floor), end.min(cell_top));
        if high > low {
            inside.push((low, high, style));
        }
        start = start.max(end);
    }
    let (&(_, first_end, first_style), above) = inside.split_first()?;
    if above.is_empty() {
        return Some((cell_top, first_style));
    }
    if cell_top < ceiling {
        return Some((cell_top, widest(&inside)?));
    }
    let behind: Option<Color> = widest(above)?.fg;
    Some((
        first_end,
        behind.map_or(first_style, |color| first_style.bg(color)),
    ))
}

/// The style of the layer spanning most of a cell; the lowest of a tie.
fn widest(layers: &[(u16, u16, Style)]) -> Option<Style> {
    layers
        .iter()
        .rev()
        .max_by_key(|(low, high, _)| high - low)
        .map(|&(_, _, style)| style)
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

    /// The GPU's share of the used RAM draws at the bottom of its column in
    /// its own color; where it ends inside a row the column goes on behind
    /// it, so the column keeps its height.
    #[test]
    fn a_part_draws_at_the_bottom_of_its_column() {
        use ratatui::style::Color;
        let area = Rect::new(0, 0, 3, 2);
        let mut buffer = Buffer::empty(area);
        let whole = [Some(100.0), Some(100.0), Some(20.0)];
        // Newest at the right edge: the part's two points share the last two
        // columns; a gap leaves its column as it is.
        let part = [Some(50.0), None];
        GapSparkline::new(&whole, Style::new().fg(Color::Red))
            .max(Some(100.0))
            .parts(vec![(&part, Style::new().fg(Color::Blue))])
            .render(area, &mut buffer);
        assert_eq!(row(&buffer, 0, 3), "▆▆ ");
        assert_eq!(row(&buffer, 1, 3), "█▇▃");
        let cell = |x: u16, y: u16| {
            let cell = &buffer[(x, y)];
            (cell.symbol().to_owned(), cell.fg, cell.bg)
        };
        assert_eq!(cell(0, 1), ("█".to_owned(), Color::Red, Color::Reset));
        assert_eq!(cell(1, 1), ("▇".to_owned(), Color::Blue, Color::Red));
        assert_eq!(cell(1, 0), ("▆".to_owned(), Color::Red, Color::Reset));
        // A part above its column (read at another instant) is capped.
        let mut capped = Buffer::empty(area);
        GapSparkline::new(&whole, Style::new().fg(Color::Red))
            .max(Some(100.0))
            .parts(vec![(&[Some(90.0)], Style::new().fg(Color::Blue))])
            .render(area, &mut capped);
        assert_eq!(capped[(2, 1)].symbol(), "▃");
        assert_eq!(capped[(2, 1)].fg, Color::Blue);
    }

    /// Several GPU processes' parts stack from the foot of the column in
    /// order, each in its color, under the rest of the column; a cell holds
    /// two of them where one ends inside it.
    #[test]
    fn parts_stack_in_order_under_the_rest_of_the_column() {
        use ratatui::style::Color;
        // Three rows: 22 eighths at full scale.
        let area = Rect::new(0, 0, 1, 3);
        let mut buffer = Buffer::empty(area);
        let whole = [Some(100.0)];
        let (first, second) = ([Some(25.0)], [Some(50.0)]);
        GapSparkline::new(&whole, Style::new().fg(Color::Red))
            .max(Some(100.0))
            .parts(vec![
                (&first, Style::new().fg(Color::Blue)),
                (&second, Style::new().fg(Color::Green)),
            ])
            .render(area, &mut buffer);
        let cell = |y: u16| {
            let cell = &buffer[(0, y)];
            (cell.symbol().to_owned(), cell.fg, cell.bg)
        };
        // Blue to 5.5 → 6 eighths, green to 75 % → 17, red to 22.
        assert_eq!(cell(2), ("▆".to_owned(), Color::Blue, Color::Green));
        assert_eq!(cell(1), ("█".to_owned(), Color::Green, Color::Reset));
        assert_eq!(cell(0), ("▆".to_owned(), Color::Red, Color::Reset));
        // A column ending inside a cell shared by two layers keeps its
        // height in the color of the larger one.
        let mut short = Buffer::empty(Rect::new(0, 0, 1, 1));
        GapSparkline::new(&[Some(60.0)], Style::new().fg(Color::Red))
            .max(Some(100.0))
            .parts(vec![(&[Some(20.0)], Style::new().fg(Color::Blue))])
            .render(Rect::new(0, 0, 1, 1), &mut short);
        assert_eq!(short[(0, 0)].symbol(), "▄");
        assert_eq!(short[(0, 0)].fg, Color::Red);
        // A part of zero or a gap draws nothing of its own.
        let mut plain = Buffer::empty(Rect::new(0, 0, 1, 1));
        GapSparkline::new(&[Some(60.0)], Style::new().fg(Color::Red))
            .max(Some(100.0))
            .parts(vec![(&[Some(0.0)], Style::new().fg(Color::Blue))])
            .render(Rect::new(0, 0, 1, 1), &mut plain);
        assert_eq!(plain[(0, 0)].fg, Color::Red);
    }
}
