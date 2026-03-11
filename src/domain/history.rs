use std::collections::VecDeque;
use std::time::Instant;

/// A fixed-size ring buffer for time-series data used in sparklines and charts.
#[derive(Debug, Clone)]
pub struct RingBuffer {
    data: VecDeque<f64>,
    capacity: usize,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, value: f64) {
        if self.data.len() >= self.capacity {
            self.data.pop_front();
        }
        self.data.push_back(value);
    }

    pub fn as_slice_pair(&self) -> (&[f64], &[f64]) {
        self.data.as_slices()
    }

    /// Return a contiguous Vec for rendering (copies data).
    pub fn to_vec(&self) -> Vec<f64> {
        self.data.iter().copied().collect()
    }

    /// Return the most recent `max_points` values as u64 for ratatui Sparkline.
    /// Pass the sparkline widget width to avoid displaying stale leading data.
    pub fn to_sparkline_data(&self, max_points: usize) -> Vec<u64> {
        let skip = self.data.len().saturating_sub(max_points);
        self.data
            .iter()
            .skip(skip)
            .map(|v| v.round().max(0.0) as u64)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn last(&self) -> Option<f64> {
        self.data.back().copied()
    }

    pub fn average(&self) -> f64 {
        if self.data.is_empty() {
            return 0.0;
        }
        self.data.iter().sum::<f64>() / self.data.len() as f64
    }

    pub fn max_value(&self) -> f64 {
        self.data.iter().copied().fold(0.0_f64, f64::max)
    }

    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Return data as (x, y) pairs for ratatui Chart datasets.
    /// X values are sequential indices (0, 1, 2, ...).
    pub fn to_chart_data(&self) -> Vec<(f64, f64)> {
        self.data
            .iter()
            .enumerate()
            .map(|(i, &v)| (i as f64, v))
            .collect()
    }
}

// ── Time-Window Aggregation ──────────────────────────────────────────

const MINUTES_PER_24H: usize = 1440;
const SECONDS_PER_MINUTE: u64 = 60;

/// A single minute-resolution bucket for aggregating metric values.
#[derive(Debug, Clone)]
struct MinuteBucket {
    sum: f64,
    count: u32,
    max: f64,
}

impl MinuteBucket {
    fn new() -> Self {
        Self {
            sum: 0.0,
            count: 0,
            max: f64::NEG_INFINITY,
        }
    }

    fn push(&mut self, value: f64) {
        self.sum += value;
        self.count += 1;
        if value > self.max {
            self.max = value;
        }
    }

    #[cfg(test)]
    fn average(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.sum / self.count as f64
    }
}

/// Aggregates metric values into minute-resolution buckets for 24-hour statistics.
///
/// Supports querying average, max, and cumulative sum over 1h/6h/12h/24h windows.
#[derive(Debug, Clone)]
pub struct TimeWindowAggregator {
    buckets: VecDeque<MinuteBucket>,
    current_bucket: MinuteBucket,
    current_minute: u64,
    start_instant: Instant,
}

impl Default for TimeWindowAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeWindowAggregator {
    pub fn new() -> Self {
        Self {
            buckets: VecDeque::with_capacity(MINUTES_PER_24H),
            current_bucket: MinuteBucket::new(),
            current_minute: 0,
            start_instant: Instant::now(),
        }
    }

    /// Record a new metric value. Automatically rotates buckets when the minute changes.
    pub fn push(&mut self, value: f64) {
        let elapsed_secs = self.start_instant.elapsed().as_secs();
        let minute = elapsed_secs / SECONDS_PER_MINUTE;

        if minute > self.current_minute && self.current_bucket.count > 0 {
            // Finalize the current bucket and start a new one
            let finished = std::mem::replace(&mut self.current_bucket, MinuteBucket::new());
            if self.buckets.len() >= MINUTES_PER_24H {
                self.buckets.pop_front();
            }
            self.buckets.push_back(finished);
            self.current_minute = minute;
        } else if self.current_minute == 0 && minute == 0 {
            // First push, just record
        } else {
            self.current_minute = minute;
        }

        self.current_bucket.push(value);
    }

    /// Average value over the last N hours.
    pub fn average_over_hours(&self, hours: usize) -> f64 {
        let window = hours * 60;
        let (total_sum, total_count) = self.aggregate_window(window);
        if total_count == 0 {
            return 0.0;
        }
        total_sum / total_count as f64
    }

    /// Maximum value observed in the last N hours.
    pub fn max_over_hours(&self, hours: usize) -> f64 {
        let window = hours * 60;
        let max = self.max_in_window(window);
        if max == f64::NEG_INFINITY { 0.0 } else { max }
    }

