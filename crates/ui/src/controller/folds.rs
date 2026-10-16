//! The panels folded away (D-114). A key folds or unfolds a panel on the
//! next frame; the whole set is then saved in the configuration through
//! the Manager (`SetFolded`), one command at a time, a busy or refused
//! save tried again, so the next start shows what was folded last. Until
//! the user folds anything the saved set is followed.

use std::collections::BTreeSet;
use std::time::Duration;

use dgxtop_core::commands::{CommandResult, CommandStatus};
use dgxtop_core::errors::{ErrorCode, ResultCode};
use dgxtop_core::view::FOLDABLE_PANELS;

/// Wait before a save the Manager was too busy for is sent again.
const RETRY_AFTER: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Folds {
    /// What the frames show.
    shown: BTreeSet<String>,
    /// The user folded or unfolded a panel since the start.
    touched: bool,
    /// What the Manager saved, as far as is known.
    saved: Option<BTreeSet<String>>,
    /// The save on its way: its request and set.
    sending: Option<(u128, BTreeSet<String>)>,
    /// When a busy save goes out again.
    retry_at: Option<Duration>,
}

impl Folds {
    pub(super) fn shown(&self) -> &BTreeSet<String> {
        &self.shown
    }

    /// A projection's saved set: shown until the user folds anything.
    pub(super) fn adopt(&mut self, saved: &[String]) {
        let saved: BTreeSet<String> = saved.iter().cloned().collect();
        if !self.touched {
            self.shown.clone_from(&saved);
        }
        if self.sending.is_none() {
            self.saved = Some(saved);
        }
    }

    /// Folds `panel`, or unfolds it when folded.
    pub(super) fn toggle(&mut self, panel: &str) {
        self.touched = true;
        if !self.shown.remove(panel) {
            self.shown.insert(panel.to_owned());
        }
    }

    /// Unfolds the folded panels `of` takes; whether any was.
    pub(super) fn unfold(&mut self, of: impl Fn(&str) -> bool) -> bool {
        let before = self.shown.len();
        self.shown.retain(|panel| !of(panel));
        let unfolded = self.shown.len() != before;
        self.touched |= unfolded;
        unfolded
    }

    /// The panels to save now, in their canonical order: none while a save
    /// is on its way or waits to be tried again, or once saved.
    pub(super) fn due(&self, now: Duration) -> Option<Vec<String>> {
        if !self.touched
            || self.sending.is_some()
            || self.retry_at.is_some_and(|at| now < at)
            || self.saved.as_ref() == Some(&self.shown)
        {
            return None;
        }
        Some(
            FOLDABLE_PANELS
                .iter()
                .filter(|panel| self.shown.contains(**panel))
                .map(|panel| (*panel).to_owned())
                .collect(),
        )
    }

    /// The save of `panels` left as request `request_id`.
    pub(super) fn sent(&mut self, request_id: u128, panels: Vec<String>) {
        self.sending = Some((request_id, panels.into_iter().collect()));
        self.retry_at = None;
    }

    /// The save could not even be queued: tried again later.
    pub(super) fn refused(&mut self, now: Duration) {
        self.retry_at = Some(now + RETRY_AFTER);
    }

    /// A result of request `request_id`: whether it was a save, and why it
    /// failed for good (said once; the set stays as shown).
    pub(super) fn answered(
        &mut self,
        request_id: u128,
        result: &CommandResult,
        now: Duration,
    ) -> Option<Option<String>> {
        let (_, panels) = self
            .sending
            .take_if(|(sending, _)| *sending == request_id)?;
        match (result.status, result.code) {
            (CommandStatus::Completed, _) => {
                self.saved = Some(panels);
                Some(None)
            }
            (_, ResultCode::Error(ErrorCode::Busy)) => {
                self.retry_at = Some(now + RETRY_AFTER);
                Some(None)
            }
            _ => {
                // Saving again would fail again: the folds stay this run.
                self.saved = Some(self.shown.clone());
                Some(Some(format!(
                    "folded panels not saved: {}",
                    crate::format::clean(&result.message)
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::TEST_REQUEST_ID;

    const NOW: Duration = Duration::from_secs(1);

    fn ok() -> CommandResult {
        CommandResult::completed(TEST_REQUEST_ID.try_into().unwrap(), "saved")
    }

    fn rejected(code: ErrorCode) -> CommandResult {
        CommandResult::rejected(TEST_REQUEST_ID.try_into().unwrap(), code, "no")
    }

    /// The request id of the results.
    fn id() -> u128 {
        ok().request_id.as_u128()
    }

    #[test]
    fn the_saved_set_shows_until_the_user_folds_and_then_is_saved_once() {
        let mut folds = Folds::default();
        folds.adopt(&["history.disk".to_owned()]);
        assert!(folds.shown().contains("history.disk"));
        assert_eq!(folds.due(NOW), None, "nothing to save");
        folds.toggle("disk");
        folds.toggle("history.disk");
        let due = folds.due(NOW).unwrap();
        assert_eq!(due, ["disk"]);
        folds.sent(id(), due);
        assert_eq!(folds.due(NOW), None, "one save at a time");
        folds.adopt(&[]);
        assert!(folds.shown().contains("disk"), "the user's folds stay");
        assert_eq!(
            folds.answered(id() + 1, &ok(), NOW),
            None,
            "another request"
        );
        assert_eq!(folds.answered(id(), &ok(), NOW), Some(None));
        assert_eq!(folds.due(NOW), None, "saved");
    }

    #[test]
    fn a_busy_save_is_tried_again_and_a_refused_one_is_said_once() {
        let mut folds = Folds::default();
        folds.toggle("network");
        folds.sent(id(), folds.due(NOW).unwrap());
        assert_eq!(
            folds.answered(id(), &rejected(ErrorCode::Busy), NOW),
            Some(None)
        );
        assert_eq!(folds.due(NOW), None, "waits a moment");
        assert_eq!(folds.due(NOW + RETRY_AFTER).unwrap(), ["network"]);
        folds.sent(id(), vec!["network".to_owned()]);
        let refused = rejected(ErrorCode::ValidationFailed);
        assert!(folds.answered(id(), &refused, NOW).unwrap().is_some());
        assert_eq!(folds.due(NOW + RETRY_AFTER), None, "not again");
        assert!(folds.unfold(|panel| !panel.starts_with("history.")));
        assert!(folds.shown().is_empty());
        assert!(!folds.unfold(|_| true));
    }
}
