//! Sparkline where `None` points are gaps (blank columns), so a missing
//! sample is never drawn as a zero bar. A real zero still shows the lowest
//! bar to stay distinguishable from a gap. One column is one sample, the
//! newest at the right edge; columns can take the gauge colors of their
//! level, so a busy stretch reads red at a glance (D-41). The top quarter of
//! the top row stays empty, so a chart under load never touches the bar or
//! the chart above it (D-52). Parts of a column (what each GPU process
//! holds of the used RAM, D-64) stack from its foot in their own colors,
//! each at least an eighth of a row high (D-133); raised, a column of parts
//! lower than a row is drawn a row high, split between them (D-134).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

use crate::num::{saturate_u16, scaled_round};
use crate::theme::{LevelScale, ThemePalette};

const LEVELS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// Eighths of one row.
const ROW_EIGHTHS: u16 = 8;
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
    /// How columns take the level colors where that is not their share of
    /// the scale: a temperature's own (D-126).
    level_scale: Option<LevelScale>,
    parts: Vec<Part<'a>>,
    /// Draws a low column of parts one row high (D-134).
    raise_low_columns: bool,
}

impl<'a> GapSparkline<'a> {
    pub fn new(points: &'a [Option<f64>], style: Style) -> Self {
        Self {
            points,
            max: None,
            style,
            levels: None,
            level_scale: None,
            parts: Vec::new(),
            raise_low_columns: false,
        }
    }

    /// A column holding parts that ends below the top of its bottom row is
    /// drawn one row high instead, split in proportion between its parts
    /// and the rest of it, so however low the column, both colors show; a
    /// column of one color or reaching that row's top keeps its height
    /// (D-134). Not for a chart of one row, which has no full row.
    pub fn raise_low_columns(mut self) -> Self {
        self.raise_low_columns = true;
        self
    }

    /// Parts of each point (what each GPU process holds of the used RAM),
    /// each aligned with the points at the newest end, stacked from the
    /// foot of the column in order in its style, a part above zero at
    /// least an eighth while the column has room; together capped at the
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

