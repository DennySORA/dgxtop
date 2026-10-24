//! One-row bar split into colored segments, e.g. RAM held by the GPU, by
//! other programs and by the page cache. Segment edges come from cumulative
//! fractions, so rounding never makes the segments overrun the bar, and a
//! share above zero keeps at least one cell, however small (D-133). Like
//! the gauge it stands three quarters of a row high over a dim track of the
//! same height (D-52).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

use super::bar::BAR;

/// A colored share of the whole bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    /// Share of the whole, clamped to `0.0..=1.0` (non-finite counts as 0).
    pub fraction: f64,
    pub color: Color,
}

/// The bar: segments left to right, the rest drawn as the track.
#[derive(Debug, Clone, PartialEq)]
pub struct StackedBar<'a> {
    segments: &'a [Segment],
    track: Color,
}

impl<'a> StackedBar<'a> {
    pub fn new(segments: &'a [Segment], track: Color) -> Self {
        Self { segments, track }
    }

    /// Cells of every segment within `width`: their edges come from the
    /// cumulative fractions, then a segment above zero that rounded to no
    /// cell takes one from the widest segment, or from the track while no
    /// segment has one to spare, so however small a share it shows.
    fn widths(&self, width: u16) -> Vec<u16> {
        let mut total = 0.0;
        let mut start = 0;
        let mut widths: Vec<u16> = self
            .segments
            .iter()
            .map(|segment| {
                total = (total + share(segment)).min(1.0);
                let end = cells(total, width);
                let cells = end.saturating_sub(start);
                start = start.max(end);
                cells
            })
            .collect();
        for index in 0..widths.len() {
            if widths[index] > 0 || share(&self.segments[index]) <= 0.0 {
                continue;
            }
            if let Some(widest) = widest_to_spare(&widths) {
                widths[widest] -= 1;
                widths[index] = 1;
            } else if widths.iter().sum::<u16>() < width {
                widths[index] = 1;
            }
        }
        widths
    }
}

/// A segment's share of the bar, clamped to `0.0..=1.0` (non-finite
/// counts as 0).
fn share(segment: &Segment) -> f64 {
    if segment.fraction.is_finite() {
        segment.fraction.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The widest segment with more than one cell, the first of a tie.
fn widest_to_spare(widths: &[u16]) -> Option<usize> {
    widths
        .iter()
        .enumerate()
        .filter(|(_, width)| **width > 1)
        .rev()
        .max_by_key(|(_, width)| **width)
        .map(|(index, _)| index)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn cells(fraction: f64, width: u16) -> u16 {
    (fraction * f64::from(width))
        .round()
        .clamp(0.0, f64::from(width)) as u16
}

impl Widget for StackedBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let widths = self.widths(area.width);
        let mut column = 0u16;
        for (segment, width) in self.segments.iter().zip(&widths) {
            for _ in 0..*width {
                buf.set_string(area.x + column, area.y, BAR, Style::new().fg(segment.color));
                column += 1;
            }
        }
        for column in column..area.width {
            buf.set_string(area.x + column, area.y, BAR, Style::new().fg(self.track));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(segments: &[Segment], width: u16) -> Buffer {
        let area = Rect::new(0, 0, width, 1);
        let mut buffer = Buffer::empty(area);
        StackedBar::new(segments, Color::DarkGray).render(area, &mut buffer);
        buffer
    }

    fn colors(buffer: &Buffer) -> Vec<Color> {
        (0..buffer.area.width).map(|x| buffer[(x, 0)].fg).collect()
    }

    #[test]
    fn segments_fill_their_shares_in_order_and_the_rest_is_track() {
        let segments = [
            Segment {
                fraction: 0.25,
                color: Color::Magenta,
            },
            Segment {
                fraction: 0.5,
                color: Color::Blue,
            },
        ];
        let buffer = draw(&segments, 8);
        let mut expected = vec![Color::Magenta; 2];
        expected.extend([Color::Blue; 4]);
        expected.extend([Color::DarkGray; 2]);
        assert_eq!(colors(&buffer), expected);
        assert_eq!(buffer[(0, 0)].symbol(), BAR);
        assert_eq!(buffer[(7, 0)].symbol(), BAR);
    }

    /// However small, a share above zero keeps one cell: taken from the
    /// widest segment, so the bar's length holds, or from the track when
    /// no segment has a cell to spare.
    #[test]
    fn a_share_above_zero_always_keeps_a_cell() {
        let segment = |fraction: f64, color: Color| Segment { fraction, color };
        let tiny = [
            segment(0.62, Color::Magenta),
            segment(0.02, Color::Blue),
            segment(0.3, Color::Green),
        ];
        let mut expected = vec![Color::Magenta; 5];
        expected.push(Color::Blue);
        expected.extend([Color::Green; 3]);
        expected.push(Color::DarkGray);
        assert_eq!(colors(&draw(&tiny, 10)), expected);
        let slivers = [segment(0.04, Color::Magenta), segment(0.04, Color::Blue)];
        let mut expected = vec![Color::Magenta, Color::Blue];
        expected.extend([Color::DarkGray; 8]);
        assert_eq!(colors(&draw(&slivers, 10)), expected);
        let full = [segment(0.98, Color::Magenta), segment(0.02, Color::Blue)];
        assert_eq!(colors(&draw(&full, 2)), vec![Color::Magenta, Color::Blue]);
    }

    #[test]
    fn shares_over_the_whole_never_overrun_and_bad_values_are_empty() {
        let segments = [
            Segment {
                fraction: 0.7,
                color: Color::Magenta,
            },
            Segment {
                fraction: 0.7,
                color: Color::Blue,
            },
            Segment {
                fraction: f64::NAN,
                color: Color::Red,
            },
            Segment {
                fraction: -1.0,
                color: Color::Green,
            },
        ];
        let buffer = draw(&segments, 10);
        let drawn = colors(&buffer);
        assert_eq!(drawn.iter().filter(|c| **c == Color::Magenta).count(), 7);
        assert_eq!(drawn.iter().filter(|c| **c == Color::Blue).count(), 3);
        assert!(!drawn.contains(&Color::Red) && !drawn.contains(&Color::Green));
        assert!(!drawn.contains(&Color::DarkGray));
    }
}
