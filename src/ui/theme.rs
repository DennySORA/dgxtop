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
    pub header_bg: Color,
    pub tab_active_bg: Color,
    pub tab_active_fg: Color,
    pub gauge_low: Color,
    pub gauge_mid: Color,
    pub gauge_high: Color,
    pub gauge_bg: Color,
    pub sparkline_color: Color,
    pub row_alt_bg: Color,
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
            primary: Color::Rgb(0, 210, 210),
            secondary: Color::Rgb(80, 120, 200),
            accent: Color::Rgb(180, 100, 220),
            success: Color::Rgb(80, 200, 120),
            warning: Color::Rgb(230, 180, 40),
            danger: Color::Rgb(220, 60, 60),
            text: Color::Rgb(220, 220, 220),
            text_dim: Color::Rgb(140, 140, 140),
            text_muted: Color::Rgb(80, 80, 80),
            border: Color::Rgb(60, 65, 70),
            border_active: Color::Rgb(0, 210, 210),
            background: Color::Reset,
            highlight_bg: Color::Rgb(40, 50, 60),
            header_bg: Color::Rgb(20, 25, 32),
            tab_active_bg: Color::Rgb(0, 210, 210),
            tab_active_fg: Color::Rgb(15, 15, 20),
            gauge_low: Color::Rgb(80, 200, 120),
            gauge_mid: Color::Rgb(230, 180, 40),
            gauge_high: Color::Rgb(220, 60, 60),
            gauge_bg: Color::Rgb(40, 42, 46),
            sparkline_color: Color::Rgb(0, 160, 180),
            row_alt_bg: Color::Rgb(22, 24, 28),
        }
    }

    pub fn green() -> Self {
        Self {
            primary: Color::Rgb(80, 220, 100),
            secondary: Color::Rgb(0, 180, 180),
            accent: Color::Rgb(230, 180, 40),
            success: Color::Rgb(80, 220, 100),
            warning: Color::Rgb(230, 180, 40),
            danger: Color::Rgb(220, 60, 60),
            text: Color::Rgb(220, 220, 220),
            text_dim: Color::Rgb(140, 140, 140),
            text_muted: Color::Rgb(80, 80, 80),
            border: Color::Rgb(55, 65, 55),
            border_active: Color::Rgb(80, 220, 100),
            background: Color::Reset,
            highlight_bg: Color::Rgb(35, 50, 35),
            header_bg: Color::Rgb(18, 24, 18),
            tab_active_bg: Color::Rgb(80, 220, 100),
            tab_active_fg: Color::Rgb(15, 15, 15),
            gauge_low: Color::Rgb(80, 220, 100),
            gauge_mid: Color::Rgb(230, 180, 40),
            gauge_high: Color::Rgb(220, 60, 60),
            gauge_bg: Color::Rgb(38, 42, 38),
            sparkline_color: Color::Rgb(60, 180, 80),
            row_alt_bg: Color::Rgb(20, 26, 20),
        }
    }

    pub fn amber() -> Self {
        Self {
            primary: Color::Rgb(240, 180, 40),
            secondary: Color::Rgb(220, 120, 20),
            accent: Color::Rgb(0, 180, 200),
            success: Color::Rgb(80, 200, 120),
            warning: Color::Rgb(240, 180, 40),
            danger: Color::Rgb(220, 60, 60),
            text: Color::Rgb(220, 220, 220),
            text_dim: Color::Rgb(140, 140, 140),
            text_muted: Color::Rgb(80, 80, 80),
            border: Color::Rgb(65, 58, 45),
            border_active: Color::Rgb(240, 180, 40),
            background: Color::Reset,
            highlight_bg: Color::Rgb(50, 42, 28),
            header_bg: Color::Rgb(24, 20, 14),
            tab_active_bg: Color::Rgb(240, 180, 40),
            tab_active_fg: Color::Rgb(15, 12, 5),
            gauge_low: Color::Rgb(80, 200, 120),
            gauge_mid: Color::Rgb(240, 180, 40),
            gauge_high: Color::Rgb(220, 60, 60),
            gauge_bg: Color::Rgb(42, 40, 36),
            sparkline_color: Color::Rgb(200, 150, 30),
            row_alt_bg: Color::Rgb(26, 22, 16),
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
