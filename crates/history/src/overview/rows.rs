//! The finalized minutes of the Overview series as the sizes take them
//! (D-82): the same rows the archive queue journals, placed on the wall
//! clock by the boot-to-UTC pairing. A minute starts on a whole UTC minute
//! (the boot's grid), so it is filed in exactly that UTC minute, never in
//! pieces of two. Only the Overview series in their
//! canonical unit count (units never mix); a minute without time held
//! is a gap and adds nothing. Bridges (counter intervals across minutes)
//! are left out: the Overview series are gauges. A minute the wall clock
//! cannot place is lost Overview time and comes back as a gap, never
//! dropped in silence.

use dgxtop_core::canonical::GapRecord;
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::metric::descriptor;
use dgxtop_core::overview_history::{OverviewBucket, is_overview_series};

use super::book::TierMinute;
use crate::grid::utc_minute_ending_near;
use crate::output::ArchiveRow;
use crate::segment::SegmentMeta;
use crate::wall_clock::WallClock;

/// Gap reason of a finished Overview minute the wall clock could not place.
pub(crate) const UNPLACED_REASON: &str = "overview_unplaced";

/// The Overview minutes among a batch of finished rows.
#[derive(Debug, Default)]
pub(crate) struct OverviewMinutes {
    /// Placed on the wall clock, for the sizes.
    pub placed: Vec<TierMinute>,
    /// Those that could not be placed (no pairing yet, or a UTC an
    /// Overview bucket cannot hold), as gaps of their series.
    pub unplaced: Vec<GapRecord>,
}

/// Sorts the Overview minutes among `rows` into placed and unplaced ones;
/// rows of other series, bridges, gaps and minutes without time held are
/// not Overview minutes.
pub(crate) fn overview_minutes<'r>(
    rows: impl IntoIterator<Item = &'r ArchiveRow>,
    clock: &WallClock,
) -> OverviewMinutes {
    let mut minutes = OverviewMinutes::default();
    for row in rows {
        let Some(minute) = OverviewRow::of(row) else {
            continue;
        };
        match minute.place(clock) {
            Some(placed) => minutes.placed.push(placed),
            None => minutes.unplaced.push(minute.as_gap()),
        }
    }
    minutes
}

fn is_overview_segment(segment: &SegmentMeta) -> bool {
    let metric = segment.key.metric_id.as_str();
    is_overview_series(metric, segment.key.entity_id.as_str())
        && descriptor(metric).is_some_and(|known| known.unit == segment.unit)
}

/// One finished minute of an Overview series, still on the boot clock.
struct OverviewRow<'r> {
    segment: &'r SegmentMeta,
    start_ns: u64,
    end_ns: u64,
    /// Its UTC start is set once it is placed.
    bucket: OverviewBucket,
}

impl<'r> OverviewRow<'r> {
    fn of(row: &'r ArchiveRow) -> Option<Self> {
        let (segment, start_ns, end_ns, bucket) = match row {
            ArchiveRow::GaugeBucket { segment, bucket } => (
                segment,
                bucket.start_ns,
                bucket.end_ns,
                OverviewBucket::gauge(
                    0,
                    bucket.covered_ns,
                    bucket.weighted_sum,
                    (bucket.minimum, bucket.maximum),
                    bucket.sample_count,
                ),
            ),
            ArchiveRow::CounterBucket { segment, bucket } => (
                segment,
                bucket.start_ns,
                bucket.end_ns,
                OverviewBucket::counter(
                    0,
                    bucket.covered_ns,
                    bucket.total.integral(),
                    bucket.interval_count,
                ),
            ),
            ArchiveRow::Bridge { .. } | ArchiveRow::Gap(_) => return None,
        };
        (!bucket.is_empty() && is_overview_segment(segment)).then_some(Self {
            segment,
            start_ns,
            end_ns,
            bucket,
        })
    }

    /// The minute on the wall clock, filed under the UTC minute it ends
    /// nearest.
    fn place(&self, clock: &WallClock) -> Option<TierMinute> {
        let end_utc_ns = i64::try_from(clock.utc_of(self.end_ns)?).ok()?;
        let start = utc_minute_ending_near(end_utc_ns);
        Some(TierMinute {
            series: SeriesInEveryBoot::from(&self.segment.key),
            key: self.segment.key.clone(),
            bucket: OverviewBucket {
                start_utc_ns: start,
                ..self.bucket
            },
        })
    }

