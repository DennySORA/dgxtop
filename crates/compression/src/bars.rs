//! A series' buckets as bars of one size (D-77, D-79): each bar a bucket
//! of that size, the merge of every sample in its time, so its mean is the
//! mean over that time. The bars end with the one holding the series'
//! latest sample and reach back as many bars as a plot has columns; the
//! latest takes the finer buckets being filled that it has not merged yet.

use dgxtop_core::tiers::{BarSize, Tier};

use crate::bucket::{Bucket, regroup_into};
use crate::series::TieredSeries;
use crate::whole::chart_tier;

/// The bars of a series at one size.
#[derive(Debug, Clone, PartialEq)]
pub struct Bars<B> {
    pub bar: BarSize,
    pub step_ns: i64,
    /// `[start, end)` of the window, UTC.
    pub window: (i64, i64),
    /// The bars holding a sample, oldest first.
    pub bars: Vec<B>,
}

/// The bars of `series` at `bar` (`Auto`: the finest that shows the whole
/// series) over `columns` bars; `None` without a bucket.
pub fn bars_of<B: Bucket>(
    series: &TieredSeries<B>,
    bar: BarSize,
    columns: usize,
) -> Option<Bars<B>> {
    let span = Span::of(series)?;
    let columns = i64::try_from(columns.max(1)).unwrap_or(i64::MAX);
    let bar = match bar {
        BarSize::Auto => BarSize::fixed()
            .find(|candidate| shows_whole_series(series, *candidate, columns, span))
            .unwrap_or(BarSize::OneDay),
        fixed => fixed,
    };
    let tier = bar.tier()?;
    let window = window_of(series, tier, columns, span);
    Some(Bars {
        bar,
        step_ns: tier.step_i64(),
        window,
        bars: bars_in(series, tier, window, std::iter::empty()),
    })
}

/// Whether `bar`'s window starts by the series' first sample and its size
/// still holds that sample's bucket.
fn shows_whole_series<B: Bucket>(
    series: &TieredSeries<B>,
    bar: BarSize,
    columns: i64,
    span: Span,
) -> bool {
    let Some(tier) = bar.tier() else {
        return false;
    };
    let first = tier.align(span.first, series.shift_ns());
    let held = series
        .buckets(tier)
        .next()
        .is_some_and(|bucket| bucket.start_utc_ns() <= first);
    held && first >= window_of(series, tier, columns, span).0
}

/// `columns` bars of `tier` ending with the one holding the latest sample.
fn window_of<B: Bucket>(
    series: &TieredSeries<B>,
    tier: Tier,
    columns: i64,
    span: Span,
) -> (i64, i64) {
    tier.window_ending_at(span.last, series.shift_ns(), columns)
}

/// The buckets of `tier` that start in the window `[start, end)`, each with
/// what the finer sizes still fill of its time and the `extra` pieces
/// (finer buckets the series was not given yet, in any order) it holds;
/// the ones holding nothing are left out (a gap). The Overview's charts
/// share one window, the bars of the local clock ending with the one being
/// filled now (D-82).
pub fn bars_in<'a, B: Bucket + 'a>(
    series: &'a TieredSeries<B>,
    tier: Tier,
    (start, end): (i64, i64),
    extra: impl IntoIterator<Item = &'a B>,
) -> Vec<B> {
    let shift = series.shift_ns();
    let in_window = |at: i64| at >= start && at < end;
    let mut bars: Vec<B> = series
        .buckets(tier)
        .filter(|bucket| in_window(bucket.start_utc_ns()))
        .copied()
        .collect();
    let pieces = series
        .unmerged(tier)
        .chain(extra.into_iter().filter(|piece| !piece.is_empty()))
        .filter(|piece| in_window(tier.align(piece.start_utc_ns(), shift)));
    regroup_into(&mut bars, tier, shift, pieces);
    bars.retain(|bar| !bar.is_empty());
    bars
}

/// Where the series' samples begin and end.
#[derive(Debug, Clone, Copy)]
struct Span {
    /// The first bucket start of the size that holds the whole series
    /// ([`chart_tier`]; the finer ones may have let it go).
    first: i64,
    /// The latest bucket start of any size.
    last: i64,
}

