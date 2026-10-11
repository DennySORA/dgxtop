//! The bar sizes every history shares (D-77, D-79): the Process and the
//! Overview History keep each series in buckets of these sizes, 1, 5, 10
//! and 30 minutes, 1, 2, 4, 6, 8, 12, 18 and 24 hours (and 5 s buckets for
//! the last 10 minutes), each bucket the merge of the finer ones it covers,
//! aligned to the local clock. Each size keeps its latest
//! [`TIER_BUCKETS`], as many as the widest chart draws: older time is in
//! the coarser sizes already.

use serde::{Deserialize, Serialize};

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Buckets each size but the 5 s one keeps: bars of the widest chart.
pub const TIER_BUCKETS: usize = 300;

/// The size of a bucket: the 5 s buckets of the last minutes, and one size
/// for each bar size of the charts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Tier {
    Recent,
    OneMinute,
    FiveMinutes,
    TenMinutes,
    ThirtyMinutes,
    OneHour,
    TwoHours,
    FourHours,
    SixHours,
    EightHours,
    TwelveHours,
    EighteenHours,
    OneDay,
}

impl Tier {
    /// The sizes, finest first: a size's source comes before it.
    pub const ALL: [Self; 13] = [
        Self::Recent,
        Self::OneMinute,
        Self::FiveMinutes,
        Self::TenMinutes,
        Self::ThirtyMinutes,
        Self::OneHour,
        Self::TwoHours,
        Self::FourHours,
        Self::SixHours,
        Self::EightHours,
        Self::TwelveHours,
        Self::EighteenHours,
        Self::OneDay,
    ];

    pub const fn step_ns(self) -> u64 {
        let minutes = match self {
            Self::Recent => return 5 * NANOS_PER_SECOND,
            Self::OneMinute => 1,
            Self::FiveMinutes => 5,
            Self::TenMinutes => 10,
            Self::ThirtyMinutes => 30,
            Self::OneHour => 60,
            Self::TwoHours => 120,
            Self::FourHours => 240,
            Self::SixHours => 360,
            Self::EightHours => 480,
            Self::TwelveHours => 720,
            Self::EighteenHours => 1_080,
            Self::OneDay => 1_440,
        };
        minutes * 60 * NANOS_PER_SECOND
    }

    /// The size whose closed buckets this one merges: the coarsest finer
    /// size its step is a multiple of (18 hours take 6, 24 take 12).
    pub const fn source(self) -> Option<Self> {
        Some(match self {
            Self::Recent => return None,
            Self::OneMinute => Self::Recent,
            Self::FiveMinutes => Self::OneMinute,
            Self::TenMinutes => Self::FiveMinutes,
            Self::ThirtyMinutes => Self::TenMinutes,
            Self::OneHour => Self::ThirtyMinutes,
            Self::TwoHours => Self::OneHour,
            Self::FourHours | Self::SixHours => Self::TwoHours,
            Self::EightHours => Self::FourHours,
            Self::TwelveHours | Self::EighteenHours => Self::SixHours,
            Self::OneDay => Self::TwelveHours,
        })
    }

    /// Its position in [`Tier::ALL`].
    pub const fn level(self) -> usize {
        self as usize
    }

    /// Buckets of this size a series keeps at most: 10 minutes of 5 s
    /// buckets, else [`TIER_BUCKETS`].
    pub const fn capacity(self) -> usize {
        match self {
            Self::Recent => 120,
            _ => TIER_BUCKETS,
        }
    }

    /// The code stored in the databases. A stored code never changes
    /// meaning: renumbering needs a new format version (D-85).
    pub const fn code(self) -> i64 {
        match self {
            Self::Recent => 0,
            Self::OneMinute => 1,
            Self::TenMinutes => 2,
            Self::FiveMinutes => 3,
            Self::ThirtyMinutes => 4,
            Self::OneHour => 5,
            Self::TwoHours => 6,
            Self::FourHours => 7,
            Self::SixHours => 8,
            Self::EightHours => 9,
            Self::TwelveHours => 10,
            Self::EighteenHours => 11,
            Self::OneDay => 12,
        }
    }

