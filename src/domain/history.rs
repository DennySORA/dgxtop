use std::collections::VecDeque;

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

    /// Return data as u64 values suitable for ratatui Sparkline.
    pub fn to_sparkline_data(&self) -> Vec<u64> {
        self.data
            .iter()
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
        assert_eq!(buf.to_sparkline_data(), vec![1, 3, 0]);
    }
}