impl Span {
    fn of<B: Bucket>(series: &TieredSeries<B>) -> Option<Self> {
        // A coarse bucket starts on its boundary, before its samples may:
        // the series begins where the size that holds all of it begins.
        let first = series.buckets(chart_tier(series)?).next()?.start_utc_ns();
        let last = Tier::ALL
            .into_iter()
            .filter_map(|tier| series.buckets(tier).last())
            .map(Bucket::start_utc_ns)
            .max()?;
        Some(Self { first, last })
    }
}

#[cfg(test)]
mod tests {
    use dgxtop_core::process_history::{RUN_METRICS, RunBucket, RunMetric};
    use dgxtop_core::tiers::TIER_BUCKETS;

    use super::*;
    use crate::bucket::Tally;
    use crate::run::RunSeries;

    const SECOND: i64 = 1_000_000_000;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;
    /// Midnight UTC.
    const MIDNIGHT: i64 = 1_790_208_000 * SECOND;

    fn means(bars: &Bars<RunBucket>) -> Vec<(i64, f64)> {
        bars.bars
            .iter()
            .map(|bar| {
                (
                    (bar.start_utc_ns - bars.window.0) / bars.step_ns,
                    bar.value(RunMetric::Cpu).mean().unwrap(),
                )
            })
            .collect()
    }

    /// A run sampled every `every` seconds for `minutes` from `start`.
    fn sampled(
        series: &mut RunSeries,
        (start, minutes, every): (i64, i64, i64),
        cpu: impl Fn(i64) -> f64,
    ) {
        for second in (0..minutes * 60).step_by(usize::try_from(every).unwrap()) {
            let mut sample = [None; RUN_METRICS];
            sample[RunMetric::Cpu.index()] = Some(cpu(second / 60));
            series.add(start + second * SECOND, &sample);
        }
    }

    /// A bar is the mean of every sample in its time, the latest bar at
    /// the right edge of the window.
    #[test]
    fn a_bar_is_the_mean_of_its_time() {
        // 90 minutes: the first hour at 10 %, the next half hour at 40 %.
        let mut series = RunSeries::default();
        sampled(&mut series, (MIDNIGHT, 90, 1), |minute| {
            if minute < 60 { 10.0 } else { 40.0 }
        });
        let bars = bars_of(&series, BarSize::ThirtyMinutes, 10).unwrap();
        assert_eq!(bars.step_ns, 30 * MINUTE);
        assert_eq!(
            bars.window.1,
            MIDNIGHT + 90 * MINUTE,
            "ends with the latest bar"
        );
        assert_eq!(bars.window.0, bars.window.1 - 10 * 30 * MINUTE);
        assert_eq!(means(&bars), vec![(7, 10.0), (8, 10.0), (9, 40.0)]);
        let hours = bars_of(&series, BarSize::OneHour, 10).unwrap();
        assert_eq!(means(&hours), vec![(8, 10.0), (9, 40.0)]);
        let total: u32 = hours
            .bars
            .iter()
            .map(|bar| bar.value(RunMetric::Cpu).count)
            .sum();
        assert_eq!(total, 90 * 60, "every sample once, the filling ones too");
    }

    /// `Auto` picks the finest size that shows the whole run.
    #[test]
    fn auto_takes_the_finest_bar_holding_the_whole_run() {
        let mut series = RunSeries::default();
        sampled(&mut series, (MIDNIGHT, 90, 1), |_| 5.0);
        let fit = |columns| bars_of(&series, BarSize::Auto, columns).unwrap().bar;
        assert_eq!(fit(100), BarSize::OneMinute);
        assert_eq!(fit(60), BarSize::FiveMinutes);
        assert_eq!(fit(3), BarSize::ThirtyMinutes);
        assert_eq!(fit(1), BarSize::TwoHours);
        assert!(bars_of(&RunSeries::default(), BarSize::Auto, 10).is_none());
    }

