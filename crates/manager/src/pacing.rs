//! When a session's next projection is built.
//!
//! Three things ask for a frame, each paced on its own:
//! * **input** (a view update, a viewport, a configuration or action
//!   result): at most every `input_interval_ns` (50 ms in the normal state),
//!   so the view answers keys at once;
//! * **data** (a commit, a refreshed chart): at most once per frame
//!   interval of `viewer.fps`, which is all the viewer draws. The commits of
//!   one collection cycle arrive one collector at a time, so after the first
//!   the frame waits while the cycle is still being collected, for at most
//!   [`SETTLE_NS`], and shows them together;
//! * **age**: a frame at least every [`AGE_REFRESH_NS`] plus [`SETTLE_NS`],
//!   so ages advance when no data arrives; the slack keeps the refresh from
//!   landing just before a cycle's commits at the 1 s sampling default.
//!
//! A frame shows everything, so building one satisfies all three.

use dgxtop_core::clock::{NANOS_PER_MILLI, NANOS_PER_SECOND};

/// Longest wait for the rest of a cycle's commits after the first. It also
/// keeps the commit-to-projection latency within its budget (p95
/// ≤ 100 ms).
pub(crate) const SETTLE_NS: u64 = 50 * NANOS_PER_MILLI;
/// Ages in projections advance at least this often without data.
pub(crate) const AGE_REFRESH_NS: u64 = NANOS_PER_SECOND;

/// The pacing in force for one control iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FramePacing {
    /// Spacing of any two frames; what input waits for.
    pub(crate) input_interval_ns: u64,
    /// Spacing of data-driven frames: the viewer's frame interval.
    pub(crate) data_interval_ns: u64,
    /// More commits of the current cycle are still on their way.
    pub(crate) cycle_settling: bool,
}

/// What one session asks of the next frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FrameDemand {
    input: bool,
    data_since_ns: Option<u64>,
    last_frame_ns: Option<u64>,
    last_data_frame_ns: Option<u64>,
}

impl FrameDemand {
    /// A new session asks for its first frame at once.
    pub(crate) fn new_session() -> Self {
        Self {
            input: true,
            ..Self::default()
        }
    }

    pub(crate) fn on_input(&mut self) {
        self.input = true;
    }

    /// New data at `now_ns`; the first since the last frame starts the wait.
    pub(crate) fn on_data(&mut self, now_ns: u64) {
        self.data_since_ns.get_or_insert(now_ns);
    }

    /// Whether a frame has anything new to show beyond the ages.
    pub(crate) fn wants_content(&self) -> bool {
        self.input || self.data_since_ns.is_some()
    }

    /// A frame was published at `now_ns`: it shows everything asked so far.
    pub(crate) fn on_frame(&mut self, now_ns: u64) {
        self.on_attempt(now_ns);
        self.input = false;
        self.data_since_ns = None;
    }

    /// A frame was attempted and skipped (presentation budget): it still
    /// counts against the pacing so a full ledger is not retried in a storm,
    /// and what it would have shown stays asked for.
    pub(crate) fn on_skipped(&mut self, now_ns: u64) {
        self.on_attempt(now_ns);
    }

    fn on_attempt(&mut self, now_ns: u64) {
        if self.data_since_ns.is_some() {
            self.last_data_frame_ns = Some(now_ns);
        }
        self.last_frame_ns = Some(now_ns);
    }

    /// The earliest instant the next frame may be built.
    pub(crate) fn due_ns(&self, pacing: &FramePacing) -> u64 {
        // Pending content brings a frame soon anyway; an age refresh then
        // would only cut a settling cycle in two.
        if !self.wants_content() {
            return self.after_last_frame(AGE_REFRESH_NS.saturating_add(SETTLE_NS));
        }
        let spaced = self.after_last_frame(pacing.input_interval_ns);
        if self.input {
            return spaced;
        }
        let since = self.data_since_ns.unwrap_or(0);
        let paced = self
            .last_data_frame_ns
            .map_or(0, |last| last.saturating_add(pacing.data_interval_ns));
        let settled = if pacing.cycle_settling {
            since.saturating_add(SETTLE_NS)
        } else {
            since
        };
        paced.max(settled).max(spaced)
    }

