//! The tracked GPU processes on the memory bars, charts and legends
//! (D-64): each process's part in its color, the GPU memory of the other
//! processes in gray, and a `[GPU]` legend of `■ name size` entries wrapped
//! onto the lines it may take (D-69).

use dgxtop_core::view::{ProcessShare, ProcessTrend};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::common::gap;
use crate::format::{clean, display_width, format_bytes};
use crate::registry::DisplayContext;
use crate::widgets::{Part, Segment};

/// Marker of a colored part, drawn in its color.
pub(crate) const SWATCH: &str = "■ ";
/// Leads the GPU legend: the memory GPU processes hold.
pub(crate) const GPU_TAG: &str = "[GPU] ";
/// Name of the GPU processes without a color of their own.
const OTHER: &str = "other";

/// Segments of `shares` on a bar whose whole is `scale` bytes, in slot
/// order, together at most `room` bytes (what the bar shows of GPU memory),
/// and the bytes they cover.
pub(crate) fn share_segments(
    shares: &[ProcessShare],
    scale: f64,
    room: f64,
    context: &DisplayContext,
) -> (Vec<Segment>, f64) {
    let mut left = room.max(0.0);
    let mut segments = Vec::with_capacity(shares.len());
    for share in shares {
        let bytes = bytes_f64(share.bytes).min(left);
        left -= bytes;
        segments.push(Segment {
            fraction: bytes / scale,
            color: context.theme.process_color(share.slot),
        });
    }
    (segments, room.max(0.0) - left)
}

/// The GPU legend (D-69): `[GPU] ■ train 6.0 GiB  ■ serve 2.0 GiB …`, each
/// share and, when `other` is above zero, `■ other …` in gray, wrapped onto
/// at most `max_lines` lines of `width` columns, the later ones indented
/// under the first entry; `+N` ends the last line for those left out.
pub(crate) fn legend_lines(
    shares: &[ProcessShare],
    other: Option<u64>,
    width: usize,
    max_lines: usize,
    context: &DisplayContext,
) -> Vec<Line<'static>> {
    let theme = &context.theme;
    let mut entries: Vec<[Span<'static>; 3]> = shares
        .iter()
        .map(|share| {
            entry(
                &clean(&share.name),
                share.bytes,
                theme.process_color(share.slot),
                context,
            )
        })
        .collect();
    if let Some(other) = other.filter(|bytes| *bytes > 0) {
        entries.push(entry(OTHER, other, theme.other_processes_color(), context));
    }
    if entries.is_empty() || max_lines == 0 {
        return Vec::new();
    }
    let lead = display_width(GPU_TAG);
    let gap_width = display_width(&gap(context).content);
    let entry_width = |entry: &[Span<'static>; 3]| -> usize { entry.iter().map(Span::width).sum() };
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut index = 0;
    while index < entries.len() && lines.len() < max_lines {
        let last = lines.len() + 1 == max_lines;
        let mut spans = vec![if lines.is_empty() {
            Span::styled(GPU_TAG, theme.label_style())
        } else {
            Span::raw(" ".repeat(lead))
        }];
        let mut used = lead;
        let mut placed = 0;
        while index < entries.len() {
            let spacing = if placed == 0 { 0 } else { gap_width };
            let left_after = entries.len() - index - 1;
            let more = if last && left_after > 0 {
                gap_width + display_width(&format!("+{left_after}"))
            } else {
                0
            };
            let fits = used + spacing + entry_width(&entries[index]) + more <= width;
            if !fits && placed > 0 {
                break;
            }
            if !fits && last {
                break;
            }
            if spacing > 0 {
                spans.push(gap(context));
            }
            used += spacing + entry_width(&entries[index]);
            spans.extend(entries[index].iter().cloned());
            placed += 1;
            index += 1;
        }
        if last && index < entries.len() {
            if placed > 0 {
                spans.push(gap(context));
            }
            spans.push(Span::styled(
                format!("+{}", entries.len() - index),
                theme.muted_style(),
            ));
            index = entries.len();
        }
        lines.push(spans);
    }
    lines.into_iter().map(Line::from).collect()
}

/// Lines [`legend_lines`] takes for everything at `width`, at most
/// `max_lines`.
pub(crate) fn legend_line_count(
    shares: &[ProcessShare],
    other: Option<u64>,
    width: usize,
    max_lines: usize,
    context: &DisplayContext,
) -> usize {
    legend_lines(shares, other, width, max_lines, context).len()
}

/// `■ name size`, the marker in `color`.
fn entry(
    name: &str,
    bytes: u64,
    color: ratatui::style::Color,
    context: &DisplayContext,
) -> [Span<'static>; 3] {
    [
        Span::styled(SWATCH, Style::new().fg(color)),
        Span::styled(format!("{name} "), context.theme.label_style()),
        Span::styled(format_bytes(bytes), context.theme.text_style()),
    ]
}

/// Chart parts of `trends`, each in its process's color.
pub(crate) fn trend_parts<'a>(
    trends: &'a [ProcessTrend],
    context: &DisplayContext,
) -> Vec<Part<'a>> {
    trends
        .iter()
        .map(|part| {
            (
                part.trend.points.as_slice(),
                Style::new().fg(context.theme.process_color(part.slot)),
            )
        })
        .collect()
}

