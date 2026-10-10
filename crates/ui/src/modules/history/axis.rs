//! The history view's time axis (D-50): under each column of panels, a
//! label at every tick of the window, from `-1h` at the plot's left edge to
//! `now` at its right, at a step that gives the window about six ticks
//! (every minute of 5 minutes, every 10 minutes of an hour, every 4 hours
//! of a day). A label that would touch its right neighbour is left out.

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::format::{display_width, format_span_ns};
use crate::num::saturate_u16;
use crate::registry::DisplayContext;

/// Tick steps in seconds, from the finest.
const STEPS: [u64; 11] = [
    60, 120, 300, 600, 900, 1_800, 3_600, 7_200, 10_800, 14_400, 21_600,
];
/// Most ticks a window gets besides `now`.
const MAX_TICKS: u64 = 6;

/// A tick: its column in the plot and its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Tick {
    pub(super) column: u16,
    pub(super) label: String,
}

/// The ticks of a `span_seconds` window over `width` columns, from `now`
/// at the right edge back to the window's start.
pub(super) fn ticks(span_seconds: u64, width: u16) -> Vec<Tick> {
    if width == 0 || span_seconds == 0 {
        return Vec::new();
    }
    let step = STEPS
        .into_iter()
        .find(|step| span_seconds / step <= MAX_TICKS)
        .unwrap_or(span_seconds);
    let last = u64::from(width - 1);
    (0..=span_seconds / step)
        .map(|count| {
            let ago = count * step;
            let column = last - (ago * last).div_ceil(span_seconds).min(last);
            let label = if ago == 0 {
                "now".to_owned()
            } else {
                format!("-{}", format_span_ns(ago.saturating_mul(1_000_000_000)))
            };
            Tick {
                column: u16::try_from(column).unwrap_or(u16::MAX),
                label,
            }
        })
        .collect()
}

/// Draws the ticks of `span_seconds` under a plot `width` columns wide
/// starting at `plot_x` of `row`.
pub(super) fn render_axis(
    frame: &mut Frame<'_>,
    row: Rect,
    (plot_x, width): (u16, u16),
    span_seconds: u64,
    context: &DisplayContext,
) {
    let style = context.theme.muted_style();
    let right_edge = (plot_x + width).min(row.right());
    // Right to left, so `now` and the recent ticks win when crowded.
    let mut free_until = right_edge;
    for tick in ticks(span_seconds, width) {
        let label_width = saturate_u16(display_width(&tick.label));
        let center = plot_x + tick.column;
        let x = center
            .saturating_sub(label_width / 2)
            .clamp(plot_x, right_edge.saturating_sub(label_width));
        if x + label_width > free_until || x < row.x {
            continue;
        }
        frame.buffer_mut().set_string(x, row.y, &tick.label, style);
        free_until = x.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(span_seconds: u64) -> Vec<String> {
        ticks(span_seconds, 80)
            .into_iter()
            .map(|tick| tick.label)
            .collect()
    }

    #[test]
    fn each_window_gets_about_six_ticks_at_round_steps() {
        assert_eq!(labels(300), vec!["now", "-1m", "-2m", "-3m", "-4m", "-5m"]);
        assert_eq!(
            labels(3_600),
            vec!["now", "-10m", "-20m", "-30m", "-40m", "-50m", "-1h"]
        );
        assert_eq!(labels(21_600).last().map(String::as_str), Some("-6h"));
        assert_eq!(labels(43_200)[1], "-2h");
        assert_eq!(labels(86_400)[1], "-4h");
        assert_eq!(labels(86_400).len(), 7);
        // Columns run from the right edge to the left.
        let hour = ticks(3_600, 80);
        assert_eq!(hour[0].column, 79);
        assert_eq!(hour[6].column, 0);
        assert!(hour.windows(2).all(|pair| pair[0].column > pair[1].column));
        assert!(ticks(3_600, 0).is_empty());
    }
}
