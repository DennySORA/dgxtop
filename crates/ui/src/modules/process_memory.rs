//! The tracked GPU processes on the memory bars, charts and legends
//! (D-64): each process's part in its color, the GPU memory of the other
//! processes in gray, and a legend of `■ name size` entries that fit.

use dgxtop_core::view::{ProcessShare, ProcessTrend};
use ratatui::style::Style;
use ratatui::text::Span;

use super::common::gap;
use crate::format::{clean, display_width, format_bytes};
use crate::registry::DisplayContext;
use crate::widgets::{Part, Segment};

/// Marker of a colored part, drawn in its color.
pub(crate) const SWATCH: &str = "■ ";
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

/// The legend: `■ train 6.0 GiB` for each share and, when `other` is
/// above zero, `■ other …` in gray, as many as fit `width` columns, then
/// `+N` for those left out when it fits.
pub(crate) fn legend(
    shares: &[ProcessShare],
    other: Option<u64>,
    width: usize,
    context: &DisplayContext,
) -> Vec<Span<'static>> {
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
    let spacing = display_width(&gap(context).content);
    let mut spans = Vec::new();
    let mut used = 0;
    for (index, entry) in entries.iter().enumerate() {
        let entry_width: usize = entry.iter().map(Span::width).sum();
        let left_out = entries.len() - index - 1;
        let more = if left_out > 0 {
            spacing + display_width(&format!("+{left_out}"))
        } else {
            0
        };
        if used + spacing + entry_width + more > width {
            let rest = format!("+{}", entries.len() - index);
            if used + spacing + display_width(&rest) <= width {
                spans.push(gap(context));
                spans.push(Span::styled(rest, theme.muted_style()));
            }
            break;
        }
        used += spacing + entry_width;
        spans.push(gap(context));
        spans.extend(entry.iter().cloned());
    }
    spans
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

    /// The legend keeps whole entries and counts the ones it leaves out.
    #[test]
    fn the_legend_keeps_whole_entries_and_counts_the_rest() {
        let context = crate::test_fixtures::display_context();
        let shares = [share(0, "train", 6), share(1, "serve", 2)];
        let wide = text(&legend(&shares, Some(GIB), 80, &context));
        assert_eq!(wide, "  ■ train 6.0 GiB  ■ serve 2.0 GiB  ■ other 1.0 GiB");
        let narrow = text(&legend(&shares, Some(GIB), 22, &context));
        assert_eq!(narrow, "  ■ train 6.0 GiB  +2");
        assert_eq!(text(&legend(&shares, Some(0), 4, &context)), "  +2");
        assert_eq!(text(&legend(&shares, Some(0), 3, &context)), "");
        assert_eq!(text(&legend(&[], None, 80, &context)), "");
        let colors: Vec<_> = legend(&shares, None, 80, &context)
            .iter()
            .filter(|span| span.content == SWATCH)
            .map(|span| span.style.fg)
            .collect();
        assert_eq!(
            colors,
            vec![
                Some(context.theme.process_color(0)),
                Some(context.theme.process_color(1))
            ]
        );
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
