//! Side effects produced by services while handling one input.
//!
//! Services never deliver results, record gaps or mark sessions directly:
//! they describe the effects and the control loop applies them in one place
//! (single responsibility, and a natural seam for tests).

use dgxtop_core::canonical::GapRecord;
use dgxtop_core::commands::CommandResult;

#[derive(Debug, Default)]
pub(crate) struct Effects {
    /// Final command results, routed by `request_id`.
    pub(crate) results: Vec<CommandResult>,
    /// Gaps to record in the store.
    pub(crate) gaps: Vec<GapRecord>,
    /// Every session needs a new projection.
    pub(crate) dirty_all: bool,
    /// A process dgxtop signaled exited: the process table is read again
    /// now, not at its next slot (D-109).
    pub(crate) reread_processes: bool,
}

impl Effects {
    pub(crate) fn reply(&mut self, result: CommandResult) {
        self.results.push(result);
    }

    pub(crate) fn mark_all_dirty(&mut self) {
        self.dirty_all = true;
    }

    pub(crate) fn reread_processes(&mut self) {
        self.reread_processes = true;
    }
}
