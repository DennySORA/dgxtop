//! Color palettes for the four configured themes (`dark`, `cyan`, `green`,
//! `amber`). Color is only an extra cue: every state is also spelled out in
//! text, so a monochrome terminal loses nothing semantically.

use dgxtop_core::config::Theme;
use dgxtop_core::view::{BannerLevel, PROCESS_COLOR_SLOTS, ViewModel};
use ratatui::style::{Color, Modifier, Style};

use crate::format::CellTone;

/// Fill ratio from which a gauge leaves the "low" color for the gradient
/// toward the redline (D-87).
const GRADIENT_START: f64 = 0.50;
/// The spec's default `viewer.redline_threshold_percent` (80 %), in
/// hundredths of a percent, for a palette no projection configured.
const DEFAULT_REDLINE_HUNDREDTHS: u16 = 8_000;
/// Hundredths of a percent in a whole fill (100 %).
const FULL_HUNDREDTHS: f64 = 10_000.0;

/// Colors of the tracked GPU processes, in slot order (D-64, D-65), the
/// same in every theme: teal, orange, magenta, blue, rose, violet, olive,
/// orchid. Validated as a categorical set on each theme's background
/// with the data-viz palette validator: any two colors, since any two can
/// meet in a bar, stay apart for full color vision (OKLab ΔE ≥ 16.0) and
/// under protanopia and deuteranopia (ΔE ≥ 7.9, so names always go beside
/// them); neighbors in this order (which processes of one name take,
/// wrapping around) at least ΔE 21.6 / 10.3; each is at least ΔE 12.3 /
/// 6.0 from the gauge levels, the tracks and the gray of other processes,
/// darker than the green and amber that say how full a bar is; each holds
/// 3:1 contrast.
const PROCESS_COLORS: [Color; PROCESS_COLOR_SLOTS] = [
    Color::Rgb(0x1e, 0x6b, 0x98),
    Color::Rgb(0xd3, 0x7c, 0x07),
    Color::Rgb(0xc9, 0x03, 0xa4),
    Color::Rgb(0x4a, 0x8d, 0xef),
    Color::Rgb(0x9e, 0x4a, 0x5f),
    Color::Rgb(0x8a, 0x3c, 0xe1),
    Color::Rgb(0x75, 0x73, 0x07),
    Color::Rgb(0xbc, 0x77, 0xbc),
];

/// Title colors of the resources (D-74), bright and the same in every
/// theme, so a resource's panels look alike on every page.
const SECTION_COLORS: [(&str, Color); 9] = [
    ("GPU Processes", Color::Rgb(0xff, 0x9e, 0x64)),
    ("CPU", Color::Rgb(0x7a, 0xa2, 0xf7)),
    ("Memory", Color::Rgb(0x9e, 0xce, 0x6a)),
    ("GPUs", Color::Rgb(0xe0, 0xaf, 0x68)),
    ("GPU", Color::Rgb(0xe0, 0xaf, 0x68)),
    ("Disk", Color::Rgb(0xbb, 0x9a, 0xf7)),
    ("Network", Color::Rgb(0x7d, 0xcf, 0xff)),
    ("History", Color::Rgb(0xf7, 0x76, 0x8e)),
    ("Settings", Color::Rgb(0xc0, 0xca, 0xf5)),
];

/// The color `position` (0..=1) of the way from `from` to `to`, mixed in
/// OKLab so the steps look even; a color that is not RGB stays `from`.
fn mix_oklab(from: Color, to: Color, position: f64) -> Color {
    let (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) = (from, to) else {
        return from;
    };
    let position = position.clamp(0.0, 1.0);
    let start = oklab([r0, g0, b0]);
    let end = oklab([r1, g1, b1]);
    let mixed: [f64; 3] =
        std::array::from_fn(|index| start[index] + (end[index] - start[index]) * position);
    let [r, g, b] = srgb_of_oklab(mixed);
    Color::Rgb(r, g, b)
}

/// OKLab of an sRGB color (Bjorn Ottosson's matrices).
fn oklab(rgb: [u8; 3]) -> [f64; 3] {
    let [r, g, b] = rgb.map(|channel| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    });
    let l = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
    let m = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
    let s = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
    [
        0.210_454_255_3 * l + 0.793_617_785_0 * m - 0.004_072_046_8 * s,
        1.977_998_495_1 * l - 2.428_592_205_0 * m + 0.450_593_709_9 * s,
        0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766_0 * s,
    ]
}

