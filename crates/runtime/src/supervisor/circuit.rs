//! Consecutive-failure circuit breaker with deterministic backoff.
//!
//! `failure_threshold` consecutive failures open the circuit; while it is
//! open the backend waits 1, 2, 4 … `max_backoff` seconds (±10 % jitter)
//! between half-open probes. The jitter seed is derived from the adapter id,
//! so a given backend always produces the same, testable schedule while
//! different backends do not restart in lock step.

use dgxtop_core::clock::{Backoff, splitmix64};

/// Failure accounting for one backend.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    threshold: u32,
    consecutive_failures: u32,
    backoff: Backoff,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, base_ms: u64, max_ms: u64, jitter_permille: u64, seed: u64) -> Self {
        Self {
            threshold: threshold.max(1),
            consecutive_failures: 0,
            backoff: Backoff::new(base_ms, max_ms, jitter_permille, seed),
        }
    }

    /// Counts one failure; returns `true` when the threshold is reached.
    pub fn record_failure(&mut self) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.is_tripped()
    }

    /// A successful on-time reply closes the circuit and resets the backoff.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.backoff.reset();
    }

    pub const fn is_tripped(&self) -> bool {
        self.consecutive_failures >= self.threshold
    }

    pub const fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Next open-circuit delay; advances the exponential schedule.
    pub fn next_delay_ms(&mut self) -> u64 {
        self.backoff.next_delay_ms()
    }
}

/// Stable per-adapter jitter seed (FNV-1a of the id, mixed by SplitMix64).
pub fn backoff_seed(adapter_id: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in adapter_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    splitmix64(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_the_threshold_and_resets_on_success() {
        let mut circuit = CircuitBreaker::new(3, 1000, 60_000, 100, 1);
        assert!(!circuit.record_failure());
        assert!(!circuit.record_failure());
        assert!(circuit.record_failure());
        assert!(circuit.is_tripped());
        assert_eq!(circuit.consecutive_failures(), 3);
        circuit.record_success();
        assert!(!circuit.is_tripped());
        assert_eq!(circuit.consecutive_failures(), 0);
    }

    #[test]
    fn schedule_doubles_to_the_cap_within_ten_percent() {
        let mut circuit = CircuitBreaker::new(3, 1000, 60_000, 100, backoff_seed("nvml"));
        for nominal in [1000u64, 2000, 4000, 8000, 16_000, 32_000, 60_000, 60_000] {
            let delay = circuit.next_delay_ms();
            assert!(
                delay >= nominal - nominal / 10 && delay <= nominal + nominal / 10,
                "{delay} outside {nominal} ± 10 %"
            );
        }
        circuit.record_success();
        let first = circuit.next_delay_ms();
        assert!((900..=1100).contains(&first));
    }

    #[test]
    fn the_seed_makes_the_schedule_deterministic_per_adapter() {
        let schedule = |adapter: &str| {
            let mut circuit = CircuitBreaker::new(1, 1000, 60_000, 100, backoff_seed(adapter));
            (0..8).map(|_| circuit.next_delay_ms()).collect::<Vec<_>>()
        };
        assert_eq!(schedule("nvml"), schedule("nvml"));
        assert_ne!(schedule("nvml"), schedule("procfs"));
        assert_eq!(backoff_seed("nvml"), backoff_seed("nvml"));
    }
}
