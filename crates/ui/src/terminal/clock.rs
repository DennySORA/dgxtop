//! Monotonic loop clock (injectable so the runner is testable without real
//! time). It never touches the data clocks of the Manager.

use std::time::{Duration, Instant};

/// Monotonic time since an arbitrary origin.
pub trait LoopClock {
    fn now(&self) -> Duration;
}

/// Real monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopClock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}