    pub fn from_code(code: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|tier| tier.code() == code)
    }

    /// [`Self::step_ns`] as a span of signed UTC time.
    pub fn step_i64(self) -> i64 {
        i64::try_from(self.step_ns()).unwrap_or(i64::MAX)
    }

    /// The start of the bucket holding `utc_ns`, its boundaries moved by
    /// `shift_ns` (the local clock's offset, so a day is a local day).
    pub fn align(self, utc_ns: i64, shift_ns: i64) -> i64 {
        let step = self.step_i64();
        utc_ns
            .saturating_add(shift_ns)
            .div_euclid(step)
            .saturating_mul(step)
            .saturating_sub(shift_ns)
    }

    /// `[start, end)` of `columns` bars of this size ending with the one
    /// holding `utc_ns`: every chart's window, so the Overview's and the
    /// Process History's bars share their boundaries (D-79).
    pub fn window_ending_at(self, utc_ns: i64, shift_ns: i64, columns: i64) -> (i64, i64) {
        let step = self.step_i64();
        let end = self.align(utc_ns, shift_ns).saturating_add(step);
        (end.saturating_sub(columns.saturating_mul(step)), end)
    }
}

/// How much time one bar of a chart covers (D-77, D-78): its mean of every
/// sample in that time. The Process and the Overview History share these
/// sizes; `Auto` (Process History only) takes the finest that shows the
/// whole run. Serialized as its token (`1m`, `24h`, `auto`), as the
/// session's bar sizes travel (D-80).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BarSize {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "1m")]
    OneMinute,
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "10m")]
    TenMinutes,
    #[serde(rename = "30m")]
    ThirtyMinutes,
    #[serde(rename = "1h")]
    OneHour,
    #[serde(rename = "2h")]
    TwoHours,
    #[serde(rename = "4h")]
    FourHours,
    #[serde(rename = "6h")]
    SixHours,
    #[serde(rename = "8h")]
    EightHours,
    #[serde(rename = "12h")]
    TwelveHours,
    #[serde(rename = "18h")]
    EighteenHours,
    #[serde(rename = "24h")]
    OneDay,
}

impl Default for BarSize {
    /// The Process History's default.
    fn default() -> Self {
        Self::Auto
    }
}

impl BarSize {
    /// The Overview History's default (it has no `Auto`).
    pub const fn overview_default() -> Self {
        Self::OneMinute
    }

    /// As the Overview History draws it: `Auto` is its default.
    pub const fn for_overview(self) -> Self {
        match self {
            Self::Auto => Self::overview_default(),
            fixed => fixed,
        }
    }

    /// The next (or previous) size the Overview History steps to, wrapping
    /// past `Auto`.
    pub fn step_overview(self, forward: bool) -> Self {
        match self.for_overview().step(forward) {
            Self::Auto => self.for_overview().step(forward).step(forward),
            next => next,
        }
    }

    /// In the order `w` steps through.
    pub const ALL: [Self; 13] = [
        Self::Auto,
        Self::OneMinute,
        Self::FiveMinutes,
        Self::TenMinutes,
        Self::ThirtyMinutes,
        Self::OneHour,
        Self::TwoHours,
        Self::FourHours,
        Self::SixHours,
        Self::EightHours,
        Self::TwelveHours,
        Self::EighteenHours,
        Self::OneDay,
    ];

