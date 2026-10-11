//! Accounting of published `Arc` allocations by liveness.
//!
//! The store keeps only the latest snapshot, but a slow reader may still
//! hold an older one. Each published allocation is tracked through a `Weak`
//! next to the permit reserved before it was built; the permit is released
//! only once no reader holds the allocation any more, so retired snapshots
//! stay accounted for exactly as long as they are alive.

use std::any::Any;
use std::sync::{Arc, Weak};

use dgxtop_core::budget::BytePermit;

struct Tracked {
    allocation: Weak<dyn Any + Send + Sync>,
    _permit: BytePermit,
}

/// Permits of published allocations, released when the last `Arc` drops.
#[derive(Default)]
pub(crate) struct RetainedAllocations {
    tracked: Vec<Tracked>,
}

impl RetainedAllocations {
    pub(crate) fn track<T: Any + Send + Sync>(&mut self, allocation: &Arc<T>, permit: BytePermit) {
        let weak: Weak<T> = Arc::downgrade(allocation);
        let allocation: Weak<dyn Any + Send + Sync> = weak;
        self.tracked.push(Tracked {
            allocation,
            _permit: permit,
        });
    }

    /// Releases the permits of allocations nobody holds any more.
    pub(crate) fn sweep(&mut self) -> usize {
        let before = self.tracked.len();
        self.tracked
            .retain(|tracked| tracked.allocation.strong_count() > 0);
        before - self.tracked.len()
    }

    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.tracked.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::budget::{ByteLedger, Partition};

    #[test]
    fn permit_lives_as_long_as_any_reader() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut retained = RetainedAllocations::default();
        let snapshot = Arc::new(vec![1u8; 16]);
        retained.track(
            &snapshot,
            ledger.try_reserve(Partition::HotState, 100).unwrap(),
        );
        let reader = Arc::clone(&snapshot);
        drop(snapshot);
        assert_eq!(retained.sweep(), 0);
        assert_eq!(ledger.used(Partition::HotState), 100);
        drop(reader);
        assert_eq!(retained.sweep(), 1);
        assert_eq!(ledger.used(Partition::HotState), 0);
        assert_eq!(retained.live(), 0);
    }
}
