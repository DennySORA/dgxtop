//! Side effects produced by services while handling one input.
//!
//! Services never deliver results, write events or mark sessions directly:
//! they describe the effects and the control loop applies them in one place
//! (single responsibility, and a natural seam for tests).

use dgxtop_core::canonical::GapRecord;
use dgxtop_core::commands::CommandResult;
use dgxtop_core::event::EventKind;
use uuid::Uuid;

#[derive(Debug, Default)]
pub(crate) struct Effects {
    /// Final command results, routed by `request_id`.
    pub(crate) results: Vec<CommandResult>,
    /// Events with their correlation id.
    pub(crate) events: Vec<(Uuid, EventKind)>,
    /// Gaps to record in the store.
    pub(crate) gaps: Vec<GapRecord>,
    /// Every session needs a new projection.
    pub(crate) dirty_all: bool,
}

impl Effects {
    pub(crate) fn reply(&mut self, result: CommandResult) {
        self.results.push(result);
    }

    pub(crate) fn event(&mut self, correlation_id: Uuid, kind: EventKind) {
        self.events.push((correlation_id, kind));
    }

    pub(crate) fn mark_all_dirty(&mut self) {
        self.dirty_all = true;
    }
}