    pub const fn token(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::OneMinute => "1m",
            Self::FiveMinutes => "5m",
            Self::TenMinutes => "10m",
            Self::ThirtyMinutes => "30m",
            Self::OneHour => "1h",
            Self::TwoHours => "2h",
            Self::FourHours => "4h",
            Self::SixHours => "6h",
            Self::EightHours => "8h",
            Self::TwelveHours => "12h",
            Self::EighteenHours => "18h",
            Self::OneDay => "24h",
        }
    }

    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|bar| bar.token() == token)
    }

    /// The bucket size its bars are; `None` for `Auto`.
    pub const fn tier(self) -> Option<Tier> {
        Some(match self {
            Self::Auto => return None,
            Self::OneMinute => Tier::OneMinute,
            Self::FiveMinutes => Tier::FiveMinutes,
            Self::TenMinutes => Tier::TenMinutes,
            Self::ThirtyMinutes => Tier::ThirtyMinutes,
            Self::OneHour => Tier::OneHour,
            Self::TwoHours => Tier::TwoHours,
            Self::FourHours => Tier::FourHours,
            Self::SixHours => Tier::SixHours,
            Self::EightHours => Tier::EightHours,
            Self::TwelveHours => Tier::TwelveHours,
            Self::EighteenHours => Tier::EighteenHours,
            Self::OneDay => Tier::OneDay,
        })
    }

    pub const fn step_ns(self) -> Option<u64> {
        match self.tier() {
            Some(tier) => Some(tier.step_ns()),
            None => None,
        }
    }

    /// The next (or with `forward` false the previous) size, wrapping.
    pub fn step(self, forward: bool) -> Self {
        let count = Self::ALL.len();
        let position = Self::ALL.iter().position(|bar| *bar == self).unwrap_or(0);
        let next = if forward {
            (position + 1) % count
        } else {
            (position + count - 1) % count
        };
        Self::ALL[next]
    }

    /// The sizes `Auto` chooses from, finest first.
    pub fn fixed() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter().skip(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_align_nest_and_keep_their_buckets() {
        const HOUR: i64 = 3_600 * 1_000_000_000;
        assert_eq!(Tier::Recent.capacity(), 120);
        assert_eq!(Tier::OneDay.capacity(), TIER_BUCKETS);
        assert_eq!(Tier::OneMinute.align(119_000_000_000, 0), 60_000_000_000);
        assert_eq!(Tier::OneMinute.align(-1, 0), -60_000_000_000);
        // UTC+8: a day starts at 16:00 UTC.
        assert_eq!(Tier::OneDay.align(HOUR, 8 * HOUR), -8 * HOUR);
        for (level, tier) in Tier::ALL.into_iter().enumerate() {
            assert_eq!(Tier::from_code(tier.code()), Some(tier));
            assert_eq!(tier.level(), level);
            if let Some(source) = tier.source() {
                assert!(source < tier, "{tier:?} after its source");
                assert_eq!(tier.step_ns() % source.step_ns(), 0, "{tier:?} nests");
            }
        }
        assert_eq!(Tier::Recent.source(), None);
        assert_eq!(Tier::from_code(13), None);
        assert_eq!(Tier::from_code(1), Some(Tier::OneMinute));
        assert_eq!(Tier::from_code(2), Some(Tier::TenMinutes));
        for bar in BarSize::fixed() {
            assert_eq!(bar.tier().map(Tier::step_ns), bar.step_ns());
        }
    }

    /// A window ends with the bar holding its time, on the local clock.
    #[test]
    fn windows_end_with_the_bar_holding_their_time() {
        const HOUR: i64 = 3_600 * 1_000_000_000;
        assert_eq!(Tier::OneHour.step_i64(), HOUR);
        assert_eq!(
            Tier::OneHour.window_ending_at(2 * HOUR + 5, 0, 2),
            (HOUR, 3 * HOUR)
        );
        // UTC+8: the day holding 01:00 UTC is 16:00 UTC the day before on.
        assert_eq!(
            Tier::OneDay.window_ending_at(HOUR, 8 * HOUR, 1),
            (-8 * HOUR, 16 * HOUR)
        );
        assert_eq!(
            Tier::OneMinute.window_ending_at(-1, 0, 1),
            (-60_000_000_000, 0),
            "before the epoch"
        );
        let (start, end) = Tier::OneDay.window_ending_at(0, 0, i64::MAX);
        assert!(start < 0 && end > 0, "saturates instead of overflowing");
    }

    #[test]
    fn bars_parse_step_and_wrap() {
        let tokens: Vec<&str> = BarSize::ALL.iter().map(|bar| bar.token()).collect();
        assert_eq!(
            tokens,
            vec![
                "auto", "1m", "5m", "10m", "30m", "1h", "2h", "4h", "6h", "8h", "12h", "18h", "24h"
            ]
        );
        for bar in BarSize::ALL {
            assert_eq!(BarSize::parse(bar.token()), Some(bar));
        }
        assert_eq!(BarSize::parse("3h"), None);
        assert_eq!(BarSize::Auto.step_ns(), None);
        assert_eq!(
            BarSize::EighteenHours.step_ns(),
            Some(18 * 3_600 * 1_000_000_000)
        );
        assert_eq!(BarSize::OneDay.step(true), BarSize::Auto);
        assert_eq!(BarSize::Auto.step(false), BarSize::OneDay);
        assert_eq!(BarSize::OneMinute.step(true), BarSize::FiveMinutes);
        assert_eq!(BarSize::fixed().count(), 12);
        // The Overview History has no `auto`: stepping skips it (D-78).
        assert_eq!(BarSize::OneDay.step_overview(true), BarSize::OneMinute);
        assert_eq!(BarSize::OneMinute.step_overview(false), BarSize::OneDay);
        assert_eq!(BarSize::Auto.for_overview(), BarSize::OneMinute);
        // Sizes travel as their tokens (D-80).
        assert_eq!(serde_json::to_value(BarSize::EighteenHours).unwrap(), "18h");
        assert_eq!(
            serde_json::from_value::<BarSize>(serde_json::json!("auto")).unwrap(),
            BarSize::Auto
        );
    }
}
