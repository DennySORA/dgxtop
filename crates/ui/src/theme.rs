//! Color palettes for the four configured themes (`dark`, `cyan`, `green`,
//! `amber`). Color is only an extra cue: every state is also spelled out in
//! text, so a monochrome terminal loses nothing semantically.

use dgxtop_core::config::Theme;
use dgxtop_core::view::{
    BannerLevel, MetricCell, PROCESS_COLOR_SLOTS, TEMPERATURE_SCALE_C, ViewModel,
};
use ratatui::style::{Color, Modifier, Style};

use crate::format::{CellTone, cell_tone, gpu_number};

/// Steps of the level colors from an empty to a full gauge (D-104): each
/// whole percent of a fill has a color of its own.
const LEVEL_STEPS: u8 = 100;
/// The step of a half fill, where the level color is the theme's amber,
/// halfway from its green to its red.
const MIDDLE_STEP: u8 = LEVEL_STEPS / 2;
/// A percentage's full scale.
const PERCENT_SCALE: f64 = 100.0;

/// How a reading takes the level colors (D-104, D-126): green up to
/// `green_until`, then a step of the gradient for each whole percent of the
/// way to `top` (amber halfway), red from `top` on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelScale {
    green_until: f64,
    top: f64,
}

impl LevelScale {
    /// Temperatures (D-126): green up to 50 °C, where idle and lightly
    /// loaded parts sit, amber at 75 °C, red at 100 °C.
    pub const TEMPERATURE: Self = Self {
        green_until: 50.0,
        top: TEMPERATURE_SCALE_C,
    };
    /// Fan speeds (D-126): green up to 50 %, amber at 75 %, red at 100 %.
    pub const FAN: Self = Self {
        green_until: 50.0,
        top: PERCENT_SCALE,
    };

    /// The share of `top`, like a gauge that full: green only when empty.
    pub const fn share_of(top: f64) -> Self {
        Self {
            green_until: 0.0,
            top,
        }
    }

    /// Where `value` sits on the gradient: 0 up to `green_until`, 1 at
    /// `top`; [`ThemePalette::level_color`] takes it from there.
    pub fn level(self, value: f64) -> f64 {
        (value - self.green_until) / (self.top - self.green_until)
    }
}

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

/// Colors of the GPUs by their number (D-123), the same in every theme and
/// wherever a GPU is named: sky blue, pink, yellow, aqua, violet, green,
/// salmon, teal; a ninth GPU takes the first again. Validated as text on
/// each theme's backgrounds: the first two, the most common pair, stay
/// apart by OKLab ΔE ≥ 27.5 (≥ 10.9 under protanopia and deuteranopia),
/// the first four by ≥ 19.0 (10.9), all eight by ≥ 12.9 (3.1, so the GPU's
/// number always goes beside its color); each is at least ΔE 10.7 from the
/// level colors (the first four 16.1), so a label never reads as how full
/// a bar is, 12.1 from the grays of text and 8.8 from the process colors
/// (the first four 10.5), and holds 4.5:1 contrast.
const GPU_COLORS: [Color; 8] = [
    Color::Rgb(0x00, 0xb8, 0xf8),
    Color::Rgb(0xf8, 0x78, 0xc0),
    Color::Rgb(0xf8, 0xf8, 0x28),
    Color::Rgb(0x68, 0xf8, 0xf8),
    Color::Rgb(0xb8, 0x80, 0xf8),
    Color::Rgb(0x8c, 0xfc, 0x98),
    Color::Rgb(0xf8, 0xa8, 0xa0),
    Color::Rgb(0x00, 0xb0, 0xa0),
];

