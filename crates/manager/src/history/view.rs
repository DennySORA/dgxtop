//! What a session's history view shows (D-46, D-48, D-50, D-51): every
//! chart's points on the wall clock, the archive's from earlier runs and
//! boots before this run's from memory, with the mean, peak and covered
//! time of both, and each GPU's statistics over 1 h to 24 h from both.

use std::collections::BTreeMap;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::history::{
    ArchivePoint, ArchiveResult, HistoryMethod, HistoryPoint, HistoryResult,
};
use dgxtop_core::metric::{CanonicalUnit, Scalar, descriptor};
use dgxtop_core::view::{HistoryChartKind, HistoryPointView, HistoryWindowSummary};

const NANOS_PER_SECOND_F64: f64 = 1e9;

/// This run's boot clock on the wall clock, paired when a view is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Wall {
    offset_ns: i128,
}

impl Wall {
    pub(super) fn now(clock: &dyn TimeProvider) -> Self {
        Self {
            offset_ns: clock
                .now_utc_ns()
                .saturating_sub(i128::from(clock.now_boot_ns())),
        }
    }

    /// The wall-clock time of `boot_ns` (0 before the epoch).
    pub(super) fn utc_of(self, boot_ns: u64) -> u64 {
        u64::try_from(i128::from(boot_ns).saturating_add(self.offset_ns)).unwrap_or(0)
    }
}

/// A window on the wall clock, `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct UtcWindow {
    pub(super) start: u64,
    pub(super) end: u64,
}

impl UtcWindow {
    pub(super) fn span(self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

/// What one series holds for a window: this run's mean and maximum from
/// memory and the earlier runs' minutes from the archive.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Evidence<'a> {
    pub(super) mean: Option<&'a HistoryResult>,
    pub(super) maximum: Option<&'a HistoryResult>,
    pub(super) archived: Option<&'a ArchiveResult>,
    /// This run's start on the wall clock when the archive answers before
    /// it: the archive's points end there and this run's begin there, so
    /// the points stay in time order without overlap (memory answers the
    /// whole window, empty before this run).
    pub(super) seam: Option<u64>,
}

/// A value and the time it held.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Held {
    value: f64,
    covered_ns: u64,
}

