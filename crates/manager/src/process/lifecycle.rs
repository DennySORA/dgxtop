//! Timers and descriptor lifecycle of the ProcessService: deadline and TTL
//! expiry, unknown send outcomes, exit polling, revocation and pidfd release.

use super::*;

impl ProcessService {
    pub(super) fn tick_record(&mut self, index: usize, now: Now, fx: &mut Effects) {
        let Some(record) = self.records.get(index) else {
            return;
        };
        match record.state {
            ActionState::Requested
                if record
                    .prepare_deadline_ns
                    .is_some_and(|deadline| now.boot_ns >= deadline) =>
            {
                self.reject_prepare(
                    index,
                    &ErrorDetail::new(
                        ErrorCode::TimedOut,
                        "action worker did not verify the target in time",
                    ),
                    now,
                    fx,
                );
                fx.mark_all_dirty();
            }
            ActionState::Prepared if now.boot_ns >= record.expires_ns => {
                self.end_unconfirmed(index, ActionState::Expired, "confirmation expired", now);
                fx.mark_all_dirty();
            }
            ActionState::IntentPending
                if record
                    .confirm
                    .as_ref()
                    .is_some_and(|confirm| now.boot_ns >= confirm.intent_deadline_ns) =>
            {
                let denial = Denial {
                    code: ErrorCode::TimedOut,
                    reason: "audit intent not durable before the deadline; signal not sent"
                        .to_owned(),
                };
                self.finish_not_sent(index, &denial, ActionState::NotSent, now, fx);
                fx.mark_all_dirty();
            }
            ActionState::Sending
                if record
                    .send_deadline_ns
                    .is_some_and(|deadline| now.boot_ns >= deadline) =>
            {
                self.mark_unknown(index, "no send report from the action worker", now, fx);
                fx.mark_all_dirty();
            }
            ActionState::SignalSent => self.poll_exit(index, now, fx),
            _ => {}
        }
    }
    pub(super) fn poll_exit(&mut self, index: usize, now: Now, fx: &mut Effects) {
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        let (Some(mut watch), Some(handle), Some(request_id)) =
            (record.exit, record.handle(), record.confirm_request())
        else {
            return;
        };
        if let Some(since) = watch.outstanding_since_ns {
            if now.boot_ns.saturating_sub(since) < EXIT_POLL_STUCK_NS {
                return;
            }
            watch.outstanding_since_ns = None;
        }
        if now.boot_ns >= watch.until_ns {
            record.exit = Some(watch);
            self.conclude_watch(index, "process still running after 10 s", now);
            fx.mark_all_dirty();
            return;
        }
        if now.boot_ns < watch.next_poll_ns {
            record.exit = Some(watch);
            return;
        }
        match self
            .actions
            .try_dispatch(ActionOp::ObserveExit { request_id, handle })
        {
            Ok(()) => watch.outstanding_since_ns = Some(now.boot_ns),
            Err(_) => watch.next_poll_ns = now.boot_ns.saturating_add(EXIT_POLL_NS),
        }
        if let Some(record) = self.records.get_mut(index) {
            record.exit = Some(watch);
        }
    }
    /// Stops watching for exit: `still_running` (exit not observed).
    pub(super) fn conclude_watch(&mut self, index: usize, reason: &str, now: Now) {
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        let unobservable = record
            .exit
            .and_then(|watch| watch.last)
            .is_some_and(|last| last == ExitObservation::Unknown);
        let text = if unobservable {
            "exit could not be observed".to_owned()
        } else {
            reason.to_owned()
        };
        record.exit = None;
        record.set_state(ActionState::StillRunning, &text, now.boot_ns);
        self.release(index);
        self.append_outcome(index, ActionState::StillRunning, "Ok", &text, now);
    }
    pub(super) fn mark_unknown(&mut self, index: usize, reason: &str, now: Now, fx: &mut Effects) {
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        record.send_deadline_ns = None;
        record.set_state(ActionState::Unknown, reason, now.boot_ns);
        let pending_confirm = record
            .confirm
            .as_mut()
            .filter(|confirm| !confirm.answered)
            .map(|confirm| {
                confirm.answered = true;
                confirm.request_id
            });
        if let Some(confirm_request) = pending_confirm {
            fx.reply(
                CommandResult::unknown(
                    confirm_request,
                    &format!("{reason}; signal outcome unknown and never retried"),
                )
                .with_action_state(ActionState::Unknown),
            );
        }
        // The worker serializes operations: this release closes the
        // descriptor once a stuck send returns.
        self.release(index);
        self.append_outcome(
            index,
            ActionState::Unknown,
            ErrorCode::UnknownOutcome.as_str(),
            reason,
            now,
        );
    }
    pub(super) fn revoke(
        &mut self,
        reason: RevokeReason,
        session: Option<&SessionId>,
        now: Now,
        fx: &mut Effects,
    ) -> u32 {
        let targets: Vec<usize> = (0..self.records.len())
            .filter(|index| {
                self.records.get(*index).is_some_and(|record| {
                    record.is_revocable()
                        && session.is_none_or(|id| record.session_id.as_ref() == Some(id))
                })
            })
            .collect();
        let mut revoked = 0u32;
        for index in targets {
            let Some(record) = self.records.get_mut(index) else {
                continue;
            };
            record.revoked = Some(reason);
            revoked = revoked.saturating_add(1);
            // A prepare still in flight is answered when the worker replies
            // (or by the final shutdown pass); its descriptor is then released.
            if record.state == ActionState::Requested {
                continue;
            }
            let denial = Denial {
                code: reason.code(),
                reason: reason.message().to_owned(),
            };
            let confirmed = record
                .confirm
                .as_ref()
                .is_some_and(|confirm| confirm.intent_dispatched);
            if confirmed {
                self.finish_not_sent(index, &denial, ActionState::Rejected, now, fx);
            } else {
                self.end_unconfirmed(index, ActionState::Rejected, reason.message(), now);
            }
        }
        if revoked > 0 {
            fx.mark_all_dirty();
        }
        revoked
    }
    pub(super) fn release(&mut self, index: usize) {
        if let Some(record) = self.records.get_mut(index) {
            record.release_wanted = true;
        }
        self.try_release(index);
    }
    pub(super) fn try_release(&mut self, index: usize) {
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        if !record.release_wanted || !record.handle_held {
            return;
        }
        let Some(handle) = record.handle() else {
            return;
        };
        if self
            .actions
            .try_dispatch(ActionOp::Release { handle })
            .is_ok()
        {
            record.handle_held = false;
            record.release_wanted = false;
        }
    }
    pub(super) fn release_untracked(&mut self, handle: ActionHandle) {
        // Best effort: a release refused for a full queue is dropped.
        let _ = self.actions.try_dispatch(ActionOp::Release { handle });
    }
}