/// The sRGB color of an OKLab color, clamped into the gamut.
fn srgb_of_oklab([lightness, a, b]: [f64; 3]) -> [u8; 3] {
    let l = (lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b).powi(3);
    let m = (lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b).powi(3);
    let s = (lightness - 0.089_484_177_5 * a - 1.291_485_548_0 * b).powi(3);
    let linear = [
        4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s,
        -1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s,
        -0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701_0 * s,
    ];
    linear.map(|value| {
        let value = value.clamp(0.0, 1.0);
        let encoded = if value <= 0.003_130_8 {
            value * 12.92
        } else {
            1.055 * value.powf(1.0 / 2.4) - 0.055
        };
        // Clamped to 0..=255 first, so the cast is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let channel = (encoded * 255.0).round().clamp(0.0, 255.0) as u8;
        channel
    })
}

/// See [`ThemePalette::os_memory_color`].
const OS_MEMORY_COLOR: Color = Color::Rgb(0x5a, 0xc8, 0x78);

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
    /// `viewer.redline_threshold_percent` in hundredths of a percent: the
    /// fill from which a gauge is fully red (D-87). An integer so the
    /// palette stays comparable.
    pub redline_hundredths: u16,
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

    /// The palette a projection asks for: its theme and its redline.
    pub fn for_model(model: &ViewModel) -> Self {
        Self::for_theme(model.header.theme)
            .with_redline_percent(model.header.redline_threshold_percent)
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
            redline_hundredths: DEFAULT_REDLINE_HUNDREDTHS,
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
            redline_hundredths: DEFAULT_REDLINE_HUNDREDTHS,
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
            redline_hundredths: DEFAULT_REDLINE_HUNDREDTHS,
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
            redline_hundredths: DEFAULT_REDLINE_HUNDREDTHS,
        }
    }

    /// This palette with the redline at `percent` (the configured
    /// `viewer.redline_threshold_percent`), clamped to 0..=100; not a
    /// number keeps the default.
    pub fn with_redline_percent(mut self, percent: f64) -> Self {
        let hundredths = if percent.is_nan() {
            f64::from(DEFAULT_REDLINE_HUNDREDTHS)
        } else {
            (percent * 100.0).round().clamp(0.0, FULL_HUNDREDTHS)
        };
        // Clamped to 0..=10000 above, so the cast is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            self.redline_hundredths = hundredths as u16;
        }
        self
    }

    /// The redline as a 0..=1 fill ratio.
    pub fn redline_ratio(&self) -> f64 {
        f64::from(self.redline_hundredths) / FULL_HUNDREDTHS
    }

    /// Gauge color for a 0..=1 fill ratio (D-87): green below 50 %, from
    /// amber at 50 % toward red along an OKLab gradient, red from the
    /// redline up. A redline at or below 50 % leaves no gradient: red from
    /// the redline, green below it.
    pub fn level_color(&self, ratio: f64) -> Color {
        let redline = self.redline_ratio();
        if ratio >= redline {
            self.gauge_high
        } else if ratio >= GRADIENT_START {
            let position = (ratio - GRADIENT_START) / (redline - GRADIENT_START);
            mix_oklab(self.gauge_mid, self.gauge_high, position)
        } else {
            self.gauge_low
        }
    }

    /// Color of tracked GPU process `slot` (D-64).
    pub fn process_color(&self, slot: u8) -> Color {
        PROCESS_COLORS
            .get(usize::from(slot))
            .copied()
            .unwrap_or_else(|| self.other_processes_color())
    }

    /// GPU processes without a color of their own.
    pub fn other_processes_color(&self) -> Color {
        self.text_dim
    }

    /// What the system itself holds of a RAM shared with a GPU (D-69): a
    /// green, the same in every theme, at least OKLab ΔE 17.1 from every
    /// process color, the track and the gray (10.1 under protanopia and
    /// deuteranopia); a shared RAM's bar has no level colors to mistake it
    /// for.
    pub fn os_memory_color(&self) -> Color {
        OS_MEMORY_COLOR
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

    /// The title style of a panel or chart named `title`: its resource's
    /// fixed bright color (D-74), told by the title's first words (`GPU
    /// memory · GPU 0` is a GPU's); the theme's title style otherwise.
    pub fn section_title_style(&self, title: &str) -> Style {
        let title = title.trim_start();
        SECTION_COLORS
            .iter()
            .find(|(section, _)| {
                title
                    .strip_prefix(section)
                    .is_some_and(|rest| rest.is_empty() || !rest.starts_with(char::is_alphanumeric))
            })
            .map_or_else(
                || self.title_style(),
                |(_, color)| self.title_style().fg(*color),
            )
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

    fn rgb(color: Color) -> [u8; 3] {
        match color {
            Color::Rgb(r, g, b) => [r, g, b],
            other => panic!("not RGB: {other:?}"),
        }
    }

    /// D-87 at the spec's default redline (80 %): green below 50 %, the
    /// amber at 50 %, between amber and red at the midpoint, red from 80 %.
    #[test]
    fn level_colors_run_from_amber_at_half_to_red_at_the_redline() {
        let palette = ThemePalette::for_theme(Theme::Dark);
        assert_eq!(palette.redline_ratio(), 0.80);
        assert_eq!(palette.level_color(0.1), palette.gauge_low);
        assert_eq!(palette.level_color(0.49), palette.gauge_low);
        assert_eq!(palette.level_color(0.50), palette.gauge_mid);
        assert_eq!(rgb(palette.gauge_mid), [230, 180, 60]);
        assert_eq!(rgb(palette.gauge_high), [225, 70, 70]);
        let midpoint = rgb(palette.level_color(0.65));
        assert!(midpoint[1] < 180 && midpoint[1] > 70, "{midpoint:?}");
        assert_ne!(palette.level_color(0.65), palette.gauge_mid);
        assert_ne!(palette.level_color(0.65), palette.gauge_high);
        assert_eq!(palette.level_color(0.80), palette.gauge_high);
        assert_eq!(palette.level_color(0.95), palette.gauge_high);
        assert_eq!(palette.level_color(1.5), palette.gauge_high);
        assert_eq!(palette.level_color(f64::NAN), palette.gauge_low);
    }

    #[test]
    fn a_custom_redline_moves_the_red() {
        let palette = ThemePalette::for_theme(Theme::Cyan).with_redline_percent(90.0);
        assert_eq!(palette.redline_hundredths, 9_000);
        assert_ne!(palette.level_color(0.85), palette.gauge_high);
        assert_eq!(palette.level_color(0.90), palette.gauge_high);
        assert_eq!(
            palette.level_color(0.70),
            mix_oklab(palette.gauge_mid, palette.gauge_high, 0.5)
        );
        let edges = ThemePalette::default();
        assert_eq!(edges.with_redline_percent(250.0).redline_hundredths, 10_000);
        assert_eq!(edges.with_redline_percent(-1.0).redline_hundredths, 0);
        assert_eq!(
            edges.with_redline_percent(f64::NAN).redline_hundredths,
            8_000
        );
    }

    /// The viewer's palette takes the projection's redline, so an
    /// applied configuration recolors the next frame (D-87).
    #[test]
    fn the_palette_follows_the_projections_redline() {
        let mut model =
            crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::Overview, 1);
        let palette = ThemePalette::for_model(&model);
        assert_eq!(palette.level_color(0.85), palette.gauge_high);
        model.header.redline_threshold_percent = 90.0;
        model.header.theme = Theme::Green;
        let palette = ThemePalette::for_model(&model);
        assert_eq!(palette.theme, Theme::Green);
        assert_ne!(palette.level_color(0.85), palette.gauge_high);
        assert_eq!(palette.level_color(0.90), palette.gauge_high);
    }

    /// A redline at or below 50 % leaves no gradient: red from the redline.
    #[test]
    fn a_low_redline_is_red_from_the_redline() {
        for percent in [50.0, 30.0, 0.0] {
            let palette = ThemePalette::default().with_redline_percent(percent);
            let redline = percent / 100.0;
            assert_eq!(palette.level_color(redline), palette.gauge_high);
            assert_eq!(palette.level_color(0.6), palette.gauge_high);
            if redline > 0.0 {
                assert_eq!(palette.level_color(redline - 0.01), palette.gauge_low);
            }
        }
    }

    /// Between the band edges every step moves further from the amber
    /// and closer to the red, in every theme.
    #[test]
    fn the_gradient_is_monotonic() {
        let distance = |a: [f64; 3], b: [f64; 3]| {
            a.iter()
                .zip(b)
                .map(|(x, y)| (x - y).powi(2))
                .sum::<f64>()
                .sqrt()
        };
        for theme in [Theme::Dark, Theme::Cyan, Theme::Green, Theme::Amber] {
            let palette = ThemePalette::for_theme(theme);
            let amber = oklab(rgb(palette.gauge_mid));
            let red = oklab(rgb(palette.gauge_high));
            let mut previous = (0.0, distance(amber, red));
            for step in 0..=30 {
                let ratio = 0.50 + 0.30 * f64::from(step) / 30.0;
                let color = oklab(rgb(palette.level_color(ratio)));
                let now = (distance(amber, color), distance(color, red));
                // One 8-bit step of rounding is allowed.
                assert!(now.0 + 0.005 >= previous.0, "{theme:?} at {ratio}");
                assert!(now.1 <= previous.1 + 0.005, "{theme:?} at {ratio}");
                previous = now;
            }
        }
    }

    #[test]
    fn oklab_round_trips_srgb() {
        for color in [
            [0, 0, 0],
            [255, 255, 255],
            [230, 180, 60],
            [225, 70, 70],
            [30, 107, 152],
        ] {
            assert_eq!(srgb_of_oklab(oklab(color)), color);
        }
    }

    /// A process keeps one color in every theme; a slot past the palette
    /// is drawn like the other processes, never as a level color.
    /// A resource's titles share one fixed bright color on every page and
    /// in every theme (D-74).
    #[test]
    fn a_resources_titles_share_a_fixed_color() {
        let color = |theme: Theme, title: &str| {
            ThemePalette::for_theme(theme).section_title_style(title).fg
        };
        for theme in [Theme::Dark, Theme::Cyan, Theme::Green, Theme::Amber] {
            assert_eq!(
                color(theme, "GPUs  1x"),
                color(Theme::Dark, "GPU memory · GPU 0")
            );
            assert_eq!(
                color(theme, "GPU 0 · NVIDIA GB10"),
                color(Theme::Dark, "GPU utilization")
            );
            assert_eq!(
                color(theme, "CPU ▸ Grace"),
                color(Theme::Dark, "CPU utilization")
            );
            assert_eq!(
                color(theme, "Memory · shared with GPU"),
                color(Theme::Dark, "Memory used")
            );
            assert_eq!(
                color(theme, "Disk I/O [nvme0n1]"),
                color(Theme::Dark, "Disk · nvme0n1")
            );
        }
        let titles = [
            "CPU",
            "Memory",
            "GPUs",
            "Disk",
            "Network",
            "GPU Processes",
            "History",
            "Settings",
        ];
        let colors: std::collections::BTreeSet<_> = titles
            .iter()
            .map(|title| format!("{:?}", color(Theme::Dark, title)))
            .collect();
        assert_eq!(colors.len(), titles.len(), "every resource its own color");
        assert_ne!(
            color(Theme::Dark, "GPU Processes"),
            color(Theme::Dark, "GPUs")
        );
        // A word that only begins like a resource is no resource.
        let palette = ThemePalette::for_theme(Theme::Dark);
        assert_eq!(
            palette.section_title_style("CPUs-ish"),
            palette.title_style()
        );
    }

    #[test]
    fn process_colors_are_fixed_and_never_a_level() {
        for theme in [Theme::Dark, Theme::Cyan, Theme::Green, Theme::Amber] {
            let palette = ThemePalette::for_theme(theme);
            let slots = u8::try_from(PROCESS_COLOR_SLOTS).unwrap();
            let colors: Vec<Color> = (0..slots).map(|slot| palette.process_color(slot)).collect();
            assert_eq!(colors, PROCESS_COLORS.to_vec());
            assert_eq!(
                palette.process_color(slots),
                palette.other_processes_color()
            );
            for color in colors {
                assert!(
                    ![palette.gauge_low, palette.gauge_mid, palette.gauge_high].contains(&color)
                );
            }
        }
    }

    #[test]
    fn conflict_tone_is_emphasized() {
        let palette = ThemePalette::default();
        let style = palette.tone_style(CellTone::Conflict);
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(style.fg, Some(palette.danger));
    }
}
