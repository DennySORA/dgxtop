//! One strip of a history panel (D-50), left to right:
//!
//! - the gutter: in a panel of one series the scale's top and its zero, in a
//!   panel of several the strip's label and its scale;
//! - the plot: each column the mean over its time in block glyphs, colored
//!   by its share of the scale (or for a rate by its direction, as on the
//!   overview), a memory chart's tracked processes stacked at its foot in
//!   their colors (D-64); a dotted base where nothing was recorded, so a
//!   gap never reads as zero. Peaks are not drawn (D-57): the legend gives
//!   the window's peak;
//! - the legend: the value now (or a run's last), the mean and the peak
//!   (and for power the energy), as many as the strip has rows.

use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::view::{HISTORY_GUTTER_COLUMNS, HISTORY_LEGEND_COLUMNS, HistoryPointView};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::panels::{Paint, SeriesState, Strip};
use crate::format::{CellTone, NOT_AVAILABLE, fixed, format_energy, format_value, pad_left};
use crate::modules::common::draw_line;
use crate::registry::DisplayContext;
use crate::widgets::GapSparkline;

/// Marks a column where nothing was recorded.
const NO_DATA: &str = "┈";
/// Text of a legend whose query has not answered yet.
const LOADING: &str = "loading…";

/// The window a strip draws: its start and end on the boot clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Window {
    pub(super) start_ns: u64,
    pub(super) end_ns: u64,
}

/// Columns of a strip's plot in `width`.
pub(super) fn plot_width(width: u16) -> u16 {
    width.saturating_sub(HISTORY_GUTTER_COLUMNS + HISTORY_LEGEND_COLUMNS)
}