impl Evidence<'_> {
    fn memory_points(&self) -> &[HistoryPoint] {
        self.mean
            .map(|result| result.points.as_slice())
            .unwrap_or_default()
    }

    /// The archive's points that reach into `window` and end by the seam,
    /// each with the time it holds there (a point the window's start cuts
    /// holds at most what is left of it). A point that ends after the seam
    /// is left out: this run writes the archive too, so the minute it
    /// started in sums samples memory answers after the seam, and cutting
    /// it at the seam would draw them before dgxtop started and count them
    /// twice. At most the minute before this run's start is lost.
    fn archived_in(&self, window: UtcWindow) -> impl Iterator<Item = (ArchivePoint, u64)> + '_ {
        let seam = self.seam.unwrap_or(u64::MAX);
        self.archived
            .map(|result| result.points.as_slice())
            .unwrap_or_default()
            .iter()
            .filter(move |point| point.end_utc_ns <= seam)
            .filter_map(move |point| {
                let start = point.start_utc_ns.max(window.start);
                (point.end_utc_ns > start).then(|| {
                    (
                        point.clone(),
                        point.covered_ns.min(point.end_utc_ns - start),
                    )
                })
            })
    }

    /// Every value with the time it held: the archive's then memory's.
    fn held(&self, window: UtcWindow) -> Vec<Held> {
        let archived = self.archived_in(window).filter_map(|(point, covered_ns)| {
            Some(Held {
                value: point.mean.filter(|value| value.is_finite())?,
                covered_ns,
            })
        });
        let memory = self.memory_points().iter().filter_map(|point| {
            Some(Held {
                value: value_of(point)?,
                covered_ns: point.covered_ns,
            })
        });
        archived.chain(memory).collect()
    }

    /// Time held in `window`.
    pub(super) fn covered_ns(&self, window: UtcWindow) -> Option<u64> {
        if self.mean.is_none() && self.archived.is_none() {
            return None;
        }
        let archived = self
            .archived_in(window)
            .map(|(_, covered_ns)| covered_ns)
            .fold(0, u64::saturating_add);
        let memory = self.mean.map_or(0, |result| result.covered_ns);
        Some(archived.saturating_add(memory))
    }

    /// Time-weighted mean over the time held.
    pub(super) fn mean(&self, window: UtcWindow) -> Option<f64> {
        weighted_mean(
            self.held(window)
                .into_iter()
                .map(|held| (held.value, held.covered_ns)),
        )
    }

    /// Largest value: the maxima where points merge samples, else the means.
    pub(super) fn peak(&self, window: UtcWindow) -> Option<f64> {
        let archived = self
            .archived_in(window)
            .filter_map(|(point, _)| point.max.or(point.mean));
        let memory = self
            .maximum
            .map_or(self.memory_points(), |result| result.points.as_slice())
            .iter()
            .filter_map(value_of);
        archived
            .chain(memory)
            .filter(|value| value.is_finite())
            .reduce(f64::max)
    }

    /// Mean times held time in seconds (joules for power).
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn integral(&self, window: UtcWindow) -> f64 {
        self.held(window)
            .into_iter()
            .map(|held| held.value * held.covered_ns as f64 / NANOS_PER_SECOND_F64)
            .sum()
    }

    /// The chart's points on the wall clock, oldest first and without
    /// overlap: the archive's up to the seam, this run's from it.
    pub(super) fn points(&self, wall: Wall, window: UtcWindow) -> Vec<HistoryPointView> {
        let seam = self.seam.unwrap_or(0);
        let archived = self.archived_in(window).map(|(point, _)| HistoryPointView {
            start_utc_ns: point.start_utc_ns,
            end_utc_ns: point.end_utc_ns,
            mean: point.mean,
            min: None,
            max: point.max.or(point.mean),
            method: HistoryMethod::TimeWeightedStep,
        });
        let maximums: BTreeMap<u64, f64> = self
            .maximum
            .map(|result| {
                result
                    .points
                    .iter()
                    .filter_map(|point| Some((point.start_boot_ns, value_of(point)?)))
                    .collect()
            })
            .unwrap_or_default();
        let memory = self.memory_points().iter().filter_map(|point| {
            let start = wall.utc_of(point.start_boot_ns).max(seam);
            let end = wall.utc_of(point.end_boot_ns);
            let mean = value_of(point);
            (end > start).then(|| HistoryPointView {
                start_utc_ns: start,
                end_utc_ns: end,
                mean,
                min: None,
                max: maximums.get(&point.start_boot_ns).copied().or(mean),
                method: point.method,
            })
        });
        archived.chain(memory).collect()
    }
}

/// Unit of a series' metric (counts when unknown).
pub(super) fn unit_of(metric_id: &str) -> CanonicalUnit {
    descriptor(metric_id).map_or(CanonicalUnit::Count, |known| known.unit)
}

/// One statistics row: a GPU series over one window.
pub(super) struct SummaryInput<'a> {
    pub(super) kind: HistoryChartKind,
    pub(super) subject: &'a str,
    pub(super) metric_id: &'a str,
    pub(super) window_label: &'a str,
    pub(super) window: UtcWindow,
    pub(super) evidence: Evidence<'a>,
    pub(super) pending: bool,
}

pub(super) fn summary(input: &SummaryInput<'_>) -> HistoryWindowSummary {
    let evidence = &input.evidence;
    let covered_ns = evidence.covered_ns(input.window);
    let answered = evidence.mean.is_some() || evidence.archived.is_some();
    HistoryWindowSummary {
        kind: input.kind,
        subject: input.subject.to_owned(),
        window_label: input.window_label.to_owned(),
        unit: unit_of(input.metric_id),
        mean: evidence.mean(input.window),
        peak: evidence.peak(input.window),
        coverage_ratio: covered_ns
            .map(|covered| HistoryResult::ratio(covered, input.window.span())),
        covered_ns,
        integral: (input.kind == HistoryChartKind::GpuPower && answered)
            .then(|| evidence.integral(input.window)),
        pending: input.pending,
    }
}

fn value_of(point: &HistoryPoint) -> Option<f64> {
    point
        .value
        .map(Scalar::to_f64_lossy)
        .filter(|value| value.is_finite())
}

