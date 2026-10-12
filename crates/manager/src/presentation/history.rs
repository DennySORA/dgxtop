//! Display downsampling of the history charts when a projection must
//! shrink to its byte budget: pixel buckets keep
//! min/max/mean, never bridge a gap or a time discontinuity, and leave the
//! authoritative result untouched. A chart is never downsampled to its
//! width: the HistoryService asks one bar per plot column already (D-50).

use dgxtop_core::view::HistoryPointView;

/// Merges contiguous points of one pixel bucket. Gap points (no mean) only
/// merge with gap points, so every break stays visible.
pub(crate) fn downsample(points: &[HistoryPointView], width: usize) -> Vec<HistoryPointView> {
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return Vec::new();
    };
    let span = last.end_utc_ns.saturating_sub(first.start_utc_ns).max(1);
    let buckets = u64::try_from(width.max(1)).unwrap_or(u64::MAX);
    let bucket_ns = span.div_ceil(buckets).max(1);
    let bucket_of = |point: &HistoryPointView| {
        point.start_utc_ns.saturating_sub(first.start_utc_ns) / bucket_ns
    };
    let mut merged = Vec::with_capacity(width.min(points.len()));
    let mut open: Option<(u64, Bucket)> = None;
    for point in points {
        let index = bucket_of(point);
        match open.as_mut() {
            Some((open_index, bucket)) if *open_index == index && bucket.can_absorb(point) => {
                bucket.absorb(point);
            }
            _ => {
                if let Some((_, bucket)) = open.take() {
                    merged.push(bucket.finish());
                }
                open = Some((index, Bucket::start(point)));
            }
        }
    }
    if let Some((_, bucket)) = open {
        merged.push(bucket.finish());
    }
    merged
}

struct Bucket {
    start_utc_ns: u64,
    end_utc_ns: u64,
    weighted_sum: f64,
    weight_ns: f64,
    min: Option<f64>,
    max: Option<f64>,
    is_gap: bool,
}

impl Bucket {
    fn start(point: &HistoryPointView) -> Self {
        let mut bucket = Self {
            start_utc_ns: point.start_utc_ns,
            end_utc_ns: point.start_utc_ns,
            weighted_sum: 0.0,
            weight_ns: 0.0,
            min: None,
            max: None,
            is_gap: point.mean.is_none(),
        };
        bucket.absorb(point);
        bucket
    }

    fn can_absorb(&self, point: &HistoryPointView) -> bool {
        point.start_utc_ns == self.end_utc_ns && point.mean.is_none() == self.is_gap
    }

    // Durations are at most a 24 h window in ns (< 2^47), exact in f64.
    #[allow(clippy::cast_precision_loss)]
    fn absorb(&mut self, point: &HistoryPointView) {
        self.end_utc_ns = point.end_utc_ns.max(self.end_utc_ns);
        let Some(mean) = point.mean.filter(|mean| mean.is_finite()) else {
            return;
        };
        let duration = point.end_utc_ns.saturating_sub(point.start_utc_ns).max(1) as f64;
        self.weighted_sum += mean * duration;
        self.weight_ns += duration;
        let low = point.min.filter(|value| value.is_finite()).unwrap_or(mean);
        let high = point.max.filter(|value| value.is_finite()).unwrap_or(mean);
        self.min = Some(self.min.map_or(low, |current| current.min(low)));
        self.max = Some(self.max.map_or(high, |current| current.max(high)));
    }

    fn finish(self) -> HistoryPointView {
        let mean = (self.weight_ns > 0.0).then(|| self.weighted_sum / self.weight_ns);
        HistoryPointView {
            start_utc_ns: self.start_utc_ns,
            end_utc_ns: self.end_utc_ns,
            mean,
            min: self.min,
            max: self.max,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: u64 = 1_000_000_000;

    fn point(start_s: u64, mean: Option<f64>) -> HistoryPointView {
        HistoryPointView {
            start_utc_ns: start_s * SECOND,
            end_utc_ns: (start_s + 1) * SECOND,
            mean,
            min: mean,
            max: mean,
        }
    }

    #[test]
    fn buckets_keep_min_max_and_time_weighted_mean() {
        let points: Vec<HistoryPointView> = (0..300)
            .map(|second| {
                let value = if second == 150 { 95.0 } else { 10.0 };
                point(second, Some(value))
            })
            .collect();
        let merged = downsample(&points, 30);
        assert!(merged.len() <= 30);
        let peak = merged
            .iter()
            .filter_map(|point| point.max)
            .fold(f64::MIN, f64::max);
        assert_eq!(peak, 95.0);
        let spike_bucket = merged.iter().find(|point| point.max == Some(95.0)).unwrap();
        assert_eq!(spike_bucket.min, Some(10.0));
        assert!((spike_bucket.mean.unwrap() - 18.5).abs() < 1e-9);
    }

    #[test]
    fn gaps_and_discontinuities_are_never_bridged() {
        let mut points: Vec<HistoryPointView> =
            (0..10).map(|second| point(second, Some(1.0))).collect();
        points[4] = point(4, None);
        points.push(point(20, Some(2.0)));
        let merged = downsample(&points, 2);
        let shape: Vec<(u64, u64, Option<f64>)> = merged
            .iter()
            .map(|point| {
                (
                    point.start_utc_ns / SECOND,
                    point.end_utc_ns / SECOND,
                    point.mean,
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (0, 4, Some(1.0)),
                (4, 5, None),
                (5, 10, Some(1.0)),
                (20, 21, Some(2.0)),
            ]
        );
        assert!(downsample(&[], 10).is_empty());
    }
}
