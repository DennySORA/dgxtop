//! One-row gauge with a text label: a bar three quarters of a row high
//! (D-52), filled in the color of its level over a dim track of the same
//! height, so it never touches a neighbor. A missing ratio draws a dotted
//! track and `n/a` (or the caller's state text) instead of an empty or zero
//! bar.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;

use super::bar::BAR;
use crate::format::{NOT_AVAILABLE, display_width, truncate_to_width};
use crate::num::{saturate_u16, scaled_round};
use crate::theme::ThemePalette;

/// Horizontal bar followed by its label (`▆▆▆▆▆▆▆▆▆▆ 45.3%`).
#[derive(Debug, Clone)]
pub struct Gauge<'a> {
    ratio: Option<f64>,
    /// Where the value sits on the level colors, when that is not its fill.
    level: Option<f64>,
    label: &'a str,
    palette: &'a ThemePalette,
    label_style: Option<Style>,
}

impl<'a> Gauge<'a> {
    /// `ratio` is the 0..=1 fill; `None` means the value is missing.
    pub fn new(ratio: Option<f64>, label: &'a str, palette: &'a ThemePalette) -> Self {
        Self {
            ratio,
            level: None,
            label,
            palette,
            label_style: None,
        }
    }

    /// Colors the fill by `level` rather than by its share of the bar: a
    /// temperature stays green up to 50 °C (D-126).
    pub fn level(mut self, level: Option<f64>) -> Self {
        self.level = level;
        self
    }

    pub fn label_style(mut self, style: Style) -> Self {
        self.label_style = Some(style);
        self
    }

    fn effective_label(&self) -> &'a str {
        if self.ratio.is_none() && self.label.is_empty() {
            NOT_AVAILABLE
        } else {
            self.label
        }
    }

    fn render_bar(&self, bar: Rect, buf: &mut Buffer) {
        match self.ratio.filter(|ratio| ratio.is_finite()) {
            Some(ratio) => {
                let clamped = ratio.clamp(0.0, 1.0);
                let level = self.level.unwrap_or(clamped);
                let fill = Style::new().fg(self.palette.level_color(level));
                let track = Style::new().fg(self.palette.gauge_track);
                // Whole cells: a half cell of a bar lower than its row would
                // leave a notch in the track.
                let filled = scaled_round(clamped * f64::from(bar.width), bar.width);
                for offset in 0..bar.width {
                    let style = if offset < filled { fill } else { track };
                    buf.set_string(bar.x + offset, bar.y, BAR, style);
                }
            }
            None => {
                let style = self.palette.muted_style();
                for offset in 0..bar.width {
                    buf.set_string(bar.x + offset, bar.y, "·", style);
                }
            }
        }
    }
}

impl Widget for Gauge<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let label = self.effective_label();
        let label_width = saturate_u16(display_width(label));
        let reserved = if label_width == 0 {
            0
        } else {
            label_width.saturating_add(1)
        };
        let bar_width = area.width.saturating_sub(reserved);
        let bar = Rect::new(area.x, area.y, bar_width, 1);
        self.render_bar(bar, buf);
        let gap = u16::from(bar_width > 0);
        let label_x = area.x.saturating_add(bar_width).saturating_add(gap);
        let label_room = area.right().saturating_sub(label_x);
        if label_room == 0 {
            return;
        }
        let style = self.label_style.unwrap_or_else(|| {
            if self.ratio.is_some() {
                self.palette.text_style()
            } else {
                self.palette.muted_style()
            }
        });
        let fitted = truncate_to_width(label, usize::from(label_room));
        buf.set_string(label_x, area.y, fitted, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(buffer: &Buffer, width: u16) -> String {
        (0..width)
            .map(|x| buffer[(x, 0)].symbol().to_owned())
            .collect()
    }

    #[test]
    fn half_filled_gauge_is_a_band_over_its_track_with_a_label() {
        let palette = ThemePalette::default();
        let area = Rect::new(0, 0, 16, 1);
        let mut buffer = Buffer::empty(area);
        Gauge::new(Some(0.5), "50.0%", &palette).render(area, &mut buffer);
        let text = row_text(&buffer, 16);
        assert_eq!(text, "▆▆▆▆▆▆▆▆▆▆ 50.0%", "{text}");
        // Ten cells of bar: five filled in the level color, five of track.
        let colors: Vec<_> = (0..10).map(|x| buffer[(x, 0)].fg).collect();
        assert_eq!(colors[..5], [palette.level_color(0.5); 5]);
        assert_eq!(colors[5..], [palette.gauge_track; 5]);
    }

    #[test]
    fn missing_ratio_shows_na_not_empty_bar() {
        let palette = ThemePalette::default();
        let area = Rect::new(0, 0, 12, 1);
        let mut buffer = Buffer::empty(area);
        Gauge::new(None, "", &palette).render(area, &mut buffer);
        let text = row_text(&buffer, 12);
        assert!(text.ends_with("n/a"), "{text}");
        assert!(!text.contains('▆') && !text.contains('█'), "{text}");
    }

    #[test]
    fn tiny_area_and_corrupt_ratio_do_not_panic() {
        let palette = ThemePalette::default();
        let area = Rect::new(0, 0, 2, 1);
        let mut buffer = Buffer::empty(area);
        Gauge::new(Some(f64::NAN), "unsupported", &palette).render(area, &mut buffer);
        Gauge::new(Some(7.0), "", &palette).render(Rect::new(0, 0, 0, 0), &mut buffer);
        let full = Rect::new(0, 0, 4, 1);
        let mut buffer = Buffer::empty(full);
        Gauge::new(Some(2.0), "", &palette).render(full, &mut buffer);
        assert_eq!(row_text(&buffer, 4), "▆▆▆▆");
        assert!((0..4).all(|x| buffer[(x, 0)].fg == palette.level_color(1.0)));
    }
}