/// What the other GPU processes hold at each point of `whole` (what all
/// of them hold): `whole` less the tracked processes' `trends`, aligned at
/// the newest end; `None` where `whole` has no point.
pub(crate) fn other_points(whole: &[Option<f64>], trends: &[ProcessTrend]) -> Vec<Option<f64>> {
    whole
        .iter()
        .enumerate()
        .map(|(index, point)| {
            let from_end = whole.len() - index;
            let tracked: f64 = trends
                .iter()
                .filter_map(|part| {
                    let points = &part.trend.points;
                    let position = points.len().checked_sub(from_end)?;
                    points.get(position).copied().flatten()
                })
                .sum();
            point.map(|whole| (whole - tracked).max(0.0))
        })
        .collect()
}

fn bytes_f64(bytes: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64;
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::view::Trend;

    const GIB: u64 = 1 << 30;

    fn share(slot: u8, name: &str, gib: u64) -> ProcessShare {
        ProcessShare {
            slot,
            pid: i32::from(slot) + 100,
            name: name.to_owned(),
            bytes: gib * GIB,
        }
    }

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    /// Segments follow the slots' colors and never cover more than the
    /// room the bar has for GPU memory.
    #[test]
    fn segments_are_in_slot_colors_and_capped_by_the_room() {
        let context = crate::test_fixtures::display_context();
        let shares = [share(0, "train", 6), share(2, "serve", 4)];
        let scale = bytes_f64(20 * GIB);
        let (segments, covered) = share_segments(&shares, scale, bytes_f64(8 * GIB), &context);
        assert_eq!(
            segments
                .iter()
                .map(|segment| segment.color)
                .collect::<Vec<_>>(),
            vec![
                context.theme.process_color(0),
                context.theme.process_color(2)
            ]
        );
        assert!((segments[0].fraction - 0.3).abs() < 1e-12);
        assert!(
            (segments[1].fraction - 0.1).abs() < 1e-12,
            "capped at 8 GiB"
        );
        assert!((covered - bytes_f64(8 * GIB)).abs() < 1.0);
    }

    /// The GPU legend leads with `[GPU]` once, wraps whole entries onto
    /// its lines under the first one and counts the ones left out (D-69).
    #[test]
    fn the_gpu_legend_wraps_whole_entries_and_counts_the_rest() {
        let context = crate::test_fixtures::display_context();
        let shares = [
            share(0, "train", 6),
            share(1, "serve", 2),
            share(2, "notebook", 1),
        ];
        let lines = |width: usize, max: usize| -> Vec<String> {
            legend_lines(&shares, Some(GIB), width, max, &context)
                .iter()
                .map(|line| text(&line.spans))
                .collect()
        };
        assert_eq!(
            lines(80, 3),
            vec!["[GPU] ■ train 6.0 GiB  ■ serve 2.0 GiB  ■ notebook 1.0 GiB  ■ other 1.0 GiB"]
        );
        assert_eq!(
            lines(42, 3),
            vec![
                "[GPU] ■ train 6.0 GiB  ■ serve 2.0 GiB",
                "      ■ notebook 1.0 GiB  ■ other 1.0 GiB",
            ]
        );
        assert_eq!(
            lines(30, 2),
            vec!["[GPU] ■ train 6.0 GiB", "      ■ serve 2.0 GiB  +2"]
        );
        assert_eq!(lines(10, 1), vec!["[GPU] +4"]);
        assert_eq!(legend_line_count(&shares, None, 42, 3, &context), 2);
        assert!(legend_lines(&[], None, 80, 3, &context).is_empty());
        let colors: Vec<_> = legend_lines(&shares, None, 80, 1, &context)[0]
            .spans
            .iter()
            .filter(|span| span.content == SWATCH)
            .map(|span| span.style.fg)
            .collect();
        assert_eq!(colors.len(), 3);
        assert_eq!(colors[0], Some(context.theme.process_color(0)));
    }

    #[test]
    fn the_other_processes_hold_the_rest_of_each_point() {
        let trend = |points: Vec<Option<f64>>| Trend {
            points,
            step_ms: 1000,
        };
        let trends = [
            ProcessTrend {
                slot: 0,
                trend: trend(vec![Some(4.0), Some(5.0)]),
            },
            ProcessTrend {
                slot: 1,
                trend: trend(vec![None, Some(9.0)]),
            },
        ];
        // The parts end with the whole; the oldest point has no part.
        let whole = [Some(3.0), Some(6.0), Some(12.0), None];
        assert_eq!(
            other_points(&whole[..3], &trends),
            vec![Some(3.0), Some(2.0), Some(0.0)]
        );
        assert_eq!(other_points(&whole, &trends)[3], None);
    }
}
