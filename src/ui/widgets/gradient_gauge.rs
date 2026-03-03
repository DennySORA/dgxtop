use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

/// A polished gauge widget with smooth gradient coloring.
/// Uses half-block characters for sub-cell precision rendering.
pub struct GradientGauge<'a> {
    ratio: f64,
    label: &'a str,
    low_color: Color,
    mid_color: Color,
    high_color: Color,
    bg_color: Color,
    show_percentage: bool,
}

impl<'a> GradientGauge<'a> {
    pub fn new(ratio: f64) -> Self {
        Self {
            ratio: ratio.clamp(0.0, 1.0),
            label: "",
            low_color: Color::Rgb(80, 200, 120),
            mid_color: Color::Rgb(230, 180, 40),
            high_color: Color::Rgb(220, 60, 60),
            bg_color: Color::Rgb(40, 42, 46),
            show_percentage: false,
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

    pub fn bg_color(mut self, color: Color) -> Self {
        self.bg_color = color;
        self
    }

    pub fn show_percentage(mut self) -> Self {
        self.show_percentage = true;
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

        // Calculate label area
        let pct_label = if self.show_percentage {
            format!("{:>5.1}%", self.ratio * 100.0)
        } else {
            String::new()
        };

        let extra_label = self.label;
        let total_label_width = if !extra_label.is_empty() {
            extra_label.width() as u16 + 1
        } else if self.show_percentage {
            pct_label.len() as u16 + 1
        } else {
            0
        };

        let bar_width = area.width.saturating_sub(total_label_width);
        if bar_width == 0 {
            return;
        }

        // Render the bar with smooth half-block precision
        let filled_f = bar_width as f64 * self.ratio;
        let filled_full = filled_f as u16;
        let has_half = (filled_f - filled_full as f64) >= 0.5;
        let bar_color = self.bar_color();

        for x in 0..bar_width {
            let (ch, style) = if x < filled_full {
                ('█', Style::default().fg(bar_color))
            } else if x == filled_full && has_half {
                ('▌', Style::default().fg(bar_color))
            } else {
                ('─', Style::default().fg(self.bg_color))
            };
            buf.set_string(area.x + x, area.y, ch.to_string(), style);
        }

        // Render label to the right of the bar
        if !extra_label.is_empty() {
            buf.set_string(
                area.x + bar_width + 1,
                area.y,
                extra_label,
                Style::default().fg(Color::Rgb(180, 180, 180)),
            );
        } else if self.show_percentage {
            let pct_color = self.bar_color();
            buf.set_string(
                area.x + bar_width + 1,
                area.y,
                &pct_label,
                Style::default().fg(pct_color),
            );
        }
    }
}