    /// Each size keeps its latest buckets only: a run of 20 days shows 20
    /// day bars, and its minutes only the latest hours.
    #[test]
    fn a_long_run_keeps_its_whole_life_at_the_coarse_sizes() {
        let mut series = RunSeries::new(8 * HOUR);
        sampled(&mut series, (MIDNIGHT - 8 * HOUR, 20 * 24 * 60, 60), |_| {
            2.0
        });
        let days = bars_of(&series, BarSize::OneDay, 30).unwrap();
        assert_eq!(days.bars.len(), 20, "20 bars, 20 local days");
        assert!(
            days.bars
                .iter()
                .all(|bar| bar.value(RunMetric::Cpu).count == 24 * 60)
        );
        assert_eq!(days.bars[0].start_utc_ns, MIDNIGHT - 8 * HOUR);
        let minutes = bars_of(&series, BarSize::OneMinute, 1_000).unwrap();
        assert_eq!(
            minutes.bars.len(),
            TIER_BUCKETS + 1,
            "the latest minutes kept and the one being filled"
        );
        assert_eq!(
            bars_of(&series, BarSize::Auto, 30).unwrap().bar,
            BarSize::EighteenHours,
            "the finest whose 30 bars span the 20 days"
        );
        let eighteen = bars_of(&series, BarSize::EighteenHours, 30).unwrap();
        let counted: u32 = eighteen
            .bars
            .iter()
            .map(|bar| bar.value(RunMetric::Cpu).count)
            .sum();
        assert_eq!(counted, 20 * 24 * 60);
        assert_eq!(eighteen.window.1 - eighteen.window.0, 30 * 18 * HOUR);
    }

    /// The bars work for any bucket: counted samples land in their bar,
    /// the latest with what is still filling, none outside the window.
    #[test]
    fn any_bucket_type_lays_out_as_bars() {
        let mut series = TieredSeries::<Tally>::default();
        for second in 0..(3 * 3_600) {
            series.record(second * SECOND, |tally| tally.samples += 1);
        }
        let hours = bars_of(&series, BarSize::OneHour, 2).unwrap();
        assert_eq!(hours.window, (HOUR, 3 * HOUR), "the last two hours");
        let counts: Vec<(i64, u32)> = hours
            .bars
            .iter()
            .map(|bar| (bar.start_utc_ns / HOUR, bar.samples))
            .collect();
        assert_eq!(counts, vec![(1, 3_600), (2, 3_600)]);
        let auto = bars_of(&series, BarSize::Auto, 4).unwrap();
        assert_eq!(auto.bar, BarSize::OneHour, "4 hour bars hold 3 hours");
        assert!(bars_of(&TieredSeries::<Tally>::default(), BarSize::OneHour, 4).is_none());
    }

    /// The bars in a window: only buckets starting inside it, the one being
    /// filled with its unmerged sources, extra pieces (in any order) merged
    /// into the bar holding them or starting one, empty pieces no bar.
    #[test]
    fn bars_in_a_window_take_the_unmerged_and_extra_pieces() {
        let mut series = TieredSeries::<Tally>::default();
        for second in 0..(3 * 3_600) {
            series.record(second * SECOND, |tally| tally.samples += 1);
        }
        let hour_samples = |bars: &[Tally]| -> Vec<(i64, u32)> {
            bars.iter()
                .map(|bar| (bar.start_utc_ns / HOUR, bar.samples))
                .collect()
        };
        let plain = bars_in(&series, Tier::OneHour, (HOUR, 3 * HOUR), std::iter::empty());
        assert_eq!(hour_samples(&plain), vec![(1, 3_600), (2, 3_600)]);
        let late = Tally {
            start_utc_ns: 4 * HOUR + MINUTE,
            samples: 5,
        };
        let early = Tally {
            start_utc_ns: 2 * HOUR + MINUTE,
            samples: 7,
        };
        let outside = Tally {
            start_utc_ns: 9 * HOUR,
            samples: 1,
        };
        let empty = Tally::new(3 * HOUR);
        let extra = [late, early, outside, empty];
        let bars = bars_in(&series, Tier::OneHour, (HOUR, 5 * HOUR), extra.iter());
        assert_eq!(hour_samples(&bars), vec![(1, 3_600), (2, 3_607), (4, 5)]);
        let none = bars_in(
            &TieredSeries::<Tally>::new(0),
            Tier::OneHour,
            (0, HOUR),
            std::iter::empty(),
        );
        assert!(none.is_empty());
    }

    /// An empty bucket is no bar: a size can hold a bucket with no sample.
    #[test]
    fn empty_buckets_are_left_out() {
        let mut series = TieredSeries::<Tally>::default();
        series.restore_closed(Tier::OneMinute, Tally::new(0));
        let counted = Tally {
            start_utc_ns: MINUTE,
            samples: 2,
        };
        series.restore_closed(Tier::OneMinute, counted);
        let bars = bars_of(&series, BarSize::OneMinute, 10).unwrap();
        assert_eq!(bars.bars, vec![counted]);
    }
}