    /// Cumulative sum over the last N hours.
    /// For rate metrics (bytes/sec), multiply by `collection_interval_secs` to get total bytes.
    pub fn sum_over_hours(&self, hours: usize) -> f64 {
        let window = hours * 60;
        let (total_sum, _) = self.aggregate_window(window);
        total_sum
    }

    /// Number of completed buckets available.
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Hours elapsed since this aggregator was created.
    pub fn elapsed_hours(&self) -> f64 {
        self.start_instant.elapsed().as_secs_f64() / 3600.0
    }

    fn aggregate_window(&self, window_minutes: usize) -> (f64, u32) {
        let mut total_sum = self.current_bucket.sum;
        let mut total_count = self.current_bucket.count;

        let take_count = window_minutes.min(self.buckets.len());
        for bucket in self.buckets.iter().rev().take(take_count) {
            total_sum += bucket.sum;
            total_count += bucket.count;
        }

        (total_sum, total_count)
    }

    fn max_in_window(&self, window_minutes: usize) -> f64 {
        let mut max = if self.current_bucket.count > 0 {
            self.current_bucket.max
        } else {
            f64::NEG_INFINITY
        };

        let take_count = window_minutes.min(self.buckets.len());
        for bucket in self.buckets.iter().rev().take(take_count) {
            if bucket.count > 0 && bucket.max > max {
                max = bucket.max;
            }
        }

        max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_within_capacity_stores_all_values() {
        let mut buf = RingBuffer::new(5);
        buf.push(1.0);
        buf.push(2.0);
        buf.push(3.0);
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.to_vec(), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn push_beyond_capacity_evicts_oldest() {
        let mut buf = RingBuffer::new(3);
        for i in 1..=5 {
            buf.push(i as f64);
        }
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.to_vec(), vec![3.0, 4.0, 5.0]);
    }

    #[test]
    fn average_returns_correct_mean() {
        let mut buf = RingBuffer::new(4);
        buf.push(10.0);
        buf.push(20.0);
        buf.push(30.0);
        buf.push(40.0);
        assert!((buf.average() - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_buffer_average_is_zero() {
        let buf = RingBuffer::new(10);
        assert!((buf.average() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn sparkline_data_rounds_correctly() {
        let mut buf = RingBuffer::new(3);
        buf.push(1.4);
        buf.push(2.6);
        buf.push(-0.5);
        assert_eq!(buf.to_sparkline_data(100), vec![1, 3, 0]);
    }

    #[test]
    fn chart_data_returns_indexed_pairs() {
        let mut buf = RingBuffer::new(3);
        buf.push(10.0);
        buf.push(20.0);
        buf.push(30.0);
        let chart = buf.to_chart_data();
        assert_eq!(chart, vec![(0.0, 10.0), (1.0, 20.0), (2.0, 30.0)]);
    }

    #[test]
    fn time_window_aggregator_push_and_average() {
        let mut agg = TimeWindowAggregator::new();
        agg.push(10.0);
        agg.push(20.0);
        agg.push(30.0);
        // All in current bucket (within same minute)
        let avg = agg.average_over_hours(1);
        assert!((avg - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn time_window_aggregator_max() {
        let mut agg = TimeWindowAggregator::new();
        agg.push(5.0);
        agg.push(99.0);
        agg.push(42.0);
        assert!((agg.max_over_hours(1) - 99.0).abs() < f64::EPSILON);
    }

    #[test]
    fn time_window_aggregator_sum() {
        let mut agg = TimeWindowAggregator::new();
        agg.push(100.0);
        agg.push(200.0);
        agg.push(300.0);
        assert!((agg.sum_over_hours(1) - 600.0).abs() < f64::EPSILON);
    }

    #[test]
    fn time_window_aggregator_empty_returns_zero() {
        let agg = TimeWindowAggregator::new();
        assert!((agg.average_over_hours(1) - 0.0).abs() < f64::EPSILON);
        assert!((agg.max_over_hours(1) - 0.0).abs() < f64::EPSILON);
        assert!((agg.sum_over_hours(24) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn minute_bucket_average() {
        let mut bucket = MinuteBucket::new();
        bucket.push(10.0);
        bucket.push(20.0);
        assert!((bucket.average() - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn minute_bucket_empty_average_is_zero() {
        let bucket = MinuteBucket::new();
        assert!((bucket.average() - 0.0).abs() < f64::EPSILON);
    }
}
