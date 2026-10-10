//! One-row bar split into colored segments, e.g. RAM held by the GPU, by
//! other programs and by the page cache. Segment edges come from cumulative
//! fractions, so rounding never makes the segments overrun the bar. Like
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

    /// End column (exclusive) of every segment within `width` cells.
    fn edges(&self, width: u16) -> Vec<u16> {
        let mut total = 0.0;
        self.segments
            .iter()
            .map(|segment| {
                let fraction = if segment.fraction.is_finite() {
                    segment.fraction.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                total = (total + fraction).min(1.0);
                cells(total, width)
            })
            .collect()
    }
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
        let edges = self.edges(area.width);
        let mut start = 0u16;
        for (segment, end) in self.segments.iter().zip(&edges) {
            for column in start..*end {
                buf.set_string(area.x + column, area.y, BAR, Style::new().fg(segment.color));
            }
            start = start.max(*end);
        }
        for column in start..area.width {
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