    fn after_last_frame(&self, interval_ns: u64) -> u64 {
        self.last_frame_ns
            .map_or(0, |last| last.saturating_add(interval_ns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = NANOS_PER_MILLI;

    /// 4 fps in the normal state, a cycle fully committed.
    fn pacing(cycle_settling: bool) -> FramePacing {
        FramePacing {
            input_interval_ns: 50 * MS,
            data_interval_ns: 250 * MS,
            cycle_settling,
        }
    }

    fn after_frame_at(now_ns: u64) -> FrameDemand {
        let mut demand = FrameDemand::new_session();
        demand.on_data(now_ns);
        demand.on_frame(now_ns);
        demand
    }

    #[test]
    fn a_new_session_gets_its_first_frame_at_once() {
        assert_eq!(FrameDemand::new_session().due_ns(&pacing(true)), 0);
    }

    /// Six collectors commit within a few milliseconds: one frame, once
    /// the last has committed.
    #[test]
    fn the_commits_of_one_cycle_share_one_frame() {
        let mut demand = after_frame_at(0);
        let first_commit = 1_000 * MS;
        demand.on_data(first_commit);
        assert_eq!(
            demand.due_ns(&pacing(true)),
            first_commit + SETTLE_NS,
            "waits for the rest of the cycle"
        );
        for later in 1..6 {
            demand.on_data(first_commit + later * MS);
        }
        assert_eq!(
            demand.due_ns(&pacing(false)),
            first_commit,
            "every collector committed: due now"
        );
        demand.on_frame(first_commit + 5 * MS);
        assert!(!demand.wants_content());
    }

    /// A collector that never finishes cannot hold the frame back longer
    /// than the settle window.
    #[test]
    fn a_slow_cycle_waits_at_most_the_settle_window() {
        let mut demand = after_frame_at(0);
        demand.on_data(2_000 * MS);
        demand.on_data(2_040 * MS);
        assert_eq!(demand.due_ns(&pacing(true)), 2_000 * MS + SETTLE_NS);
    }

    /// Data frames follow the viewer's frame rate, never the commit rate.
    #[test]
    fn data_frames_are_paced_at_the_frame_interval() {
        let mut demand = after_frame_at(1_000 * MS);
        demand.on_data(1_010 * MS);
        assert_eq!(demand.due_ns(&pacing(false)), 1_250 * MS);
        let mut cadence = Vec::new();
        let mut now = 1_250 * MS;
        // A commit every 10 ms for a second gives four frames, not 100.
        while now < 2_250 * MS {
            demand.on_data(now);
            if demand.due_ns(&pacing(false)) <= now {
                demand.on_frame(now);
                cadence.push(now / MS);
            }
            now += 10 * MS;
        }
        assert_eq!(cadence, vec![1_250, 1_500, 1_750, 2_000]);
    }

    /// Input does not wait for the frame interval nor for a settling cycle.
    #[test]
    fn input_gets_a_frame_within_the_input_interval() {
        let mut demand = after_frame_at(1_000 * MS);
        demand.on_data(1_010 * MS);
        demand.on_input();
        assert_eq!(demand.due_ns(&pacing(true)), 1_050 * MS);
        demand.on_frame(1_050 * MS);
        demand.on_input();
        assert_eq!(demand.due_ns(&pacing(true)), 1_100 * MS);
    }

    /// A frame built for input does not hold back the next commit's frame.
    #[test]
    fn an_input_frame_does_not_delay_data() {
        let mut demand = after_frame_at(0);
        demand.on_input();
        demand.on_frame(1_000 * MS);
        demand.on_data(1_010 * MS);
        assert_eq!(demand.due_ns(&pacing(false)), 1_050 * MS);
    }

    /// A cycle that starts just before the age refresh is due keeps its
    /// one frame: the refresh does not cut in.
    #[test]
    fn the_age_refresh_waits_for_a_settling_cycle() {
        let mut demand = after_frame_at(0);
        demand.on_data(AGE_REFRESH_NS + 30 * MS);
        assert_eq!(
            demand.due_ns(&pacing(true)),
            AGE_REFRESH_NS + 30 * MS + SETTLE_NS
        );
    }

    #[test]
    fn ages_refresh_without_data() {
        let demand = after_frame_at(3_000 * MS);
        assert!(!demand.wants_content());
        assert_eq!(
            demand.due_ns(&pacing(false)),
            3_000 * MS + AGE_REFRESH_NS + SETTLE_NS
        );
    }

    /// A skipped frame keeps what it should have shown and still counts
    /// against the pacing.
    #[test]
    fn a_skipped_frame_is_retried_at_the_pace() {
        let mut demand = after_frame_at(0);
        demand.on_data(1_000 * MS);
        demand.on_skipped(1_000 * MS);
        assert!(demand.wants_content());
        assert_eq!(demand.due_ns(&pacing(false)), 1_250 * MS);
        let mut input = FrameDemand::new_session();
        input.on_skipped(500 * MS);
        assert_eq!(input.due_ns(&pacing(false)), 550 * MS);
    }

    #[test]
    fn far_deadlines_saturate() {
        let demand = after_frame_at(u64::MAX - 1);
        assert_eq!(demand.due_ns(&pacing(true)), u64::MAX);
    }
}
