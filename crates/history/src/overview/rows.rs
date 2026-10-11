//! The finalized minutes of the Overview series as the sizes take them
//! (D-82): the same rows the archive queue journals, placed on the wall
//! clock by the boot-to-UTC pairing. A minute starts on a whole UTC minute
//! (the boot's grid), so it is filed in exactly that UTC minute, never in
//! pieces of two. Only the Overview series in their
//! canonical unit count (units never mix); a minute without time held
//! is a gap and adds nothing. Bridges (counter intervals across minutes)
//! are left out: the Overview series are gauges.

use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::metric::descriptor;
use dgxtop_core::overview_history::{OverviewBucket, is_overview_series};

use super::book::TierMinute;
use crate::grid::utc_minute_ending_near;
use crate::output::ArchiveRow;
use crate::segment::SegmentMeta;
use crate::wall_clock::WallClock;

/// The Overview minutes among `rows`; `None` while no boot-to-UTC pairing
/// is known (the rows wait).
pub(crate) fn tier_minutes(rows: &[ArchiveRow], clock: &WallClock) -> Option<Vec<TierMinute>> {
    if !clock.is_known() {
        return None;
    }
    Some(
        rows.iter()
            .filter_map(|row| tier_minute(row, clock))
            .collect(),
    )
}

/// Whether a row is a minute of an Overview series (worth keeping until
/// it can be placed on the wall clock).
pub(crate) fn is_overview_row(row: &ArchiveRow) -> bool {
    match row {
        ArchiveRow::GaugeBucket { segment, .. } | ArchiveRow::CounterBucket { segment, .. } => {
            is_overview_segment(segment)
        }
        ArchiveRow::Bridge { .. } | ArchiveRow::Gap(_) => false,
    }
}

fn is_overview_segment(segment: &SegmentMeta) -> bool {
    let metric = segment.key.metric_id.as_str();
    is_overview_series(metric, segment.key.entity_id.as_str())
        && descriptor(metric).is_some_and(|known| known.unit == segment.unit)
}

fn tier_minute(row: &ArchiveRow, clock: &WallClock) -> Option<TierMinute> {
    let (segment, bucket) = match row {
        ArchiveRow::GaugeBucket {
            segment,
            bucket,
            end_ns,
        } => {
            let start = filed_utc_minute(clock, *end_ns)?;
            (
                segment,
                OverviewBucket::gauge(
                    start,
                    bucket.covered_ns,
                    bucket.weighted_sum,
                    (bucket.minimum, bucket.maximum),
                    bucket.sample_count,
                ),
            )
        }
        ArchiveRow::CounterBucket {
            segment,
            bucket,
            end_ns,
        } => {
            let start = filed_utc_minute(clock, *end_ns)?;
            (
                segment,
                OverviewBucket::counter(
                    start,
                    bucket.covered_ns,
                    bucket.total.integral(),
                    bucket.interval_count,
                ),
            )
        }
        ArchiveRow::Bridge { .. } | ArchiveRow::Gap(_) => return None,
    };
    if bucket.is_empty() || !is_overview_segment(segment) {
        return None;
    }
    Some(TierMinute {
        series: SeriesInEveryBoot::from(&segment.key),
        key: segment.key.clone(),
        bucket,
    })
}

/// The UTC minute a minute ending at boot `end_ns` is filed under.
fn filed_utc_minute(clock: &WallClock, end_ns: u64) -> Option<i64> {
    i64::try_from(utc_minute_ending_near(clock.utc_of(end_ns)?)).ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use dgxtop_core::history::HistoryMethod;
    use dgxtop_core::metric::{CanonicalUnit, ids};
    use dgxtop_core::quality::SampleKind;
    use dgxtop_core::testing::{metric_key, source_id};
    use uuid::Uuid;

    use super::*;
    use crate::records::{BUCKET_NS, CounterBucket, GaugeBucket};
    use crate::segment::SegmentIdentity;

    /// A whole UTC minute, so the boot grid's minutes are UTC minutes.
    const UTC_AT_BOOT: i128 = 1_790_000_040_000_000_000;

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
            opens_series: true,
        })
    }

    fn gauge_row(metric: &str, unit: CanonicalUnit, covered: u64) -> ArchiveRow {
        let mut bucket = GaugeBucket::empty(BUCKET_NS, 0);
        if covered > 0 {
            bucket.add_step(40.0, covered);
        }
        bucket.sample_count = 3;
        ArchiveRow::GaugeBucket {
            segment: segment(metric, unit),
            bucket,
            end_ns: 2 * BUCKET_NS,
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
        assert!(
            tier_minutes(&rows, &WallClock::default()).is_none(),
            "no pairing yet"
        );
        assert!(is_overview_row(&rows[0]) && !is_overview_row(&rows[1]));
        let minutes = tier_minutes(&rows, &clock()).unwrap();
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
            let mut bucket = GaugeBucket::empty(start_ns, 0);
            bucket.add_step(40.0, grid.end_of(start_ns) - start_ns);
            ArchiveRow::GaugeBucket {
                segment: segment(ids::GPU_TEMPERATURE, CanonicalUnit::DegC),
                bucket,
                end_ns: grid.end_of(start_ns),
            }
        };
        let starts = [0, grid.phase_ns(), grid.phase_ns() + BUCKET_NS];
        let rows: Vec<ArchiveRow> = starts.iter().map(|start| row(*start)).collect();
        clock.observe(10 * second, boot_utc + i128::from(10 * second) - 1_000_000);
        let minutes = tier_minutes(&rows, &clock).unwrap();
        let minute = i64::try_from(BUCKET_NS).unwrap();
        let first = i64::try_from(UTC_AT_BOOT).unwrap();
        let filed: Vec<i64> = minutes.iter().map(|m| m.bucket.start_utc_ns).collect();
        assert_eq!(filed, vec![first, first + minute, first + 2 * minute]);
        assert_eq!(minutes[0].bucket.covered_ns, 23 * second, "the head minute");
    }

    #[test]
    fn a_counter_minute_keeps_its_exact_total() {
        let mut bucket = CounterBucket::empty(0, 0);
        bucket.add_interval(&crate::records::CounterSample {
            start_ns: 0,
            end_ns: BUCKET_NS / 2,
            delta: dgxtop_core::metric::Scalar::from_u64(700),
            segment: 0,
        });
        let row = ArchiveRow::CounterBucket {
            segment: segment(ids::GPU_POWER_DRAW, CanonicalUnit::Watts),
            bucket,
            end_ns: BUCKET_NS,
        };
        let minutes = tier_minutes(&[row], &clock()).unwrap();
        assert_eq!(
            (minutes[0].bucket.total, minutes[0].bucket.total_exact),
            (700, true)
        );
    }
}
