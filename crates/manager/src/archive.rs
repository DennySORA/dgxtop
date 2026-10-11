//! Visible state of the optional history archive (STORAGE.md §1). A store
//! acknowledgement only says "memory committed, archive pending"; the fate
//! of each archive journal (committed or a gap) is reported afterwards and
//! surfaces as the archive's persistence status.

use dgxtop_core::action::{PersistenceKind, PersistenceState, PersistenceStatus};
use dgxtop_core::ports::ArchiveOutcome;
use dgxtop_core::text::sanitize_message;

#[derive(Debug, Default)]
pub(crate) struct ArchiveTracker {
    status: Option<PersistenceStatus>,
}

impl ArchiveTracker {
    /// Applies one journal outcome; returns whether the visible status changed.
    pub(crate) fn on_outcome(
        &mut self,
        revision: u64,
        outcome: &ArchiveOutcome,
        status: Option<PersistenceStatus>,
    ) -> bool {
        let next = match outcome {
            ArchiveOutcome::Committed { .. } => status,
            ArchiveOutcome::Gap { detail, .. } => {
                // A busy worker reports no status of its own.
                Some(status.unwrap_or_else(|| gap_status(revision, detail)))
            }
        };
        let Some(next) = next else {
            return false;
        };
        let changed = self.status.as_ref() != Some(&next);
        self.status = Some(next);
        changed
    }

    /// `None` until the archive reported anything (also when it is disabled).
    pub(crate) fn status(&self) -> Option<&PersistenceStatus> {
        self.status.as_ref()
    }
}

fn gap_status(revision: u64, detail: &str) -> PersistenceStatus {
    PersistenceStatus {
        state: PersistenceState::Degraded,
        detail: Some(sanitize_message(&format!(
            "revision {revision} not archived: {detail}"
        ))),
        ..PersistenceStatus::healthy(PersistenceKind::Archive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::ports::ArchiveGapCause;

    fn failed(detail: &str) -> PersistenceStatus {
        PersistenceStatus {
            state: PersistenceState::Failed,
            detail: Some(detail.to_owned()),
            ..PersistenceStatus::healthy(PersistenceKind::Archive)
        }
    }

    #[test]
    fn nothing_is_shown_before_the_archive_reports() {
        let tracker = ArchiveTracker::default();
        assert!(tracker.status().is_none());
    }

    #[test]
    fn committed_journals_report_the_archive_status_without_gaps() {
        let mut tracker = ArchiveTracker::default();
        let healthy = PersistenceStatus::healthy(PersistenceKind::Archive);
        assert!(tracker.on_outcome(
            5,
            &ArchiveOutcome::Committed { rows: 12 },
            Some(healthy.clone())
        ));
        // An unchanged status needs no redraw.
        assert!(!tracker.on_outcome(6, &ArchiveOutcome::Committed { rows: 1 }, Some(healthy)));
        assert_eq!(tracker.status().unwrap().state, PersistenceState::Healthy);
    }

    #[test]
    fn gaps_are_made_visible() {
        let mut tracker = ArchiveTracker::default();
        let busy = ArchiveOutcome::Gap {
            cause: ArchiveGapCause::WorkerBusy,
            detail: "archive worker busy".to_owned(),
        };
        assert!(tracker.on_outcome(7, &busy, None));
        let status = tracker.status().unwrap();
        assert_eq!(status.kind, PersistenceKind::Archive);
        assert_eq!(status.state, PersistenceState::Degraded);
        assert!(status.detail.as_deref().unwrap().contains("revision 7"));
        let write_failed = ArchiveOutcome::Gap {
            cause: ArchiveGapCause::WriteFailed,
            detail: "disk low".to_owned(),
        };
        assert!(tracker.on_outcome(8, &write_failed, Some(failed("free disk below 64 MiB"))));
        assert_eq!(tracker.status().unwrap().state, PersistenceState::Failed);
    }
}
