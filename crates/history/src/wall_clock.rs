//! Boot clock to UTC mapping, for archive rows and clock discontinuities.
//!
//! UTC is only an index for display and archive rows; it never produces a
//! duration. A step of the wall clock relative to the boot clock starts a new
//! clock segment and is reported as a discontinuity.

use std::collections::VecDeque;

/// Offset change treated as a wall clock step rather than slewing.
const STEP_TOLERANCE_NS: u128 = 1_000_000_000;
/// Clock segments remembered.
const MAX_ANCHORS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anchor {
    boot_ns: u64,
    /// `utc_ns - boot_ns` while this segment lasts.
    offset_ns: i128,
}

/// Bounded list of clock segments.
#[derive(Debug, Default)]
pub(crate) struct WallClock {
    anchors: VecDeque<Anchor>,
}

impl WallClock {
    /// Records one `(boot, utc)` observation; returns the boot time of a
    /// discontinuity when the wall clock stepped.
    pub(crate) fn observe(&mut self, boot_ns: u64, utc_ns: i128) -> Option<u64> {
        let offset_ns = utc_ns.saturating_sub(i128::from(boot_ns));
        let Some(current) = self.anchors.back_mut() else {
            self.anchors.push_back(Anchor { boot_ns, offset_ns });
            return None;
        };
        if offset_ns.abs_diff(current.offset_ns) <= STEP_TOLERANCE_NS {
            // Slewing: follow it without cutting a segment.
            current.offset_ns = offset_ns;
            return None;
        }
        if self.anchors.len() >= MAX_ANCHORS {
            self.anchors.pop_front();
        }
        self.anchors.push_back(Anchor { boot_ns, offset_ns });
        Some(boot_ns)
    }

    pub(crate) fn is_known(&self) -> bool {
        !self.anchors.is_empty()
    }

    /// UTC nanoseconds of a boot time, using the clock segment in effect
    /// (the earliest segment for times before every observation).
    pub(crate) fn utc_of(&self, boot_ns: u64) -> Option<i128> {
        let anchor = self
            .anchors
            .iter()
            .rev()
            .find(|anchor| anchor.boot_ns <= boot_ns)
            .or_else(|| self.anchors.front())?;
        Some(i128::from(boot_ns).saturating_add(anchor.offset_ns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_cut_segments_and_slewing_does_not() {
        let mut clock = WallClock::default();
        assert!(!clock.is_known());
        assert_eq!(clock.observe(1_000, 5_000), None);
        assert_eq!(clock.observe(2_000, 6_100), None);
        assert_eq!(clock.utc_of(2_000), Some(6_100));
        let step_back = -86_400_000_000_000i128;
        assert_eq!(clock.observe(3_000, 7_000 + step_back), Some(3_000));
        assert_eq!(clock.utc_of(2_500), Some(2_500 + 4_100));
        assert_eq!(clock.utc_of(3_500), Some(3_500 + 4_000 + step_back));
        assert_eq!(clock.utc_of(10), Some(10 + 4_100));
    }
}