/// Title colors of the resources (D-74), bright and the same in every
/// theme, so a resource's panels look alike on every page; the host's
/// power (D-106) takes the teal the removed Sources view had.
const SECTION_COLORS: [(&str, Color); 10] = [
    ("GPU Processes", Color::Rgb(0xff, 0x9e, 0x64)),
    ("CPU", Color::Rgb(0x7a, 0xa2, 0xf7)),
    ("Memory", Color::Rgb(0x9e, 0xce, 0x6a)),
    ("GPUs", Color::Rgb(0xe0, 0xaf, 0x68)),
    ("GPU", Color::Rgb(0xe0, 0xaf, 0x68)),
    ("Disk", Color::Rgb(0xbb, 0x9a, 0xf7)),
    ("Network", Color::Rgb(0x7d, 0xcf, 0xff)),
    ("History", Color::Rgb(0xf7, 0x76, 0x8e)),
    ("Settings", Color::Rgb(0xc0, 0xca, 0xf5)),
    ("Power", Color::Rgb(0x73, 0xda, 0xca)),
];

/// The color `position` (0..=1) of the way from `from` to `to`, mixed in
/// OKLab so the steps look even; the ends are `from` and `to` exactly, and
/// a color that is not RGB stays `from`.
fn mix_oklab(from: Color, to: Color, position: f64) -> Color {
    let (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) = (from, to) else {
        return from;
    };
    if position.is_nan() || position <= 0.0 {
        return from;
    }
    if position >= 1.0 {
        return to;
    }
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
    /// The brand's gradient, left to right across `DGXTOP` (D-129).
    pub brand: [Color; 3],
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

    /// The palette of the theme a projection asks for.
    pub fn for_model(model: &ViewModel) -> Self {
        Self::for_theme(model.header.theme)
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
            brand: [
                Color::Rgb(118, 210, 90),
                Color::Rgb(0, 200, 230),
                Color::Rgb(170, 120, 255),
            ],
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
            brand: [
                Color::Rgb(0, 220, 200),
                Color::Rgb(90, 150, 255),
                Color::Rgb(200, 110, 240),
            ],
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
            brand: [
                Color::Rgb(170, 230, 60),
                Color::Rgb(60, 220, 130),
                Color::Rgb(0, 200, 200),
            ],
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
            brand: [
                Color::Rgb(255, 210, 70),
                Color::Rgb(250, 150, 40),
                Color::Rgb(235, 90, 70),
            ],
        }
    }

    /// Gauge color for a 0..=1 fill ratio (D-104): one of a hundred steps
    /// of an OKLab gradient, the theme's green at 0 %, its amber at 50 %
    /// and its red at 100 %. A fill takes the step of its whole percent;
    /// below 0 % (or not a number) it is green, above 100 % red.
    pub fn level_color(&self, ratio: f64) -> Color {
        let step = level_step(ratio);
        if step <= MIDDLE_STEP {
            mix_oklab(
                self.gauge_low,
                self.gauge_mid,
                f64::from(step) / f64::from(MIDDLE_STEP),
            )
        } else {
            mix_oklab(
                self.gauge_mid,
                self.gauge_high,
                f64::from(step - MIDDLE_STEP) / f64::from(LEVEL_STEPS - MIDDLE_STEP),
            )
        }
    }

    /// The color at `position` (0 the first letter, 1 the last) of the
    /// brand's gradient (D-129), mixed in OKLab like the level colors.
    pub fn brand_color(&self, position: f64) -> Color {
        let [first, middle, last] = self.brand;
        let position = if position.is_finite() {
            position.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if position <= 0.5 {
            mix_oklab(first, middle, position * 2.0)
        } else {
            mix_oklab(middle, last, (position - 0.5) * 2.0)
        }
    }

    /// The level color of a temperature (D-104, D-126): green up to 50 °C,
    /// then the gradient to red at 100 °C ([`LevelScale::TEMPERATURE`]), as
    /// its text, the GPU's temperature bar and History chart draw it.
    pub fn temperature_color(&self, celsius: f64) -> Color {
        self.level_color(LevelScale::TEMPERATURE.level(celsius))
    }

    /// The style a temperature's value is written in: its level color
    /// ([`Self::temperature_color`]) when it is read as it should be, else
    /// the tone that says it is a fallback, stale, conflicting or missing.
    pub fn temperature_style(&self, cell: &MetricCell) -> Style {
        self.level_style(cell, LevelScale::TEMPERATURE)
    }

    /// The style a percentage's value is written in (D-125): the level
    /// color of its share of 100 % when it is read as it should be, else
    /// the tone that says otherwise, like [`Self::temperature_style`].
    pub fn percent_style(&self, cell: &MetricCell) -> Style {
        self.level_style(cell, LevelScale::share_of(PERCENT_SCALE))
    }

    /// The style a fan speed is written in (D-126): green up to 50 %, then
    /// the gradient to red at 100 % ([`LevelScale::FAN`]), else its tone.
    pub fn fan_style(&self, cell: &MetricCell) -> Style {
        self.level_style(cell, LevelScale::FAN)
    }

    fn level_style(&self, cell: &MetricCell, scale: LevelScale) -> Style {
        let tone = cell_tone(cell);
        let style = self.tone_style(tone);
        match (tone, cell.value_f64()) {
            (CellTone::Normal, Some(value)) => style.fg(self.level_color(scale.level(value))),
            _ => style,
        }
    }

    /// Color of tracked GPU process `slot` (D-64).
    pub fn process_color(&self, slot: u8) -> Color {
        PROCESS_COLORS
            .get(usize::from(slot))
            .copied()
            .unwrap_or_else(|| self.other_processes_color())
    }

    /// Color of GPU `number`, its NVML index (D-123).
    pub fn gpu_color(&self, number: u32) -> Color {
        let slot = usize::try_from(number).unwrap_or(usize::MAX) % GPU_COLORS.len();
        GPU_COLORS[slot]
    }

    /// Color of the GPU labelled `label` (`GPU 0`); none for a GPU labelled
    /// by its UUID, which has no number.
    pub fn gpu_label_color(&self, label: &str) -> Option<Color> {
        gpu_number(label).map(|number| self.gpu_color(number))
    }

    /// `style` in the color of the GPU labelled `label`, when it has one.
    pub fn gpu_style(&self, label: &str, style: Style) -> Style {
        self.gpu_label_color(label)
            .map_or(style, |color| style.fg(color))
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

/// The gradient step (0..=[`LEVEL_STEPS`]) of a 0..=1 fill ratio: its
/// whole percent, clamped; not a number is step 0.
fn level_step(ratio: f64) -> u8 {
    if ratio.is_nan() {
        return 0;
    }
    let steps = f64::from(LEVEL_STEPS);
    let percent = (ratio * steps).round().clamp(0.0, steps);
    // Clamped to 0..=100 above, so the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let step = percent as u8;
    step
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

    const THEMES: [Theme; 4] = [Theme::Dark, Theme::Cyan, Theme::Green, Theme::Amber];

    /// D-104: green at 0 %, amber at 50 %, red only at 100 %, in every
    /// theme; nothing turns fully red earlier.
    #[test]
    fn level_colors_run_from_green_through_amber_to_red_at_full() {
        for theme in THEMES {
            let palette = ThemePalette::for_theme(theme);
            assert_eq!(palette.level_color(0.0), palette.gauge_low);
            assert_eq!(palette.level_color(0.5), palette.gauge_mid);
            assert_eq!(palette.level_color(1.0), palette.gauge_high);
            for ratio in [0.1, 0.25, 0.49, 0.51, 0.8, 0.99] {
                let color = palette.level_color(ratio);
                assert!(
                    ![palette.gauge_low, palette.gauge_mid, palette.gauge_high].contains(&color),
                    "{theme:?} at {ratio} is a blend, not a band"
                );
            }
            assert_eq!(palette.level_color(-0.3), palette.gauge_low);
            assert_eq!(palette.level_color(f64::NAN), palette.gauge_low);
            assert_eq!(palette.level_color(1.5), palette.gauge_high);
            assert_eq!(palette.level_color(f64::INFINITY), palette.gauge_high);
        }
        let dark = ThemePalette::for_theme(Theme::Dark);
        assert_eq!(rgb(dark.gauge_low), [90, 200, 120]);
        assert_eq!(rgb(dark.gauge_mid), [230, 180, 60]);
        assert_eq!(rgb(dark.gauge_high), [225, 70, 70]);
        // A quarter is a yellow green, 80 % an orange red.
        assert_eq!(rgb(dark.level_color(0.25)), [172, 192, 95]);
        assert_eq!(rgb(dark.level_color(0.8)), [230, 119, 69]);
    }

    /// One hundred steps: a fill takes its whole percent's color, and
    /// every step from 0 % to 100 % has a color of its own.
    #[test]
    fn the_gradient_has_a_color_for_every_whole_percent() {
        for theme in THEMES {
            let palette = ThemePalette::for_theme(theme);
            assert_eq!(palette.level_color(0.504), palette.level_color(0.50));
            assert_eq!(palette.level_color(0.006), palette.level_color(0.01));
            assert_ne!(palette.level_color(0.506), palette.level_color(0.50));
            let steps: std::collections::BTreeSet<[u8; 3]> = (0..=10_000)
                .map(|tenth_of_a_permille| {
                    rgb(palette.level_color(f64::from(tenth_of_a_permille) / 10_000.0))
                })
                .collect();
            assert_eq!(steps.len(), 101, "{theme:?}");
        }
    }

    /// From green to amber every step moves away from the green and toward
    /// the amber; from amber to red away from the amber and toward the red.
    #[test]
    fn the_gradient_is_monotonic() {
        let distance = |a: [f64; 3], b: [f64; 3]| {
            a.iter()
                .zip(b)
                .map(|(x, y)| (x - y).powi(2))
                .sum::<f64>()
                .sqrt()
        };
        for theme in THEMES {
            let palette = ThemePalette::for_theme(theme);
            for (from, to, start) in [
                (palette.gauge_low, palette.gauge_mid, 0.0),
                (palette.gauge_mid, palette.gauge_high, 0.5),
            ] {
                let (from, to) = (oklab(rgb(from)), oklab(rgb(to)));
                let mut previous = (0.0, distance(from, to));
                for step in 0..=50 {
                    let ratio = start + f64::from(step) / 100.0;
                    let color = oklab(rgb(palette.level_color(ratio)));
                    let now = (distance(from, color), distance(color, to));
                    // One 8-bit step of rounding is allowed.
                    assert!(now.0 + 0.005 >= previous.0, "{theme:?} at {ratio}");
                    assert!(now.1 <= previous.1 + 0.005, "{theme:?} at {ratio}");
                    previous = now;
                }
            }
        }
    }

    /// Temperatures and fan speeds stay green up to 50 and turn from there
    /// to red at 100 (D-126); a percentage turns from 0 (D-125).
    #[test]
    fn temperatures_and_fans_turn_only_past_fifty() {
        let palette = ThemePalette::default();
        for celsius in [-5.0, 0.0, 32.0, 50.0] {
            assert_eq!(palette.temperature_color(celsius), palette.gauge_low);
        }
        assert_eq!(palette.temperature_color(75.0), palette.gauge_mid);
        assert_eq!(palette.temperature_color(100.0), palette.gauge_high);
        assert_eq!(palette.temperature_color(71.0), palette.level_color(0.42));
        assert_ne!(palette.temperature_color(95.0), palette.gauge_high);
        assert_eq!(palette.temperature_color(120.0), palette.gauge_high);
        let fan = |percent: f64| {
            palette
                .fan_style(&crate::test_fixtures::ok(
                    percent,
                    dgxtop_core::metric::CanonicalUnit::Percent,
                ))
                .fg
        };
        assert_eq!(fan(45.0), Some(palette.gauge_low));
        assert_eq!(fan(75.0), Some(palette.gauge_mid));
        assert_eq!(fan(100.0), Some(palette.gauge_high));
        assert_eq!(LevelScale::share_of(100.0).level(50.0), 0.5);
    }

    /// The viewer's palette takes the projection's theme, so an applied
    /// configuration recolors the next frame.
    #[test]
    fn the_palette_follows_the_projections_theme() {
        let mut model =
            crate::test_fixtures::view_model(dgxtop_core::commands::ViewKind::Overview, 1);
        assert_eq!(ThemePalette::for_model(&model), ThemePalette::default());
        model.header.theme = Theme::Green;
        let palette = ThemePalette::for_model(&model);
        assert_eq!(palette, ThemePalette::for_theme(Theme::Green));
        assert_eq!(palette.level_color(1.0), palette.gauge_high);
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
            "Power",
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

    /// WCAG contrast of two colors.
    fn contrast(first: Color, second: Color) -> f64 {
        let luminance = |color: Color| {
            let [r, g, b] = rgb(color).map(|channel| {
                let value = f64::from(channel) / 255.0;
                if value <= 0.040_45 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            });
            0.2126 * r + 0.7152 * g + 0.0722 * b
        };
        let (first, second) = (luminance(first), luminance(second));
        (first.max(second) + 0.05) / (first.min(second) + 0.05)
    }

    /// OKLab ΔE of two colors, in hundredths.
    fn delta_e(first: Color, second: Color) -> f64 {
        let (first, second) = (oklab(rgb(first)), oklab(rgb(second)));
        100.0
            * first
                .iter()
                .zip(second)
                .map(|(x, y)| (x - y).powi(2))
                .sum::<f64>()
                .sqrt()
    }

    /// D-123: a GPU keeps one color in every theme, picked by its number
    /// and taken again by a ninth GPU; the colors stay apart, the first
    /// ones the most, and never read as a level, a gray or a process; a
    /// GPU labelled by its UUID keeps the style it is given.
    #[test]
    fn gpu_colors_are_fixed_apart_and_never_a_level() {
        let pairs = |count: usize| {
            (0..count).flat_map(move |first| (first + 1..count).map(move |second| (first, second)))
        };
        let nearest = |count: usize| {
            pairs(count)
                .map(|(first, second)| delta_e(GPU_COLORS[first], GPU_COLORS[second]))
                .fold(f64::INFINITY, f64::min)
        };
        assert!(nearest(2) >= 25.0);
        assert!(nearest(4) >= 18.0);
        assert!(nearest(GPU_COLORS.len()) >= 12.0);
        for theme in THEMES {
            let palette = ThemePalette::for_theme(theme);
            let colors: Vec<Color> = (0..8).map(|number| palette.gpu_color(number)).collect();
            assert_eq!(colors, GPU_COLORS.to_vec());
            assert_eq!(palette.gpu_color(9), palette.gpu_color(1));
            assert_eq!(palette.gpu_color(u32::MAX), GPU_COLORS[7]);
            for (number, color) in colors.into_iter().enumerate() {
                let levels = (0..=100).map(|step| palette.level_color(f64::from(step) / 100.0));
                let level = levels
                    .map(|level| delta_e(color, level))
                    .fold(f64::INFINITY, f64::min);
                assert!(
                    level >= if number < 4 { 16.0 } else { 10.0 },
                    "GPU {number}"
                );
                for gray in [palette.text, palette.text_dim] {
                    assert!(delta_e(color, gray) >= 12.0, "GPU {number}");
                }
                for process in PROCESS_COLORS {
                    assert!(delta_e(color, process) >= 8.5, "GPU {number}");
                }
                for background in [
                    palette.bar_background,
                    palette.highlight_background,
                    Color::Rgb(0, 0, 0),
                ] {
                    assert!(contrast(color, background) >= 4.5, "GPU {number}");
                }
            }
            let style = palette.label_style();
            assert_eq!(palette.gpu_style("GPU 1", style).fg, Some(GPU_COLORS[1]));
            assert_eq!(palette.gpu_style("GPU 1a2b3c4d", style), style);
            assert_eq!(palette.gpu_label_color("GPU 10"), Some(GPU_COLORS[2]));
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
