//! A series' buckets as bars of one size (D-77, D-79): each bar a bucket
//! of that size, the merge of every sample in its time, so its mean is the
//! mean over that time. The bars end with the one holding the series'
//! latest sample and reach back as many bars as a plot has columns; the
//! latest takes the finer buckets being filled that it has not merged yet.

use dgxtop_core::process_history::{RunBar, Tier};

use crate::bucket::{Bucket, merged_at};
use crate::series::TieredSeries;

/// The bars of a series at one size.
#[derive(Debug, Clone, PartialEq)]
pub struct Bars<B> {
    pub bar: RunBar,
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
    bar: RunBar,
    columns: usize,
) -> Option<Bars<B>> {
    let span = Span::of(series)?;
    let columns = i64::try_from(columns.max(1)).unwrap_or(i64::MAX);
    let bar = match bar {
        RunBar::Auto => RunBar::fixed()
            .find(|candidate| shows_whole_series(series, *candidate, columns, span))
            .unwrap_or(RunBar::OneDay),
        fixed => fixed,
    };
    let tier = bar.tier()?;
    let window = window_of(series, tier, columns, span);
    Some(Bars {
        bar,
        step_ns: step_of(tier),
        window,
        bars: collect(series, tier, window),
    })
}

/// Whether `bar`'s window starts by the series' first sample and its size
/// still holds that sample's bucket.
fn shows_whole_series<B: Bucket>(
    series: &TieredSeries<B>,
    bar: RunBar,
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
    let step = step_of(tier);
    let end = tier
        .align(span.last, series.shift_ns())
        .saturating_add(step);
    (end.saturating_sub(columns.saturating_mul(step)), end)
}

/// The buckets of `tier` in the window, the latest with what its sources
/// fill.
fn collect<B: Bucket>(series: &TieredSeries<B>, tier: Tier, (start, end): (i64, i64)) -> Vec<B> {
    let in_window = |at: i64| at >= start && at < end;
    let mut bars: Vec<B> = series
        .buckets(tier)
        .filter(|bucket| in_window(bucket.start_utc_ns()))
        .copied()
        .collect();
    for unmerged in series.unmerged(tier) {
        let at = tier.align(unmerged.start_utc_ns(), series.shift_ns());
        if !in_window(at) {
            continue;
        }
        match bars.last_mut() {
            Some(last) if last.start_utc_ns() == at => last.merge(unmerged),
            _ => bars.push(merged_at(at, unmerged)),
        }
    }
    bars.retain(|bar| !bar.is_empty());
    bars
}

/// Where the series' samples begin and end.
#[derive(Debug, Clone, Copy)]
struct Span {
    /// The earliest bucket start of the coarsest size: every size holds
    /// the series from there (the finer ones may have let it go).
    first: i64,
    /// The latest bucket start of any size.
    last: i64,
}

impl Span {
    fn of<B: Bucket>(series: &TieredSeries<B>) -> Option<Self> {
        let mut first = None;
        let mut last: Option<i64> = None;
        for tier in Tier::ALL {
            let mut starts = series.buckets(tier).map(Bucket::start_utc_ns);
            let Some(start) = starts.next() else {
                continue;
            };
            let end = starts.last().unwrap_or(start);
            last = Some(last.map_or(end, |last| last.max(end)));
            // A coarse bucket starts on its boundary, before its samples
            // may: the finest first start not cut off by a trim.
            first = Some(match first {
                Some(earliest) if start.saturating_add(step_of(tier)) > earliest => earliest,
                _ => start,
            });
        }
        Some(Self {
            first: first?,
            last: last?,
        })
    }
}

pub(crate) fn step_of(tier: Tier) -> i64 {
    i64::try_from(tier.step_ns()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use dgxtop_core::process_history::{RUN_METRICS, RunBucket, RunMetric, TIER_BUCKETS};

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
        let bars = bars_of(&series, RunBar::ThirtyMinutes, 10).unwrap();
        assert_eq!(bars.step_ns, 30 * MINUTE);
        assert_eq!(
            bars.window.1,
            MIDNIGHT + 90 * MINUTE,
            "ends with the latest bar"
        );
        assert_eq!(bars.window.0, bars.window.1 - 10 * 30 * MINUTE);
        assert_eq!(means(&bars), vec![(7, 10.0), (8, 10.0), (9, 40.0)]);
        let hours = bars_of(&series, RunBar::OneHour, 10).unwrap();
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
        let fit = |columns| bars_of(&series, RunBar::Auto, columns).unwrap().bar;
        assert_eq!(fit(100), RunBar::OneMinute);
        assert_eq!(fit(60), RunBar::FiveMinutes);
        assert_eq!(fit(3), RunBar::ThirtyMinutes);
        assert_eq!(fit(1), RunBar::TwoHours);
        assert!(bars_of(&RunSeries::default(), RunBar::Auto, 10).is_none());
    }

    /// Each size keeps its latest buckets only: a run of 20 days shows 20
    /// day bars, and its minutes only the latest hours.
    #[test]
    fn a_long_run_keeps_its_whole_life_at_the_coarse_sizes() {
        let mut series = RunSeries::new(8 * HOUR);
        sampled(&mut series, (MIDNIGHT - 8 * HOUR, 20 * 24 * 60, 60), |_| {
            2.0
        });
        let days = bars_of(&series, RunBar::OneDay, 30).unwrap();
        assert_eq!(days.bars.len(), 20, "20 bars, 20 local days");
        assert!(
            days.bars
                .iter()
                .all(|bar| bar.value(RunMetric::Cpu).count == 24 * 60)
        );
        assert_eq!(days.bars[0].start_utc_ns, MIDNIGHT - 8 * HOUR);
        let minutes = bars_of(&series, RunBar::OneMinute, 1_000).unwrap();
        assert_eq!(
            minutes.bars.len(),
            TIER_BUCKETS + 1,
            "the latest minutes kept and the one being filled"
        );
        assert_eq!(
            bars_of(&series, RunBar::Auto, 30).unwrap().bar,
            RunBar::EighteenHours,
            "the finest whose 30 bars span the 20 days"
        );
        let eighteen = bars_of(&series, RunBar::EighteenHours, 30).unwrap();
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
        let hours = bars_of(&series, RunBar::OneHour, 2).unwrap();
        assert_eq!(hours.window, (HOUR, 3 * HOUR), "the last two hours");
        let counts: Vec<(i64, u32)> = hours
            .bars
            .iter()
            .map(|bar| (bar.start_utc_ns / HOUR, bar.samples))
            .collect();
        assert_eq!(counts, vec![(1, 3_600), (2, 3_600)]);
        let auto = bars_of(&series, RunBar::Auto, 4).unwrap();
        assert_eq!(auto.bar, RunBar::OneHour, "4 hour bars hold 3 hours");
        assert!(bars_of(&TieredSeries::<Tally>::default(), RunBar::OneHour, 4).is_none());
    }

    /// An empty bucket is no bar: a size can hold a bucket with no sample.
    #[test]
    fn empty_buckets_are_left_out() {
        let mut series = TieredSeries::<Tally>::default();
        series.restore(Tier::OneMinute, Tally::new(0), false);
        let counted = Tally {
            start_utc_ns: MINUTE,
            samples: 2,
        };
        series.restore(Tier::OneMinute, counted, false);
        let bars = bars_of(&series, RunBar::OneMinute, 10).unwrap();
        assert_eq!(bars.bars, vec![counted]);
    }
}
