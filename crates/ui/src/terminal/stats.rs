//! Frame timing for diagnostics and the benchmark harness. Compute time
//! (layout + module render + Ratatui diff/encode into memory) and flush time
//! (frame enqueued → last byte accepted by the terminal) are kept apart, as
//! `PERFORMANCE.md` requires; percentiles use nearest rank.

use std::collections::VecDeque;
use std::time::Duration;

/// Samples kept per series (bounded ring).
pub const STATS_SAMPLES: usize = 1024;

/// Bounded ring of durations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DurationSamples {
    samples: VecDeque<Duration>,
}

impl DurationSamples {
    pub fn push(&mut self, sample: Duration) {
        if self.samples.len() == STATS_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Nearest-rank percentile in permille (`950` = p95): the sample at
    /// `ceil(p × N) − 1` of the sorted samples.
    pub fn percentile_permille(&self, permille: u32) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<Duration> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let count = sorted.len();
        let permille = usize::try_from(permille.min(1000)).unwrap_or(1000);
        let rank = (permille * count).div_ceil(1000).max(1);
        sorted.get(rank - 1).copied()
    }

    pub fn p50(&self) -> Option<Duration> {
        self.percentile_permille(500)
    }

    pub fn p95(&self) -> Option<Duration> {
        self.percentile_permille(950)
    }

    pub fn p99(&self) -> Option<Duration> {
        self.percentile_permille(990)
    }

    pub fn max(&self) -> Option<Duration> {
        self.samples.iter().copied().max()
    }
}

/// Counters and timings of the terminal runner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub compute: DurationSamples,
    pub flush: DurationSamples,
    pub frames_drawn: u64,
    pub full_redraws: u64,
    pub overflowed_frames: u64,
    pub draw_errors: u64,
    /// Frames whose flush exceeded the completion bound.
    pub slow_frames: u64,
    /// Steps where a draw was due but the previous frame was still flushing.
    pub deferred_while_flushing: u64,
    pub peak_output_bytes: usize,
    pub bytes_written: u64,
}

/// Snapshot of [`FrameStats`] for reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameStatsSummary {
    pub frames_drawn: u64,
    pub compute_p50: Option<Duration>,
    pub compute_p95: Option<Duration>,
    pub compute_p99: Option<Duration>,
    pub flush_p50: Option<Duration>,
    pub flush_p95: Option<Duration>,
    pub flush_p99: Option<Duration>,
    pub full_redraws: u64,
    pub overflowed_frames: u64,
    pub draw_errors: u64,
    pub slow_frames: u64,
    pub deferred_while_flushing: u64,
    pub peak_output_bytes: usize,
    pub bytes_written: u64,
}

impl FrameStats {
    pub fn summary(&self) -> FrameStatsSummary {
        FrameStatsSummary {
            frames_drawn: self.frames_drawn,
            compute_p50: self.compute.p50(),
            compute_p95: self.compute.p95(),
            compute_p99: self.compute.p99(),
            flush_p50: self.flush.p50(),
            flush_p95: self.flush.p95(),
            flush_p99: self.flush.p99(),
            full_redraws: self.full_redraws,
            overflowed_frames: self.overflowed_frames,
            draw_errors: self.draw_errors,
            slow_frames: self.slow_frames,
            deferred_while_flushing: self.deferred_while_flushing,
            peak_output_bytes: self.peak_output_bytes,
            bytes_written: self.bytes_written,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn nearest_rank_percentiles() {
        let mut samples = DurationSamples::default();
        assert_eq!(samples.p95(), None);
        for value in 1..=100 {
            samples.push(ms(value));
        }
        assert_eq!(samples.p50(), Some(ms(50)));
        assert_eq!(samples.p95(), Some(ms(95)));
        assert_eq!(samples.p99(), Some(ms(99)));
        assert_eq!(samples.percentile_permille(1000), Some(ms(100)));
        assert_eq!(samples.percentile_permille(0), Some(ms(1)));
        assert_eq!(samples.max(), Some(ms(100)));
        let mut single = DurationSamples::default();
        single.push(ms(7));
        assert_eq!(single.p99(), Some(ms(7)));
    }

    #[test]
    fn samples_are_bounded() {
        let mut samples = DurationSamples::default();
        for value in 0..(u64::try_from(STATS_SAMPLES).unwrap() + 10) {
            samples.push(ms(value));
        }
        assert_eq!(samples.len(), STATS_SAMPLES);
        assert_eq!(samples.percentile_permille(0), Some(ms(10)));
    }
}