    /// With [`Self::level_colors`], colors every column by where its value
    /// sits on `scale` instead: a temperature stays green up to 50 °C
    /// (D-126).
    pub fn level_scale(mut self, scale: LevelScale) -> Self {
        self.level_scale = Some(scale);
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

    /// The style of a column of `value`, `ratio` of the scale.
    fn style_for(&self, value: f64, ratio: f64) -> Style {
        self.levels.map_or(self.style, |palette| {
            let level = self.level_scale.map_or(ratio, |scale| scale.level(value));
            self.style.fg(palette.level_color(level))
        })
    }

    /// Each part's share of the point `from_end` points before the newest
    /// end, with its style; a part without one is left out.
    fn shares_at(&self, from_end: usize) -> Vec<(f64, Style)> {
        self.parts
            .iter()
            .filter_map(|(part, style)| aligned(part, from_end).map(|share| (share, *style)))
            .collect()
    }

    /// The layers of a low column drawn one row high (D-134): the parts
    /// together below, in the color of the one holding most, and the rest
    /// of the column behind them; where the parts fill the column, the
    /// first below and the others behind it. Both in proportion, each at
    /// least an eighth. `None` keeps the column's own height: not raised, a
    /// chart of one row (whose headroom leaves no full row), a column
    /// reaching the top of its bottom row, or a column of one color.
    fn low_column(
        &self,
        area: Rect,
        (value, whole): (f64, u16),
        shares: &[(f64, Style)],
        rest: Style,
    ) -> Option<Vec<Layer>> {
        if !self.raise_low_columns || area.height < 2 || whole >= ROW_EIGHTHS || value <= 0.0 {
            return None;
        }
        let parts = shares
            .iter()
            .map(|(share, _)| share)
            .sum::<f64>()
            .min(value);
        let (below, below_style, behind) = if parts < value {
            (parts, widest_share(shares)?, rest)
        } else {
            let (&(first, style), others) = shares.split_first()?;
            (first, style, widest_share(others)?)
        };
        let end = scaled_round(f64::from(ROW_EIGHTHS) * below / value, ROW_EIGHTHS)
            .clamp(1, ROW_EIGHTHS - 1);
        Some(vec![(end, below_style), (ROW_EIGHTHS, behind)])
    }
}

/// A column's layers at their heights: each share stacked on the ones below
/// it, at least an eighth while the column has room, then the rest of the
/// column to its top.
fn stacked_layers(
    shares: &[(f64, Style)],
    (value, whole): (f64, u16),
    rest: Style,
    eighths: impl Fn(f64) -> u16,
) -> Vec<Layer> {
    let mut layers: Vec<Layer> = Vec::with_capacity(shares.len() + 1);
    let mut stacked = 0.0;
    let mut end: u16 = 0;
    for &(share, style) in shares {
        stacked = (stacked + share).min(value);
        end = eighths(stacked).max(end.saturating_add(1)).min(whole);
        layers.push((end, style));
    }
    layers.push((whole, rest));
    layers
}

/// The style of the share holding most; the lowest of a tie.
fn widest_share(shares: &[(f64, Style)]) -> Option<Style> {
    shares
        .iter()
        .rev()
        .max_by(|(left, _), (right, _)| left.total_cmp(right))
        .map(|&(_, style)| style)
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
            let shares = self.shares_at(points.len() - index);
            let rest = self.style_for(value, ratio_of(value));
            let layers = self
                .low_column(area, (value, whole), &shares, rest)
                .unwrap_or_else(|| {
                    stacked_layers(&shares, (value, whole), rest, |stacked| {
                        part_eighths(area, ratio_of(stacked))
                    })
                });
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
        .saturating_mul(ROW_EIGHTHS)
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
/// glyph. Where the column ends inside a cell it keeps its height in one
/// color: the top layer's (D-69: the rest of the used RAM above the GPU
/// processes), but the widest part's where the whole column is in its
/// bottom cell, so a part never vanishes from a short column (D-133).
fn draw_layers(buf: &mut Buffer, area: Rect, x: u16, layers: &[Layer]) {
    let Some(&(top, _)) = layers.last() else {
        return;
    };
    for row in 0..area.height {
        let floor = row.saturating_mul(ROW_EIGHTHS);
        if top <= floor {
            break;
        }
        let ceiling = floor.saturating_add(ROW_EIGHTHS);
        let Some((reach, style)) = cell_glyph(layers, floor, top.min(ceiling), ceiling) else {
            break;
        };
        let level = reach.saturating_sub(floor).clamp(1, ROW_EIGHTHS);
        let symbol = LEVELS.get(usize::from(level - 1)).copied().unwrap_or("█");
        buf.set_string(x, area.bottom() - 1 - row, symbol, style);
    }
}

/// What of a layer is inside one cell.
#[derive(Debug, Clone, Copy)]
struct Piece {
    low: u16,
    high: u16,
    style: Style,
    /// A part of the column, not the rest of it above the parts.
    part: bool,
}

/// How far up the glyph of the cell `[floor, ceiling)` reaches and its
/// style, for a column whose top in the cell is `cell_top`.
fn cell_glyph(layers: &[Layer], floor: u16, cell_top: u16, ceiling: u16) -> Option<(u16, Style)> {
    let parts = layers.len().saturating_sub(1);
    let mut start = 0;
    let mut inside: Vec<Piece> = Vec::new();
    for (index, &(end, style)) in layers.iter().enumerate() {
        let (low, high) = (start.max(floor), end.min(cell_top));
        if high > low {
            inside.push(Piece {
                low,
                high,
                style,
                part: index < parts,
            });
        }
        start = start.max(end);
    }
    let (first, above) = inside.split_first()?;
    if above.is_empty() {
        return Some((cell_top, first.style));
    }
    if cell_top < ceiling {
        let whole_column_here = floor == 0;
        let part = whole_column_here
            .then(|| widest(inside.iter().filter(|piece| piece.part)))
            .flatten();
        let style = part.or_else(|| inside.last().map(|piece| piece.style))?;
        return Some((cell_top, style));
    }
    let behind: Option<Color> = widest(above.iter())?.fg;
    Some((
        first.high,
        behind.map_or(first.style, |color| first.style.bg(color)),
    ))
}

/// The style of the piece spanning most of a cell; the lowest of a tie.
fn widest<'a>(pieces: impl DoubleEndedIterator<Item = &'a Piece>) -> Option<Style> {
    pieces
        .rev()
        .max_by_key(|piece| piece.high - piece.low)
        .map(|piece| piece.style)
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
        // A part of zero or a gap draws nothing of its own.
        let mut plain = Buffer::empty(Rect::new(0, 0, 1, 1));
        GapSparkline::new(&[Some(60.0)], Style::new().fg(Color::Red))
            .max(Some(100.0))
            .parts(vec![(&[Some(0.0)], Style::new().fg(Color::Blue))])
            .render(Rect::new(0, 0, 1, 1), &mut plain);
        assert_eq!(plain[(0, 0)].fg, Color::Red);
    }

    /// One column of `whole` with `parts` stacked at its foot, `height`
    /// rows high on a scale of 100, a low one drawn a row high if `raise`.
    fn draw_column(height: u16, whole: f64, parts: &[(f64, Color)], raise: bool) -> Buffer {
        let area = Rect::new(0, 0, 1, height);
        let mut buffer = Buffer::empty(area);
        let points: Vec<[Option<f64>; 1]> = parts.iter().map(|(share, _)| [Some(*share)]).collect();
        let styled = points
            .iter()
            .zip(parts)
            .map(|(point, (_, color))| (point.as_slice(), Style::new().fg(*color)))
            .collect();
        let column = [Some(whole)];
        let mut sparkline = GapSparkline::new(&column, Style::new().fg(Color::Red))
            .max(Some(100.0))
            .parts(styled);
        if raise {
            sparkline = sparkline.raise_low_columns();
        }
        sparkline.render(area, &mut buffer);
        buffer
    }

