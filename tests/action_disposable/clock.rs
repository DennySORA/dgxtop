//! The stack's clock: real `CLOCK_BOOTTIME` (and the real boot id) that a
//! test can move forward, or freeze so that only the test moves it. TTL,
//! deadline and exit-watch scenarios then need no long real sleeps and do
//! not depend on scheduler timing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::BootId;
use dgxtop_runtime::clock::BootClock;

/// Marker of `frozen_at_ns`: the clock follows real time.
const FOLLOWING: u64 = 0;

#[derive(Debug)]
pub struct TestClock {
    real: BootClock,
    offset_ns: AtomicU64,
    /// Real boot time at which the clock stopped following real time.
    frozen_at_ns: AtomicU64,
}

impl TestClock {
    pub fn following_real_time() -> Arc<Self> {
        Arc::new(Self {
            real: BootClock::new().expect("CLOCK_BOOTTIME and boot_id are readable"),
            offset_ns: AtomicU64::new(0),
            frozen_at_ns: AtomicU64::new(FOLLOWING),
        })
    }

    pub fn advance(&self, by: Duration) {
        let nanos = u64::try_from(by.as_nanos()).expect("shift fits in u64 ns");
        self.offset_ns.fetch_add(nanos, Ordering::SeqCst);
    }

    /// Moves the clock to `target_boot_ns` (never backwards).
    pub fn advance_to(&self, target_boot_ns: u64) {
        let now = self.now_boot_ns();
        if target_boot_ns > now {
            self.offset_ns
                .fetch_add(target_boot_ns - now, Ordering::SeqCst);
        }
    }

    fn offset(&self) -> u64 {
        self.offset_ns.load(Ordering::SeqCst)
    }
}

impl TimeProvider for TestClock {
    fn boot_id(&self) -> BootId {
        self.real.boot_id()
    }

    fn now_boot_ns(&self) -> u64 {
        let frozen = self.frozen_at_ns.load(Ordering::SeqCst);
        let base = if frozen == FOLLOWING {
            self.real.now_boot_ns()
        } else {
            frozen
        };
        base.saturating_add(self.offset())
    }

    fn now_utc_ns(&self) -> i128 {
        // Wall time only stamps audit rows; it moves with the shifts so rows
        // stay in boot-clock order.
        self.real.now_utc_ns() + i128::from(self.offset())
    }
}
