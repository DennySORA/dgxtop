use ratatui::style::Color;

/// Color theme for the entire UI.
#[derive(Debug, Clone)]
pub struct Theme {
    pub primary: Color,
    pub secondary: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub text: Color,
    pub text_dim: Color,
    pub text_muted: Color,
    pub border: Color,
    pub border_active: Color,
    pub background: Color,
    pub highlight_bg: Color,
    pub gauge_low: Color,
    pub gauge_mid: Color,
    pub gauge_high: Color,
}

impl Theme {
    pub fn from_name(name: &str) -> Self {
        match name {
            "green" => Self::green(),
            "amber" => Self::amber(),
            _ => Self::cyan(),
        }
    }

    pub fn cyan() -> Self {
        Self {
            primary: Color::Cyan,
            secondary: Color::Blue,
            accent: Color::Magenta,
            success: Color::Green,
            warning: Color::Yellow,
            danger: Color::Red,
            text: Color::White,
            text_dim: Color::Gray,
            text_muted: Color::DarkGray,
            border: Color::DarkGray,
            border_active: Color::Cyan,
            background: Color::Reset,
            highlight_bg: Color::Indexed(236),
            gauge_low: Color::Green,
            gauge_mid: Color::Yellow,
            gauge_high: Color::Red,
        }
    }

    pub fn green() -> Self {
        Self {
            primary: Color::Green,
            secondary: Color::Cyan,
            accent: Color::Yellow,
            success: Color::Green,
            warning: Color::Yellow,
            danger: Color::Red,
            text: Color::White,
            text_dim: Color::Gray,
            text_muted: Color::DarkGray,
            border: Color::DarkGray,
            border_active: Color::Green,
            background: Color::Reset,
            highlight_bg: Color::Indexed(236),
            gauge_low: Color::Green,
            gauge_mid: Color::Yellow,
            gauge_high: Color::Red,
        }
    }

    pub fn amber() -> Self {
        Self {
            primary: Color::Yellow,
            secondary: Color::Indexed(208),
            accent: Color::Cyan,
            success: Color::Green,
            warning: Color::Yellow,
            danger: Color::Red,
            text: Color::White,
            text_dim: Color::Gray,
            text_muted: Color::DarkGray,
            border: Color::DarkGray,
            border_active: Color::Yellow,
            background: Color::Reset,
            highlight_bg: Color::Indexed(236),
            gauge_low: Color::Green,
            gauge_mid: Color::Yellow,
            gauge_high: Color::Red,
        }
    }

    /// Return the appropriate color for a percentage value (0-100).
    pub fn percent_color(&self, percent: f64) -> Color {
        if percent >= 90.0 {
            self.gauge_high
        } else if percent >= 70.0 {
            self.gauge_mid
        } else {
            self.gauge_low
        }
    }

    /// Return the appropriate color for temperature.
    pub fn temp_color(&self, celsius: f64) -> Color {
        if celsius >= 85.0 {
            self.danger
        } else if celsius >= 70.0 {
            self.warning
        } else {
            self.success
        }
    }
}