#[allow(clippy::cast_precision_loss)]
pub(super) fn weighted_mean(values: impl Iterator<Item = (f64, u64)>) -> Option<f64> {
    let (sum, weight) = values.fold((0.0, 0.0), |(sum, weight), (value, covered)| {
        let covered = covered as f64;
        (sum + value * covered, weight + covered)
    });
    (weight > 0.0).then(|| sum / weight)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::history::HISTORY_SCHEMA_VERSION;
    use dgxtop_core::testing::{FakeClock, metric_key};
    use uuid::Uuid;

    const MINUTE: u64 = 60_000_000_000;

    fn memory(points: &[(u64, f64)]) -> HistoryResult {
        HistoryResult {
            schema_version: HISTORY_SCHEMA_VERSION,
            request_id: Uuid::nil(),
            key: metric_key("gpu:GPU-1", "gpu.power.draw", "gpu"),
            start_boot_ns: 0,
            end_boot_ns: 100 * MINUTE,
            covered_ns: MINUTE * u64::try_from(points.len()).unwrap(),
            coverage_ratio: 0.0,
            truncated: false,
            points: points
                .iter()
                .map(|(start, value)| HistoryPoint {
                    start_boot_ns: *start,
                    end_boot_ns: start + MINUTE,
                    value: Some(Scalar::from_f64(*value).unwrap()),
                    covered_ns: MINUTE,
                    method: HistoryMethod::TimeWeightedStep,
                    source_ids: Vec::new(),
                    reason: None,
                })
                .collect(),
            source_changes: Vec::new(),
            clock_discontinuities: Vec::new(),
            snapshot_revision: 1,
            next_start_boot_ns: None,
        }
    }

    fn archived(points: &[(u64, u64, f64, f64, u64)]) -> ArchiveResult {
        ArchiveResult {
            request_id: Uuid::nil(),
            points: points
                .iter()
                .map(|(start, end, mean, max, covered)| ArchivePoint {
                    start_utc_ns: *start,
                    end_utc_ns: *end,
                    mean: Some(*mean),
                    max: Some(*max),
                    covered_ns: *covered,
                })
                .collect(),
            covered_ns: points.iter().map(|point| point.4).sum(),
            boots: 1,
            truncated: false,
        }
    }

    #[test]
    fn the_boot_clock_meets_the_wall_clock_at_the_pairing() {
        let clock = FakeClock::new(10 * MINUTE);
        let wall = Wall::now(clock.as_ref());
        let now_utc = u64::try_from(clock.now_utc_ns()).unwrap();
        assert_eq!(wall.utc_of(10 * MINUTE), now_utc);
        assert_eq!(wall.utc_of(9 * MINUTE), now_utc - MINUTE);
    }

    #[test]
    fn archived_minutes_come_first_and_join_this_runs_numbers() {
        // The window is minutes 100..160 on the wall clock; this run's boot
        // clock reads minute 0 at wall minute 140.
        let wall = Wall {
            offset_ns: i128::from(140 * MINUTE),
        };
        let window = UtcWindow {
            start: 100 * MINUTE,
            end: 160 * MINUTE,
        };
        let mean = memory(&[(0, 80.0), (MINUTE, 100.0)]);
        // Minute 90..120 is cut by the window: at most 20 minutes of its 30
        // count; 120..130 lies inside.
        let archive = archived(&[
            (90 * MINUTE, 120 * MINUTE, 20.0, 30.0, 30 * MINUTE),
            (120 * MINUTE, 130 * MINUTE, 40.0, 90.0, 10 * MINUTE),
            (60 * MINUTE, 90 * MINUTE, 99.0, 99.0, 30 * MINUTE),
        ]);
        let evidence = Evidence {
            mean: Some(&mean),
            maximum: None,
            archived: Some(&archive),
            seam: Some(140 * MINUTE),
        };
        let points = evidence.points(wall, window);
        let starts: Vec<u64> = points
            .iter()
            .map(|point| point.start_utc_ns / MINUTE)
            .collect();
        assert_eq!(starts, vec![90, 120, 140, 141]);
        assert_eq!(points[1].max, Some(90.0));
        assert_eq!(evidence.covered_ns(window), Some(32 * MINUTE));
        // (20×20 + 40×10 + 80 + 100) / 32 minutes.
        let expected = (20.0 * 20.0 + 40.0 * 10.0 + 80.0 + 100.0) / 32.0;
        assert!((evidence.mean(window).unwrap() - expected).abs() < 1e-9);
        assert_eq!(evidence.peak(window), Some(100.0));
        let joules = 20.0 * 1200.0 + 40.0 * 600.0 + 80.0 * 60.0 + 100.0 * 60.0;
        assert!((evidence.integral(window) - joules).abs() < 1e-6);
    }

    #[test]
    fn points_are_in_time_order_and_never_overlap_at_this_runs_start() {
        // This run's boot clock reads 0 at wall minute 100 and it started at
        // boot minute 40 (wall 140). Memory answers the whole window, empty
        // before that start; the archive's last point ends at it, and one
        // running past it is left out.
        let wall = Wall {
            offset_ns: i128::from(100 * MINUTE),
        };
        let window = UtcWindow {
            start: 100 * MINUTE,
            end: 160 * MINUTE,
        };
        let empty_before = HistoryPoint {
            start_boot_ns: 0,
            end_boot_ns: 40 * MINUTE,
            value: None,
            covered_ns: 0,
            method: HistoryMethod::Gap,
            source_ids: Vec::new(),
            reason: None,
        };
        let mut mean = memory(&[(40 * MINUTE, 80.0)]);
        mean.points.insert(0, empty_before);
        let archive = archived(&[
            (138 * MINUTE, 140 * MINUTE, 40.0, 41.0, 2 * MINUTE),
            (140 * MINUTE, 142 * MINUTE, 80.0, 80.0, MINUTE),
        ]);
        let evidence = Evidence {
            mean: Some(&mean),
            maximum: None,
            archived: Some(&archive),
            seam: Some(140 * MINUTE),
        };
        let spans: Vec<(u64, u64)> = evidence
            .points(wall, window)
            .iter()
            .map(|point| (point.start_utc_ns / MINUTE, point.end_utc_ns / MINUTE))
            .collect();
        assert_eq!(spans, vec![(138, 140), (140, 141)]);
        assert_eq!(evidence.covered_ns(window), Some(3 * MINUTE));
    }

    /// The archive also holds this run's own minutes once they are final.
    /// The minute this run started in summarizes samples memory answers
    /// after the seam: it must not be drawn before the seam (on a fresh
    /// install, as history from before dgxtop started) or counted twice.
    #[test]
    fn the_minute_this_run_started_in_is_not_taken_from_the_archive() {
        // Wall minute 100 is boot minute 0; this run started at wall
        // 140.5 min, its first sample at 140.5. The archive's minute 140
        // holds this run's first half minute.
        let wall = Wall {
            offset_ns: i128::from(100 * MINUTE),
        };
        let window = UtcWindow {
            start: 100 * MINUTE,
            end: 160 * MINUTE,
        };
        let seam = 140 * MINUTE + MINUTE / 2;
        let mut mean = memory(&[(40 * MINUTE + MINUTE / 2, 80.0)]);
        mean.covered_ns = MINUTE;
        let archive = archived(&[
            (139 * MINUTE, 140 * MINUTE, 60.0, 61.0, MINUTE),
            (140 * MINUTE, 141 * MINUTE, 80.0, 80.0, MINUTE / 2),
        ]);
        let evidence = Evidence {
            mean: Some(&mean),
            maximum: None,
            archived: Some(&archive),
            seam: Some(seam),
        };
        let spans: Vec<(u64, u64)> = evidence
            .points(wall, window)
            .iter()
            .map(|point| (point.start_utc_ns, point.end_utc_ns))
            .collect();
        assert_eq!(
            spans,
            vec![
                (139 * MINUTE, 140 * MINUTE),
                (seam, 141 * MINUTE + MINUTE / 2)
            ]
        );
        assert_eq!(evidence.covered_ns(window), Some(2 * MINUTE));
        assert!((evidence.mean(window).unwrap() - 70.0).abs() < 1e-9);
    }

    #[test]
    fn without_any_answer_nothing_is_covered() {
        let window = UtcWindow {
            start: 0,
            end: 60 * MINUTE,
        };
        let evidence = Evidence::default();
        assert_eq!(evidence.covered_ns(window), None);
        assert_eq!(evidence.mean(window), None);
        assert_eq!(evidence.peak(window), None);
        let only_archive = archived(&[(0, MINUTE, 5.0, 6.0, MINUTE)]);
        let evidence = Evidence {
            archived: Some(&only_archive),
            ..Evidence::default()
        };
        assert_eq!(evidence.covered_ns(window), Some(MINUTE));
        assert_eq!(evidence.peak(window), Some(6.0));
    }
}
