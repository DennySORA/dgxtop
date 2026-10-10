//! Color palettes for the four configured themes (`dark`, `cyan`, `green`,
//! `amber`). Color is only an extra cue: every state is also spelled out in
//! text, so a monochrome terminal loses nothing semantically.

use dgxtop_core::config::Theme;
use dgxtop_core::view::BannerLevel;
use ratatui::style::{Color, Modifier, Style};

use crate::format::CellTone;

/// Ratio above which gauges switch to the "mid" color.
const MID_LEVEL: f64 = 0.70;
/// Ratio above which gauges switch to the "high" color.
const HIGH_LEVEL: f64 = 0.90;

/// Resolved colors of one theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePalette {
    pub theme: Theme,
    pub primary: Color,
    pub secondary: Color,
    pub accent: Color,
    pub text: Color,
    pub text_dim: Color,
    pub text_muted: Color,
    pub border: Color,
    pub border_focus: Color,
    pub bar_background: Color,
    pub highlight_background: Color,
    pub tab_active_foreground: Color,
    pub tab_active_background: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub gauge_low: Color,
    pub gauge_mid: Color,
    pub gauge_high: Color,
    pub gauge_track: Color,
    pub sparkline: Color,
}

impl ThemePalette {
    /// Palette of a configured theme.
    pub const fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self::dark(),
            Theme::Cyan => Self::cyan(),
            Theme::Green => Self::green(),
            Theme::Amber => Self::amber(),
        }
    }

    const fn dark() -> Self {
        Self {
            theme: Theme::Dark,
            primary: Color::Rgb(120, 170, 255),
            secondary: Color::Rgb(150, 150, 210),
            accent: Color::Rgb(200, 140, 230),
            text: Color::Rgb(220, 220, 220),
            text_dim: Color::Rgb(150, 150, 150),
            text_muted: Color::Rgb(95, 95, 95),
            border: Color::Rgb(70, 70, 76),
            border_focus: Color::Rgb(120, 170, 255),
            bar_background: Color::Rgb(24, 24, 28),
            highlight_background: Color::Rgb(45, 50, 64),
            tab_active_foreground: Color::Rgb(15, 15, 20),
            tab_active_background: Color::Rgb(120, 170, 255),
            success: Color::Rgb(90, 200, 120),
            warning: Color::Rgb(230, 180, 60),
            danger: Color::Rgb(225, 70, 70),
            gauge_low: Color::Rgb(90, 200, 120),
            gauge_mid: Color::Rgb(230, 180, 60),
            gauge_high: Color::Rgb(225, 70, 70),
            gauge_track: Color::Rgb(48, 48, 54),
            sparkline: Color::Rgb(110, 150, 230),
        }
    }

    const fn cyan() -> Self {
        Self {
            theme: Theme::Cyan,
            primary: Color::Rgb(0, 210, 210),
            secondary: Color::Rgb(80, 120, 200),
            accent: Color::Rgb(180, 100, 220),
            text: Color::Rgb(220, 220, 220),
            text_dim: Color::Rgb(140, 140, 140),
            text_muted: Color::Rgb(90, 90, 90),
            border: Color::Rgb(60, 65, 70),
            border_focus: Color::Rgb(0, 210, 210),
            bar_background: Color::Rgb(20, 25, 32),
            highlight_background: Color::Rgb(40, 50, 60),
            tab_active_foreground: Color::Rgb(15, 15, 20),
            tab_active_background: Color::Rgb(0, 210, 210),
            success: Color::Rgb(80, 200, 120),
            warning: Color::Rgb(230, 180, 40),
            danger: Color::Rgb(220, 60, 60),
            gauge_low: Color::Rgb(80, 200, 120),
            gauge_mid: Color::Rgb(230, 180, 40),
            gauge_high: Color::Rgb(220, 60, 60),
            gauge_track: Color::Rgb(40, 42, 46),
            sparkline: Color::Rgb(0, 160, 180),
        }
    }

    const fn green() -> Self {
        Self {
            theme: Theme::Green,
            primary: Color::Rgb(80, 220, 100),
            secondary: Color::Rgb(0, 180, 180),
            accent: Color::Rgb(230, 180, 40),
            text: Color::Rgb(220, 220, 220),
            text_dim: Color::Rgb(140, 140, 140),
            text_muted: Color::Rgb(90, 90, 90),
            border: Color::Rgb(55, 65, 55),
            border_focus: Color::Rgb(80, 220, 100),
            bar_background: Color::Rgb(18, 24, 18),
            highlight_background: Color::Rgb(35, 50, 35),
            tab_active_foreground: Color::Rgb(15, 15, 15),
            tab_active_background: Color::Rgb(80, 220, 100),
            success: Color::Rgb(80, 220, 100),
            warning: Color::Rgb(230, 180, 40),
            danger: Color::Rgb(220, 60, 60),
            gauge_low: Color::Rgb(80, 220, 100),
            gauge_mid: Color::Rgb(230, 180, 40),
            gauge_high: Color::Rgb(220, 60, 60),
            gauge_track: Color::Rgb(38, 42, 38),
            sparkline: Color::Rgb(60, 180, 80),
        }
    }

    const fn amber() -> Self {
        Self {
            theme: Theme::Amber,
            primary: Color::Rgb(240, 180, 40),
            secondary: Color::Rgb(220, 120, 20),
            accent: Color::Rgb(0, 180, 200),
            text: Color::Rgb(220, 220, 220),
            text_dim: Color::Rgb(140, 140, 140),
            text_muted: Color::Rgb(90, 90, 90),
            border: Color::Rgb(65, 58, 45),
            border_focus: Color::Rgb(240, 180, 40),
            bar_background: Color::Rgb(24, 20, 14),
            highlight_background: Color::Rgb(50, 42, 28),
            tab_active_foreground: Color::Rgb(15, 12, 5),
            tab_active_background: Color::Rgb(240, 180, 40),
            success: Color::Rgb(80, 200, 120),
            warning: Color::Rgb(240, 180, 40),
            danger: Color::Rgb(220, 60, 60),
            gauge_low: Color::Rgb(80, 200, 120),
            gauge_mid: Color::Rgb(240, 180, 40),
            gauge_high: Color::Rgb(220, 60, 60),
            gauge_track: Color::Rgb(42, 40, 36),
            sparkline: Color::Rgb(200, 150, 30),
        }
    }

    /// Gauge color for a 0..=1 fill ratio.
    pub fn level_color(&self, ratio: f64) -> Color {
        if ratio >= HIGH_LEVEL {
            self.gauge_high
        } else if ratio >= MID_LEVEL {
            self.gauge_mid
        } else {
            self.gauge_low
        }
    }

    /// Style of a formatted cell; the tone text itself carries the meaning.
    pub fn tone_style(&self, tone: CellTone) -> Style {
        match tone {
            CellTone::Normal => Style::new().fg(self.text),
            CellTone::Fallback | CellTone::Stale => Style::new().fg(self.warning),
            // Dim only: tmux with a `screen` terminal shows italic as
            // reverse video, a gray block over the padded cell.
            CellTone::Missing => Style::new().fg(self.text_dim),
            CellTone::Conflict => Style::new().fg(self.danger).add_modifier(Modifier::BOLD),
        }
    }

    /// Style of a banner line of the given level.
    pub fn banner_style(&self, level: BannerLevel) -> Style {
        match level {
            BannerLevel::Info => Style::new().fg(self.text).bg(self.bar_background),
            BannerLevel::Warning => Style::new().fg(self.warning).bg(self.bar_background),
            BannerLevel::Error => Style::new()
                .fg(self.danger)
                .bg(self.bar_background)
                .add_modifier(Modifier::BOLD),
            BannerLevel::Confirmation => Style::new()
                .fg(self.tab_active_foreground)
                .bg(self.warning)
                .add_modifier(Modifier::BOLD),
        }
    }

    pub fn title_style(&self) -> Style {
        Style::new().fg(self.primary).add_modifier(Modifier::BOLD)
    }

    pub fn label_style(&self) -> Style {
        Style::new().fg(self.text_dim)
    }

    pub fn muted_style(&self) -> Style {
        Style::new().fg(self.text_muted)
    }

    pub fn text_style(&self) -> Style {
        Style::new().fg(self.text)
    }

    pub fn header_style(&self) -> Style {
        Style::new().fg(self.text_dim).add_modifier(Modifier::BOLD)
    }

    pub fn selected_row_style(&self) -> Style {
        Style::new()
            .fg(self.text)
            .bg(self.highlight_background)
            .add_modifier(Modifier::BOLD)
    }

    pub fn secondary_style(&self) -> Style {
        Style::new().fg(self.secondary)
    }

    pub fn border_style(&self, focused: bool) -> Style {
        Style::new().fg(if focused {
            self.border_focus
        } else {
            self.border
        })
    }
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self::for_theme(Theme::Dark)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_value_is_dim_never_italic() {
        // tmux with a `screen` terminal shows italic as reverse video: an
        // italic `n/a` padded to its column became a gray block.
        for theme in [Theme::Dark, Theme::Cyan, Theme::Green, Theme::Amber] {
            let palette = ThemePalette::for_theme(theme);
            let style = palette.tone_style(CellTone::Missing);
            assert_eq!(style.fg, Some(palette.text_dim));
            assert!(!style.add_modifier.contains(Modifier::ITALIC));
            assert!(!style.add_modifier.contains(Modifier::REVERSED));
        }
    }

    #[test]
    fn level_colors_follow_thresholds() {
        let palette = ThemePalette::for_theme(Theme::Cyan);
        assert_eq!(palette.level_color(0.1), palette.gauge_low);
        assert_eq!(palette.level_color(0.75), palette.gauge_mid);
        assert_eq!(palette.level_color(0.95), palette.gauge_high);
    }

    #[test]
    fn conflict_tone_is_emphasized() {
        let palette = ThemePalette::default();
        let style = palette.tone_style(CellTone::Conflict);
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(style.fg, Some(palette.danger));
    }
}
