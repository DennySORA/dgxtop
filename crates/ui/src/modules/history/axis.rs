//! The history views' time axis (D-50, D-77, D-82): under each column of
//! panels, a label at every tick of the window on the local clock the bars
//! follow ([`clock_ticks`]): `14:00`, a day's first tick `10-11`, or for
//! day steps the dates, at a step that gives the window about six ticks.
//! The Overview's bars end with the one being filled now and a run's with
//! its latest, maybe days ago, so both read the clock rather than "ago".
//! A label that would touch its right neighbour is left out.

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::format::{display_width, format_local_clock, format_local_date};
use crate::num::saturate_u16;
use crate::registry::DisplayContext;

/// Most ticks a window gets besides its latest.
const MAX_TICKS: u64 = 6;
/// Clock tick steps in seconds, from the finest, to the 300 days of day
/// bars.
const CLOCK_STEPS: [u64; 18] = [
    60, 120, 300, 600, 900, 1_800, 3_600, 7_200, 10_800, 14_400, 21_600, 43_200, 86_400, 172_800,
    604_800, 1_209_600, 2_592_000, 7_776_000,
];
const SECONDS_PER_DAY: i64 = 86_400;
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// A tick: its column in the plot and its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Tick {
    pub(super) column: u16,
    pub(super) label: String,
}

/// The ticks of the window `[start, end)` of UTC nanoseconds over `width`
/// columns at local times on round steps, the latest first.
pub(super) fn clock_ticks((start, end): (i64, i64), offset_s: i32, width: u16) -> Vec<Tick> {
    let span_seconds = u64::try_from((end - start) / NANOS_PER_SECOND).unwrap_or(0);
    if width < 2 || span_seconds == 0 {
        return Vec::new();
    }
    let step = CLOCK_STEPS
        .into_iter()
        .find(|step| span_seconds / step <= MAX_TICKS)
        .unwrap_or(span_seconds);
    let step_ns = i64::try_from(step).unwrap_or(i64::MAX) * NANOS_PER_SECOND;
    let offset_ns = i64::from(offset_s) * NANOS_PER_SECOND;
    let last = i128::from(width - 1);
    let span = i128::from(end - start);
    let mut ticks = Vec::new();
    // The latest tick at or before the end, on the local clock.
    let mut tick = (end + offset_ns).div_euclid(step_ns) * step_ns - offset_ns;
    while tick >= start {
        let column = i128::from(tick - start) * last / span;
        let midnight = (tick + offset_ns).rem_euclid(SECONDS_PER_DAY * NANOS_PER_SECOND) == 0;
        let label = if step >= 86_400 || midnight {
            format_local_date(tick, offset_s)
        } else {
            format_local_clock(tick, offset_s)
        };
        ticks.push(Tick {
            column: u16::try_from(column).unwrap_or(u16::MAX),
            label,
        });
        tick -= step_ns;
    }
    ticks
}

/// Draws `ticks` (the rightmost first) under a plot `width` columns wide
/// starting at `plot_x` of `row`.
pub(super) fn render_axis(
    frame: &mut Frame<'_>,
    row: Rect,
    (plot_x, width): (u16, u16),
    ticks: Vec<Tick>,
    context: &DisplayContext,
) {
    let style = context.theme.muted_style();
    let right_edge = (plot_x + width).min(row.right());
    // Right to left, so the recent ticks win when crowded.
    let mut free_until = right_edge;
    for tick in ticks {
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

    const MINUTE: i64 = 60 * NANOS_PER_SECOND;
    /// 2026-10-04T00:00:00Z.
    const MIDNIGHT: i64 = 1_791_072_000_000_000_000;

    fn clock_labels(window: (i64, i64), offset_s: i32) -> Vec<(u16, String)> {
        clock_ticks(window, offset_s, 81)
            .into_iter()
            .map(|tick| (tick.column, tick.label))
            .collect()
    }

    /// A run's axis reads the local clock, a day's first tick its date.
    #[test]
    fn a_run_axis_reads_the_local_clock() {
        // 80 minutes to 09:30 at UTC+8: every 15 minutes.
        let end = MIDNIGHT + 90 * MINUTE;
        let labels = clock_labels((end - 80 * MINUTE, end), 8 * 3_600);
        assert_eq!(labels[0], (80, "09:30".to_owned()));
        assert_eq!(labels[1], (65, "09:15".to_owned()));
        assert_eq!(labels.last().unwrap().1, "08:15");
        // Two days: every 12 hours; midnights as dates.
        let days = clock_labels((MIDNIGHT - 2 * 24 * 60 * MINUTE, MIDNIGHT), 0);
        let texts: Vec<&str> = days.iter().map(|(_, label)| label.as_str()).collect();
        assert_eq!(texts, vec!["10-04", "12:00", "10-03", "12:00", "10-02"]);
        // Twenty days of day bars: weekly dates.
        let weeks = clock_labels((MIDNIGHT - 20 * 24 * 60 * MINUTE, MIDNIGHT), 0);
        assert!(
            weeks.iter().all(|(_, label)| label.contains('-')),
            "{weeks:?}"
        );
        // Sixty days of day bars (D-82): about six date ticks.
        let months = clock_labels((MIDNIGHT - 60 * 24 * 60 * MINUTE, MIDNIGHT), 0);
        assert!((4..=7).contains(&months.len()), "{months:?}");
        assert!(clock_ticks((0, 0), 0, 80).is_empty());
        assert!(clock_ticks((0, MINUTE), 0, 1).is_empty());
    }
}
