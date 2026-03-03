use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

/// A gauge widget with gradient coloring based on value thresholds.
/// Renders a horizontal bar with filled/empty segments.
pub struct GradientGauge<'a> {
    ratio: f64,
    label: &'a str,
    low_color: Color,
    mid_color: Color,
    high_color: Color,
    bg_char: char,
    fill_char: char,
}

impl<'a> GradientGauge<'a> {
    pub fn new(ratio: f64) -> Self {
        Self {
            ratio: ratio.clamp(0.0, 1.0),
            label: "",
            low_color: Color::Green,
            mid_color: Color::Yellow,
            high_color: Color::Red,
            bg_char: '░',
            fill_char: '█',
        }
    }

    pub fn label(mut self, label: &'a str) -> Self {
        self.label = label;
        self
    }

    pub fn colors(mut self, low: Color, mid: Color, high: Color) -> Self {
        self.low_color = low;
        self.mid_color = mid;
        self.high_color = high;
        self
    }

    fn bar_color(&self) -> Color {
        let pct = self.ratio * 100.0;
        if pct >= 90.0 {
            self.high_color
        } else if pct >= 70.0 {
            self.mid_color
        } else {
            self.low_color
        }
    }
}

impl Widget for GradientGauge<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let label_width = if self.label.is_empty() {
            0
        } else {
            self.label.len() as u16 + 1
        };

        let bar_area_width = area.width.saturating_sub(label_width);
        if bar_area_width == 0 {
            return;
        }

        // Render label
        if !self.label.is_empty() {
            let label_x = area.x + bar_area_width + 1;
            buf.set_string(
                label_x.min(area.x + area.width - 1),
                area.y,
                self.label,
                Style::default().fg(Color::White),
            );
        }

        // Render bar
        let filled = (bar_area_width as f64 * self.ratio).round() as u16;
        let bar_color = self.bar_color();

        for x in 0..bar_area_width {
            let ch = if x < filled {
                self.fill_char
            } else {
                self.bg_char
            };
            let style = if x < filled {
                Style::default().fg(bar_color)
            } else {
                Style::default().fg(Color::Indexed(238))
            };
            buf.set_string(area.x + x, area.y, ch.to_string(), style);
        }
    }
}