    fn column_of(height: u16, whole: f64, parts: &[(f64, Color)]) -> Buffer {
        draw_column(height, whole, parts, false)
    }

    /// Each row of a one-column buffer, top first: glyph, color, backdrop.
    fn rows_of(buffer: &Buffer) -> Vec<(String, Color, Color)> {
        (0..buffer.area.height)
            .map(|y| {
                let cell = &buffer[(0, y)];
                (cell.symbol().to_owned(), cell.fg, cell.bg)
            })
            .collect()
    }

    /// Raised, as the Overview's memory chart is (D-134), a low column
    /// holding parts is drawn one row high: the parts below in the color of
    /// the one holding most, in proportion and at least an eighth, the rest
    /// of the column behind them, so both colors show. A column of one
    /// color, or one reaching past its bottom row, keeps its height.
    #[test]
    fn a_low_column_is_drawn_one_row_high_and_split() {
        let blank = (" ".to_owned(), Color::Reset, Color::Reset);
        // 20.2 % in use, a job holding 9.9 %: about half the row each.
        assert_eq!(
            rows_of(&draw_column(3, 20.2, &[(9.9, Color::Blue)], true)),
            vec![
                blank.clone(),
                blank,
                ("▄".to_owned(), Color::Blue, Color::Red)
            ]
        );
        // However small, a share keeps an eighth.
        assert_eq!(
            rows_of(&draw_column(3, 20.2, &[(0.1, Color::Blue)], true))[2],
            ("▁".to_owned(), Color::Blue, Color::Red)
        );
        // Several parts go together, in the color of the one holding most.
        assert_eq!(
            rows_of(&draw_column(
                3,
                20.0,
                &[(3.0, Color::Blue), (9.0, Color::Green)],
                true
            ))[2],
            ("▅".to_owned(), Color::Green, Color::Red)
        );
        // One color keeps the height: 20.2 % of 22 eighths → 4.
        assert_eq!(
            rows_of(&draw_column(3, 20.2, &[(20.2, Color::Blue)], true))[2],
            ("▄".to_owned(), Color::Blue, Color::Reset)
        );
        // Past its bottom row a column keeps its height, its top layer at
        // its top (D-69): the job to 15 eighths, the column to 18.
        let tall = rows_of(&draw_column(3, 80.0, &[(70.0, Color::Blue)], true));
        assert_eq!(tall[0], ("▂".to_owned(), Color::Red, Color::Reset));
        assert_eq!(tall[2], ("█".to_owned(), Color::Blue, Color::Reset));
    }

    /// A column that ends in its bottom cell has one color there: a part
    /// takes it, the widest of them, so a GPU process never vanishes from
    /// a short chart, and the column keeps its height. A taller column's
    /// top layer keeps the cell the column ends in (D-69), so the rest of
    /// it shows above the processes.
    #[test]
    fn a_column_in_one_cell_shows_its_widest_part() {
        let foot = |buffer: &Buffer| {
            let cell = &buffer[(0, buffer.area.bottom() - 1)];
            (cell.symbol().to_owned(), cell.fg)
        };
        // Three rows (22 eighths): 15.9 % in use, 10 % of it a GPU job's:
        // 3 eighths, 2 of them the job's.
        assert_eq!(
            foot(&column_of(3, 15.9, &[(10.0, Color::Blue)])),
            ("▃".to_owned(), Color::Blue)
        );
        // A part smaller than the rest of the column takes it too.
        assert_eq!(
            foot(&column_of(3, 15.9, &[(4.0, Color::Blue)])),
            ("▃".to_owned(), Color::Blue)
        );
        // Of two parts, the wider one.
        assert_eq!(
            foot(&column_of(
                3,
                20.0,
                &[(3.0, Color::Blue), (9.0, Color::Green)]
            )),
            ("▄".to_owned(), Color::Green)
        );
        // Over two rows (14 eighths): the job to 10, the column to 11.
        let tall = column_of(2, 80.0, &[(70.0, Color::Blue)]);
        assert_eq!((tall[(0, 0)].symbol(), tall[(0, 0)].fg), ("▃", Color::Red));
        assert_eq!((tall[(0, 1)].symbol(), tall[(0, 1)].fg), ("█", Color::Blue));
    }

    /// However small, a part above zero fills an eighth of its column: at
    /// the foot of a full column it shows under the rest of it.
    #[test]
    fn a_tiny_part_keeps_an_eighth() {
        let column = column_of(3, 100.0, &[(0.1, Color::Blue)]);
        let foot = &column[(0, 2)];
        assert_eq!(
            (foot.symbol(), foot.fg, foot.bg),
            ("▁", Color::Blue, Color::Red)
        );
        assert_eq!(column[(0, 1)].fg, Color::Red);
    }
}
