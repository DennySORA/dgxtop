//! The Manager's Overview reads (`OverviewReader`, D-82): the bars of a
//! size and the window statistics of one series, answered from the
//! history's sizes, with the minutes not finalized yet (from the raw
//! steps, up to now) merged into the bar and the windows holding them.
//! Each piece is taken under its own short lock, never both at once. The
//! bars come whole each time (the sizes are the history's cache), with the
//! window's totals and, for a memory chart, the tracked processes' parts
//! from the process memory record, one cell per bar (D-97, D-99).

use std::collections::BTreeSet;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::{ErrorCode, ErrorDetail};
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::EntityId;
use dgxtop_core::overview_history::{
    OverviewAnswer, OverviewBars, OverviewBucket, OverviewRequest,
};
use dgxtop_core::ports::{CancelFlag, OverviewReader};
use dgxtop_core::tiers::Tier;

use crate::HistoryIndex;
use crate::grid::utc_minute_ending_near;

impl OverviewReader for HistoryIndex {
    fn read(
        &self,
        request: &OverviewRequest,
        deadline_boot_ns: u64,
        cancel: &CancelFlag,
        clock: &dyn TimeProvider,
    ) -> Result<OverviewAnswer, ErrorDetail> {
        let selector = request.series().metric_id.as_str();
        if cancel.is_cancelled() {
            return Err(
                ErrorDetail::new(ErrorCode::Cancelled, "overview read cancelled")
                    .with_affected(selector),
            );
        }
        if clock.now_boot_ns() >= deadline_boot_ns {
            return Err(
                ErrorDetail::new(ErrorCode::TimedOut, "overview read passed its deadline")
                    .with_affected(selector),
            );
        }
        let extra = self.unfinalized_minutes(request.series(), clock);
        let tiers = self.overview();
        Ok(match request {
            OverviewRequest::Bars {
                series,
                tier,
                from_utc_ns,
                end_utc_ns,
                parts_of,
                ..
            } => {
                let window = (*from_utc_ns, *end_utc_ns);
                let mut bars = tiers.bars(series, *tier, window, &extra);
                if !parts_of.is_empty() {
                    self.join_parts(&mut bars, parts_of, (*tier, window));
                }
                OverviewAnswer::Bars(bars)
            }
            OverviewRequest::Windows {
                series,
                spans_ns,
                at_utc_ns,
            } => OverviewAnswer::Windows(tiers.windows(series, spans_ns, *at_utc_ns, &extra)),
        })
    }
}

impl HistoryIndex {
    /// Each tracked process's part of what `gpus` held, one cell for each
    /// bar of `tier` in `window`, so a part stacks on the bar it belongs
    /// to; none without the process record.
    fn join_parts(
        &self,
        bars: &mut OverviewBars,
        gpus: &BTreeSet<EntityId>,
        (tier, (start, end)): (Tier, (i64, i64)),
    ) {
        let Some(memory) = self.process_memory() else {
            return;
        };
        let cells = u64::try_from(end.saturating_sub(start)).unwrap_or(0) / tier.step_ns();
        let window = (
            u64::try_from(start).unwrap_or(0),
            u64::try_from(end).unwrap_or(0),
        );
        if let Ok(cells) = usize::try_from(cells)
            && cells > 0
        {
            bars.parts = memory.parts(gpus, window, cells);
        }
    }

    /// `series`' minutes since its last finalized one, up to now, on the
    /// wall clock of `clock`, each filed in its one UTC minute like the
    /// finalized ones.
    fn unfinalized_minutes(
        &self,
        series: &SeriesInEveryBoot,
        clock: &dyn TimeProvider,
    ) -> Vec<OverviewBucket> {
        let key = self
            .overview()
            .key_of(series)
            .or_else(|| self.read().key_in_this_boot(series));
        let Some(key) = key else {
            return Vec::new();
        };
        let now_boot_ns = clock.now_boot_ns();
        let offset_ns = clock.now_utc_ns().saturating_sub(i128::from(now_boot_ns));
        let minutes = self.read().provisional_minutes(&key, now_boot_ns);
        minutes
            .into_iter()
            .filter(|minute| minute.covered_ns > 0)
            .filter_map(|minute| {
                let end_utc_ns = i128::from(minute.end_ns).saturating_add(offset_ns);
                let start = utc_minute_ending_near(i64::try_from(end_utc_ns).ok()?);
                Some(OverviewBucket::gauge(
                    start,
                    minute.covered_ns,
                    minute.weighted_sum,
                    (minute.minimum, minute.maximum),
                    minute.sample_count,
                ))
            })
            .collect()
    }
}