    fn as_gap(&self) -> GapRecord {
        GapRecord {
            key: Some(self.segment.key.clone()),
            adapter_id: None,
            start_boot_ns: self.start_ns,
            end_boot_ns: self.end_ns,
            reason: UNPLACED_REASON.to_owned(),
            affected_count: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use dgxtop_core::history::HistoryMethod;
    use dgxtop_core::metric::{CanonicalUnit, ids};
    use dgxtop_core::quality::SampleKind;
    use dgxtop_core::testing::{UTC_ON_A_WHOLE_MINUTE_NS as UTC_AT_BOOT, metric_key, source_id};
    use uuid::Uuid;

    use super::*;
    use crate::records::{BUCKET_NS, CounterBucket, GaugeBucket};
    use crate::segment::SegmentIdentity;

    fn segment(metric: &str, unit: CanonicalUnit) -> Arc<SegmentMeta> {
        Arc::new(SegmentMeta {
            uuid: Uuid::new_v4(),
            key: metric_key("gpu:0", metric, "gpu"),
            identity: SegmentIdentity {
                source_id: source_id("nvml:0"),
                source_generation: 1,
                policy_revision: 1,
                sample_kind: SampleKind::Measured,
                method: HistoryMethod::TimeWeightedStep,
            },
            unit,
            started_ns: 0,
        })
    }

    fn gauge_row(metric: &str, unit: CanonicalUnit, covered: u64) -> ArchiveRow {
        let mut bucket = GaugeBucket::empty(BUCKET_NS, 2 * BUCKET_NS, 0);
        if covered > 0 {
            bucket.add_step(40.0, covered);
        }
        bucket.sample_count = 3;
        ArchiveRow::GaugeBucket {
            segment: segment(metric, unit),
            bucket,
        }
    }

    fn clock() -> WallClock {
        let mut clock = WallClock::default();
        clock.observe(0, UTC_AT_BOOT);
        clock
    }

    /// An Overview gauge minute lands on the wall clock with what makes it
    /// merge exactly; other metrics, units, entities, gaps and empty
    /// minutes do not.
    #[test]
    fn overview_minutes_are_placed_on_the_wall_clock() {
        let rows = vec![
            gauge_row(ids::GPU_TEMPERATURE, CanonicalUnit::DegC, BUCKET_NS),
            gauge_row(ids::GPU_TEMPERATURE, CanonicalUnit::Count, BUCKET_NS),
            gauge_row(
                ids::CPU_UTILIZATION_IOWAIT,
                CanonicalUnit::Percent,
                BUCKET_NS,
            ),
            gauge_row(ids::GPU_TEMPERATURE, CanonicalUnit::DegC, 0),
            // A CPU series of one entity that is not the whole CPU.
            gauge_row(
                ids::CPU_UTILIZATION_TOTAL,
                CanonicalUnit::Percent,
                BUCKET_NS,
            ),
        ];
        let unpaired = overview_minutes(&rows, &WallClock::default());
        assert!(unpaired.placed.is_empty(), "no pairing yet");
        let lost: Vec<(u64, u64, &str)> = unpaired
            .unplaced
            .iter()
            .map(|gap| (gap.start_boot_ns, gap.end_boot_ns, gap.reason.as_str()))
            .collect();
        assert_eq!(lost, vec![(BUCKET_NS, 2 * BUCKET_NS, UNPLACED_REASON)]);
        let placed = overview_minutes(&rows, &clock());
        assert!(placed.unplaced.is_empty());
        let minutes = placed.placed;
        assert_eq!(minutes.len(), 1);
        let minute = &minutes[0];
        let expected = i64::try_from(UTC_AT_BOOT).unwrap() + i64::try_from(BUCKET_NS).unwrap();
        assert_eq!(minute.bucket.start_utc_ns, expected);
        assert_eq!(minute.bucket.mean(), Some(40.0));
        assert_eq!(minute.bucket.covered_ns, BUCKET_NS);
        assert_eq!(minute.bucket.samples, 3);
        assert_eq!(minute.series.metric_id.as_str(), ids::GPU_TEMPERATURE);
        assert_eq!(minute.key.boot_id.as_str(), "fake-boot");
    }

    /// A boot 37 s into a minute: its head minute (boot 0–23 s) is filed in
    /// the minute boot fell in and the next in the following one, each in
    /// one UTC minute, even after the clock slewed back a millisecond.
    #[test]
    fn each_minute_of_a_mid_minute_boot_is_filed_in_one_utc_minute() {
        let second = 1_000_000_000u64;
        let boot_utc = UTC_AT_BOOT + 37 * i128::from(second);
        let mut clock = WallClock::default();
        clock.observe(0, boot_utc);
        let grid = crate::grid::MinuteGrid::on_utc_minutes(0, boot_utc);
        let row = |start_ns: u64| {
            let mut bucket = GaugeBucket::empty(start_ns, grid.end_of(start_ns), 0);
            bucket.add_step(40.0, grid.end_of(start_ns) - start_ns);
            ArchiveRow::GaugeBucket {
                segment: segment(ids::GPU_TEMPERATURE, CanonicalUnit::DegC),
                bucket,
            }
        };
        let starts = [0, grid.phase_ns(), grid.phase_ns() + BUCKET_NS];
        let rows: Vec<ArchiveRow> = starts.iter().map(|start| row(*start)).collect();
        clock.observe(10 * second, boot_utc + i128::from(10 * second) - 1_000_000);
        let minutes = overview_minutes(&rows, &clock).placed;
        let minute = i64::try_from(BUCKET_NS).unwrap();
        let first = i64::try_from(UTC_AT_BOOT).unwrap();
        let filed: Vec<i64> = minutes.iter().map(|m| m.bucket.start_utc_ns).collect();
        assert_eq!(filed, vec![first, first + minute, first + 2 * minute]);
        assert_eq!(minutes[0].bucket.covered_ns, 23 * second, "the head minute");
    }

    #[test]
    fn a_counter_minute_keeps_its_exact_total() {
        let mut bucket = CounterBucket::empty(0, BUCKET_NS, 0);
        bucket.add_interval(&crate::records::CounterSample {
            start_ns: 0,
            end_ns: BUCKET_NS / 2,
            delta: dgxtop_core::metric::Scalar::from_u64(700),
            segment: 0,
        });
        let row = ArchiveRow::CounterBucket {
            segment: segment(ids::GPU_POWER_DRAW, CanonicalUnit::Watts),
            bucket,
        };
        let minutes = overview_minutes(&[row], &clock()).placed;
        assert_eq!(
            (minutes[0].bucket.total, minutes[0].bucket.total_exact),
            (700, true)
        );
    }
}