/// Draws `strip` into `area`; `alone` when it is its panel's only strip.
pub(super) fn render_strip(
    frame: &mut Frame<'_>,
    area: Rect,
    (strip, alone): (&Strip<'_>, bool),
    window: Window,
    context: &DisplayContext,
) {
    if area.is_empty() {
        return;
    }
    let plot = Rect::new(
        area.x + HISTORY_GUTTER_COLUMNS.min(area.width),
        area.y,
        plot_width(area.width),
        area.height,
    );
    render_gutter(frame, area, strip, alone, context);
    render_plot(frame, plot, strip, window, context);
    let legend_x = plot.right() + 1;
    let legend = Rect::new(
        legend_x.min(area.right()),
        area.y,
        area.right().saturating_sub(legend_x),
        area.height,
    );
    for (row, line) in legend_lines(strip, area.height, context).iter().enumerate() {
        let y = legend.y + u16::try_from(row).unwrap_or(u16::MAX);
        if y < legend.bottom() {
            draw_line(
                frame.buffer_mut(),
                Rect::new(legend.x, y, legend.width, 1),
                line,
            );
        }
    }
}

/// The gutter: labels right-aligned before the plot's axis line.
fn render_gutter(
    frame: &mut Frame<'_>,
    area: Rect,
    strip: &Strip<'_>,
    alone: bool,
    context: &DisplayContext,
) {
    let theme = &context.theme;
    let text_width = usize::from(HISTORY_GUTTER_COLUMNS.saturating_sub(1));
    let scale = strip
        .scale
        .map(|scale| axis_value(scale, strip.series.unit))
        .unwrap_or_default();
    let last = area.height.saturating_sub(1);
    for row in 0..area.height {
        let (text, style) = match (alone, row) {
            (true, 0) => (scale.clone(), theme.label_style()),
            (true, row) if row == last => ("0".to_owned(), theme.label_style()),
            (false, 0) => (strip.label.clone(), theme.title_style()),
            (false, 1) => (scale.clone(), theme.muted_style()),
            _ => (String::new(), theme.muted_style()),
        };
        let tick = if text.is_empty() { "│" } else { "┤" };
        let line = Line::from(vec![
            Span::styled(pad_left(&text, text_width), style),
            Span::styled(tick, theme.border_style(false)),
        ]);
        draw_line(
            frame.buffer_mut(),
            Rect::new(
                area.x,
                area.y + row,
                HISTORY_GUTTER_COLUMNS.min(area.width),
                1,
            ),
            &line,
        );
    }
}

/// The columns of the window, then the dotted base where none was recorded.
fn render_plot(
    frame: &mut Frame<'_>,
    plot: Rect,
    strip: &Strip<'_>,
    window: Window,
    context: &DisplayContext,
) {
    if plot.is_empty() {
        return;
    }
    let means = columns_of(
        strip.series.points,
        (window.start_ns, window.end_ns),
        plot.width,
    );
    let theme = &context.theme;
    let color = match strip.paint {
        Paint::Levels => theme.text,
        Paint::Read | Paint::Receive => theme.success,
        Paint::Write => theme.warning,
        Paint::Transmit => theme.accent,
        Paint::Count => theme.primary,
    };
    let part_columns: Vec<Vec<Option<f64>>> = strip
        .series
        .parts
        .iter()
        .map(|part| grid_columns(&part.means, plot.width))
        .collect();
    let parts = strip
        .series
        .parts
        .iter()
        .zip(&part_columns)
        .map(|(part, columns)| {
            (
                columns.as_slice(),
                Style::new().fg(theme.process_color(part.slot)),
            )
        })
        .collect();
    let mut sparkline = GapSparkline::new(&means, Style::new().fg(color))
        .max(strip.scale)
        .parts(parts);
    if strip.paint == Paint::Levels {
        sparkline = sparkline.level_colors(theme);
    }
    frame.render_widget(sparkline, plot);
    let base = plot.bottom() - 1;
    for (column, mean) in means.iter().enumerate() {
        if mean.is_none() {
            let x = plot.x + u16::try_from(column).unwrap_or(u16::MAX);
            frame
                .buffer_mut()
                .set_string(x, base, NO_DATA, theme.muted_style());
        }
    }
}

/// The legend's lines for `rows` rows: the value now, the mean and the
/// peak, and for power the energy, from the first that fit.
fn legend_lines(strip: &Strip<'_>, rows: u16, context: &DisplayContext) -> Vec<Line<'static>> {
    let theme = &context.theme;
    let series = &strip.series;
    match series.state {
        // The panel's title says why.
        SeriesState::Failed(_) => {
            return vec![Line::from(Span::styled(
                "query failed",
                theme.tone_style(CellTone::Conflict),
            ))];
        }
        SeriesState::Pending => {
            return vec![Line::from(Span::styled(LOADING, theme.muted_style()))];
        }
        SeriesState::Ready => {}
    }
    let mut entries = vec![series.latest, ("avg", series.mean), ("max", series.peak)];
    if rows < 3 {
        entries.drain(..3 - usize::from(rows.max(1)));
    }
    let mut lines: Vec<Line<'static>> = entries
        .into_iter()
        .map(|(label, value)| legend_entry(label, value, strip, context))
        .collect();
    if rows >= 4
        && let Some(joules) = series.energy
    {
        lines.push(Line::from(vec![
            Span::styled("energy ", theme.label_style()),
            Span::styled(format_energy(joules), theme.text_style()),
        ]));
    }
    lines
}

/// `max 98.0%`: the value colored by its share of the scale on a bounded
/// strip.
fn legend_entry(
    label: &'static str,
    value: Option<f64>,
    strip: &Strip<'_>,
    context: &DisplayContext,
) -> Line<'static> {
    let theme = &context.theme;
    let (text, style) = match value.filter(|value| value.is_finite()) {
        Some(value) => {
            let style = match (strip.paint, strip.scale) {
                (Paint::Levels, Some(scale)) if scale > 0.0 => {
                    Style::new().fg(theme.level_color(value / scale))
                }
                _ => theme.text_style(),
            };
            (format_value(value, strip.series.unit), style)
        }
        None => (NOT_AVAILABLE.to_owned(), theme.muted_style()),
    };
    Line::from(vec![
        Span::styled(format!("{label} "), theme.label_style()),
        Span::styled(text, style),
    ])
}

/// A scale's top in the gutter's few columns: `100%`, `80G`, `2.8M/s`,
/// `100°C`, `250W`.
pub(super) fn axis_value(value: f64, unit: CanonicalUnit) -> String {
    match unit {
        CanonicalUnit::Percent => format!("{}%", fixed(value, 0)),
        CanonicalUnit::DegC => format!("{}°C", fixed(value, 0)),
        CanonicalUnit::Watts => format!("{}W", fixed(value, 0)),
        CanonicalUnit::Bytes => binary_compact(value, ""),
        CanonicalUnit::BytesPerSecond => binary_compact(value, "/s"),
        _ => format_value(value, unit),
    }
}

/// `80G`, `2.8M` (binary prefixes, one decimal below 10).
fn binary_compact(value: f64, suffix: &str) -> String {
    const PREFIXES: [&str; 6] = ["", "K", "M", "G", "T", "P"];
    let mut scaled = value;
    let mut prefix = 0;
    while scaled >= 1024.0 && prefix < PREFIXES.len() - 1 {
        scaled /= 1024.0;
        prefix += 1;
    }
    let digits = if scaled < 10.0 && prefix > 0 {
        fixed(scaled, 1)
    } else {
        fixed(scaled, 0)
    };
    format!("{digits}{}{suffix}", PREFIXES[prefix])
}

/// `width` columns of the window `(start, end)`: per column the
/// time-weighted mean of the points it overlaps; `None` where no point
/// with a value overlaps it, so a gap stays blank.
pub(super) fn columns_of(
    points: &[HistoryPointView],
    (start, end): (u64, u64),
    width: u16,
) -> Vec<Option<f64>> {
    let columns = usize::from(width);
    let span = u128::from(end.saturating_sub(start));
    if columns == 0 || span == 0 {
        return Vec::new();
    }
    let edge = |column: usize| -> u128 {
        u128::from(start) + span * u128::try_from(column).unwrap_or(u128::MAX) / u128::from(width)
    };
    let mut sums = vec![(0.0_f64, 0.0_f64); columns];
    for point in points {
        let Some(mean) = point.mean.filter(|mean| mean.is_finite()) else {
            continue;
        };
        let (from, to) = (u128::from(point.start_utc_ns), u128::from(point.end_utc_ns));
        for column in column_range(from, to, (u128::from(start), span), columns) {
            let overlap = to
                .min(edge(column + 1))
                .saturating_sub(from.max(edge(column)));
            #[allow(clippy::cast_precision_loss)]
            let weight = overlap.max(1) as f64;
            sums[column].0 += mean * weight;
            sums[column].1 += weight;
        }
    }
    sums.into_iter()
        .map(|(sum, weight)| (weight > 0.0).then(|| sum / weight))
        .collect()
}

/// `width` columns of an even grid of `cells` over the same window: per
/// column the mean of the cells it overlaps, weighted by the overlap;
/// `None` where no overlapped cell has a value.
pub(super) fn grid_columns(cells: &[Option<f64>], width: u16) -> Vec<Option<f64>> {
    let count = u64::try_from(cells.len()).unwrap_or(u64::MAX);
    let columns = u64::from(width);
    if count == 0 {
        return vec![None; usize::from(width)];
    }
    (0..columns)
        .map(|column| {
            // Column and cell edges in units of 1 / (count × columns).
            let (start, end) = (column * count, (column + 1) * count);
            let (first, last) = (start / columns, ((end - 1) / columns).min(count - 1));
            let mut sum = 0.0;
            let mut weight = 0.0;
            for cell in first..=last {
                let Some(mean) = usize::try_from(cell)
                    .ok()
                    .and_then(|cell| cells.get(cell).copied().flatten())
                    .filter(|mean| mean.is_finite())
                else {
                    continue;
                };
                let overlap = end.min((cell + 1) * columns) - start.max(cell * columns);
                #[allow(clippy::cast_precision_loss)]
                let overlap = overlap as f64;
                sum += mean * overlap;
                weight += overlap;
            }
            (weight > 0.0).then(|| sum / weight)
        })
        .collect()
}

/// The tallest column any of `strips` draws in a plot `width` columns
/// wide: the shared scale of a rate pair, so its heights compare and its
/// busiest stretch reaches the top.
pub(super) fn tallest_column(strips: &[Strip<'_>], window: Window, width: u16) -> Option<f64> {
    strips
        .iter()
        .flat_map(|strip| columns_of(strip.series.points, (window.start_ns, window.end_ns), width))
        .flatten()
        .filter(|mean| mean.is_finite() && *mean > 0.0)
        .reduce(f64::max)
}

/// Columns `[from, to)` overlaps among `columns` over `(start, span)`.
fn column_range(
    from: u128,
    to: u128,
    (start, span): (u128, u128),
    columns: usize,
) -> std::ops::Range<usize> {
    let width = u128::try_from(columns).unwrap_or(u128::MAX);
    let column_of = |time: u128| time.saturating_sub(start) * width / span;
    let first = column_of(from).min(width);
    let last = column_of(to.saturating_sub(1).max(from)).min(width.saturating_sub(1));
    if to <= start || from >= start + span {
        return 0..0;
    }
    let first = usize::try_from(first).unwrap_or(columns);
    let last = usize::try_from(last).unwrap_or(columns);
    first..(last + 1).min(columns)
}

/// Accessibility text of a strip: `GPU 0: now 90.0% avg 19.2% max 98.0%`.
pub(super) fn strip_text(strip: &Strip<'_>) -> String {
    let series = &strip.series;
    let value = |value: Option<f64>| {
        value.map_or_else(
            || NOT_AVAILABLE.to_owned(),
            |value| format_value(value, series.unit),
        )
    };
    let label = if strip.label.is_empty() {
        String::new()
    } else {
        format!("{}: ", strip.label)
    };
    let (latest_label, latest) = series.latest;
    format!(
        "{label}{latest_label} {} avg {} max {}",
        value(latest),
        value(series.mean),
        value(series.peak)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::history::panels::panels;
    use crate::test_fixtures::{GIB, HISTORY_END, display_context, history_view};
    use dgxtop_core::view::{HistoryChartKind, HistoryPart};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const SECOND: u64 = 1_000_000_000;

    fn point(start_s: u64, end_s: u64, mean: Option<f64>, max: Option<f64>) -> HistoryPointView {
        HistoryPointView {
            start_utc_ns: start_s * SECOND,
            end_utc_ns: end_s * SECOND,
            mean,
            min: None,
            max,
        }
    }

    fn hour() -> Window {
        Window {
            start_ns: HISTORY_END - 3_600 * SECOND,
            end_ns: HISTORY_END,
        }
    }

    fn draw(strip: &Strip<'_>, alone: bool, (width, rows): (u16, u16)) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, rows)).unwrap();
        terminal
            .draw(|frame| {
                render_strip(
                    frame,
                    frame.area(),
                    (strip, alone),
                    hour(),
                    &display_context(),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..rows)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn columns_follow_time_and_keep_gaps() {
        // Four 10 s points over 40 s in four columns: one point each.
        let points = [
            point(0, 10, Some(10.0), Some(12.0)),
            point(10, 20, None, None),
            point(20, 30, Some(30.0), Some(90.0)),
            point(30, 40, Some(40.0), None),
        ];
        let means = columns_of(&points, (0, 40 * SECOND), 4);
        assert_eq!(means, vec![Some(10.0), None, Some(30.0), Some(40.0)]);
        // Two columns merge two points each, time-weighted; the gap only
        // narrows the weight, it never counts as zero.
        let means = columns_of(&points, (0, 40 * SECOND), 2);
        assert_eq!(means, vec![Some(10.0), Some(35.0)]);
        // Eight columns: a point covers two; points before the window are
        // left out.
        let late = [
            point(0, 10, Some(5.0), None),
            point(40, 50, Some(7.0), None),
        ];
        let means = columns_of(&late, (40 * SECOND, 80 * SECOND), 8);
        assert_eq!(
            means,
            vec![Some(7.0), Some(7.0), None, None, None, None, None, None]
        );
        assert_eq!(columns_of(&points, (0, 0), 4), Vec::new());
    }

    /// A part's even grid maps onto any number of columns by overlap, a
    /// cell without a value leaving the columns only it covers blank.
    #[test]
    fn a_part_grid_maps_onto_the_plot_columns() {
        let cells = [Some(2.0), Some(4.0), None, Some(8.0)];
        assert_eq!(
            grid_columns(&cells, 2),
            vec![Some(3.0), Some(8.0)],
            "two cells a column; the gap only narrows the weight"
        );
        assert_eq!(
            grid_columns(&cells, 8),
            vec![
                Some(2.0),
                Some(2.0),
                Some(4.0),
                Some(4.0),
                None,
                None,
                Some(8.0),
                Some(8.0)
            ]
        );
        assert_eq!(grid_columns(&cells, 3)[0], Some(2.0 * 0.75 + 4.0 * 0.25));
        assert_eq!(grid_columns(&[], 2), vec![None, None]);
    }

    /// A memory strip stacks its tracked processes at the foot of each
    /// column in their colors.
    #[test]
    fn a_memory_strip_draws_its_processes_in_their_colors() {
        let context = display_context();
        let mut history = history_view();
        let memory = history
            .charts
            .iter_mut()
            .find(|chart| chart.kind == HistoryChartKind::GpuMemory)
            .unwrap();
        memory.points = (0..60)
            .map(|minute| point(minute * 60, (minute + 1) * 60, Some(40.0 * GIB), None))
            .map(|mut point| {
                point.start_utc_ns += HISTORY_END - 3_600 * SECOND;
                point.end_utc_ns += HISTORY_END - 3_600 * SECOND;
                point
            })
            .collect();
        memory.parts = vec![HistoryPart {
            slot: 2,
            means: vec![Some(30.0 * GIB); 60],
        }];
        let panels = panels(&history);
        let strip = panels
            .iter()
            .find(|panel| panel.title == "GPU memory")
            .and_then(|panel| panel.strips.first())
            .unwrap();
        let rows = draw(strip, true, (84, 3));
        let mut terminal = Terminal::new(TestBackend::new(84, 3)).unwrap();
        terminal
            .draw(|frame| render_strip(frame, frame.area(), (strip, true), hour(), &context))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let x = HISTORY_GUTTER_COLUMNS + 5;
        assert_eq!(
            buffer[(x, 2)].fg,
            context.theme.process_color(2),
            "{rows:#?}"
        );
        assert_ne!(buffer[(x, 0)].fg, context.theme.process_color(2));
    }

    #[test]
    fn a_rate_pair_scales_to_its_tallest_column() {
        let history = history_view();
        let panels = panels(&history);
        let disk = &panels[6];
        // One column a minute: the read's last minute averages 59 KiB/s,
        // the tallest column of the pair, below its 64 KiB/s window peak.
        assert_eq!(
            tallest_column(&disk.strips, hour(), 60),
            Some(59.0 * 1024.0)
        );
        assert_eq!(tallest_column(&disk.strips, hour(), 0), None);
    }

    #[test]
    fn a_loading_or_failed_query_says_so_in_the_legend() {
        let mut history = history_view();
        history.charts[0].pending = true;
        history.charts[1].error = Some("deadline".into());
        let panels = panels(&history);
        let loading = draw(&panels[0].strips[0], true, (84, 2));
        assert!(loading[0].trim_end().ends_with(LOADING), "{loading:#?}");
        let failed = draw(&panels[1].strips[0], true, (84, 2));
        assert!(
            failed[0].trim_end().ends_with("query failed"),
            "{failed:#?}"
        );
    }
}
