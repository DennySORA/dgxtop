//! One protected action and the bounded registry of actions, plus the
//! `ActionsView` projection (never containing the token).

use dgxtop_core::action::{
    ActionHandle, ActionState, ConfirmationToken, ExitObservation, PreparedProcess, Signal,
};
use dgxtop_core::clock::ns_to_ms;
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::identity::{ProcessKey, SessionId};
use dgxtop_core::view::{ActionRecordView, ConfirmationView};
use uuid::Uuid;

/// Non-terminal actions at once (RUNTIME.md §4).
pub(crate) const MAX_INFLIGHT_ACTIONS: usize = 8;
/// Terminal actions kept for the recent list.
pub(crate) const MAX_RECENT_ACTIONS: usize = 32;

/// Why tokens that had not reached `sending` were revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevokeReason {
    PolicyChanged,
    Shutdown,
    SessionClosed,
}

impl RevokeReason {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::PolicyChanged => "action policy changed; confirmation revoked",
            Self::Shutdown => "dgxtop is shutting down; confirmation revoked",
            Self::SessionClosed => "session closed; confirmation revoked",
        }
    }

    /// Code answered to a confirm/pending command of a revoked action.
    pub(crate) const fn code(self) -> ErrorCode {
        match self {
            Self::PolicyChanged | Self::SessionClosed => ErrorCode::CapabilityDenied,
            Self::Shutdown => ErrorCode::Unavailable,
        }
    }
}

/// The confirmation that consumed the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmContext {
    pub(crate) request_id: Uuid,
    pub(crate) payload_sha256: String,
    pub(crate) intent_id: Uuid,
    pub(crate) intent_deadline_ns: u64,
    /// The `AppendIntent` reached the audit worker.
    pub(crate) intent_dispatched: bool,
    /// The audit worker answered the intent (durable or not).
    pub(crate) intent_answered: bool,
    /// The ConfirmSignal command already has its result.
    pub(crate) answered: bool,
}

/// Non-blocking exit observation after a sent signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExitWatch {
    pub(crate) next_poll_ns: u64,
    pub(crate) until_ns: u64,
    pub(crate) outstanding_since_ns: Option<u64>,
    pub(crate) last: Option<ExitObservation>,
}

#[derive(Debug, Clone)]
pub(crate) struct ActionRecord {
    pub(crate) prepare_request: Uuid,
    /// `None` for intents recovered at startup.
    pub(crate) session_id: Option<SessionId>,
    pub(crate) session_digest: String,
    pub(crate) prepare_payload: String,
    pub(crate) target: ProcessKey,
    pub(crate) signal: Signal,
    pub(crate) state: ActionState,
    pub(crate) token: Option<ConfirmationToken>,
    pub(crate) expires_ns: u64,
    pub(crate) policy_revision: u32,
    pub(crate) prepared: Option<PreparedProcess>,
    pub(crate) handle_held: bool,
    pub(crate) release_wanted: bool,
    /// The PrepareSignal command is still waiting for the worker.
    pub(crate) prepare_deadline_ns: Option<u64>,
    pub(crate) confirm: Option<ConfirmContext>,
    /// Local deadline after which a missing `Sent` means "outcome unknown".
    pub(crate) send_deadline_ns: Option<u64>,
    pub(crate) exit: Option<ExitWatch>,
    pub(crate) revoked: Option<RevokeReason>,
    pub(crate) result: String,
    pub(crate) updated_ns: u64,
    /// Recovered at startup without outcome: displayed, never resent.
    pub(crate) recovered: bool,
}

impl ActionRecord {
    pub(crate) fn handle(&self) -> Option<ActionHandle> {
        self.prepared.as_ref().map(|prepared| prepared.handle)
    }

    pub(crate) fn confirm_request(&self) -> Option<Uuid> {
        self.confirm.as_ref().map(|confirm| confirm.request_id)
    }

    pub(crate) fn intent_id(&self) -> Option<Uuid> {
        self.confirm.as_ref().map(|confirm| confirm.intent_id)
    }

    /// A confirmation that has not yet reached `sending`.
    pub(crate) fn is_revocable(&self) -> bool {
        matches!(
            self.state,
            ActionState::Requested
                | ActionState::Prepared
                | ActionState::Confirming
                | ActionState::IntentPending
                | ActionState::IntentCommitted
        )
    }

    pub(crate) fn set_state(&mut self, state: ActionState, result: &str, now_ns: u64) {
        self.state = state;
        self.result = dgxtop_core::text::sanitize_message(result);
        self.updated_ns = now_ns;
    }

    pub(crate) fn recent_view(&self, now_ns: u64) -> ActionRecordView {
        ActionRecordView {
            request_id: self.prepare_request,
            target: self.target.summary(),
            signal: self.signal,
            state: self.state,
            result: self.result.clone(),
            age_ms: ns_to_ms(now_ns.saturating_sub(self.updated_ns)),
        }
    }

    pub(crate) fn confirmation_view(
        &self,
        now_ns: u64,
        warning: String,
    ) -> Option<ConfirmationView> {
        if self.state != ActionState::Prepared || now_ns >= self.expires_ns {
            return None;
        }
        let prepared = self.prepared.as_ref()?;
        Some(ConfirmationView {
            request_id: self.prepare_request,
            target: self.target.clone(),
            uid: prepared.verified.uid,
            basename: prepared.verified.basename.clone(),
            signal: self.signal,
            expires_in_ms: ns_to_ms(self.expires_ns.saturating_sub(now_ns)),
            warning,
        })
    }
}

/// Irreversibility warning shown with every confirmation.
pub(crate) fn irreversible_warning(signal: Signal, unknown_outcomes: u32) -> String {
    let mut warning = format!(
        "{} cannot be undone; the process may lose unsaved work.",
        signal.name()
    );
    if signal == Signal::Sigkill {
        warning.push_str(" SIGKILL gives the process no chance to clean up.");
    }
    if unknown_outcomes > 0 {
        warning.push_str(&format!(
            " {unknown_outcomes} earlier action(s) have unknown outcomes and were not retried."
        ));
    }
    warning
}

/// Bounded registry: ≤ 8 active actions plus ≤ 32 terminal ones.
#[derive(Debug, Default)]
pub(crate) struct ActionRegistry {
    records: Vec<ActionRecord>,
}

impl ActionRegistry {
    pub(crate) fn push(&mut self, record: ActionRecord) {
        self.records.push(record);
        self.prune();
    }

    pub(crate) fn get_mut(&mut self, index: usize) -> Option<&mut ActionRecord> {
        self.records.get_mut(index)
    }

    pub(crate) fn get(&self, index: usize) -> Option<&ActionRecord> {
        self.records.get(index)
    }

    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, ActionRecord> {
        self.records.iter()
    }

    pub(crate) fn by_token(&self, token: &ConfirmationToken) -> Option<usize> {
        self.records
            .iter()
            .position(|record| record.token.as_ref() == Some(token))
    }

    pub(crate) fn by_prepare(&self, request_id: Uuid) -> Option<usize> {
        self.records
            .iter()
            .position(|record| record.prepare_request == request_id)
    }

    pub(crate) fn by_confirm(&self, request_id: Uuid, intent_id: Option<Uuid>) -> Option<usize> {
        self.records.iter().position(|record| {
            record.confirm_request() == Some(request_id)
                && intent_id.is_none_or(|intent| record.intent_id() == Some(intent))
        })
    }

    pub(crate) fn by_handle(&self, handle: ActionHandle) -> Option<usize> {
        self.records
            .iter()
            .position(|record| record.handle() == Some(handle))
    }

    /// Actions not yet terminal (bounded by `MAX_INFLIGHT_ACTIONS`).
    pub(crate) fn active(&self) -> usize {
        self.records
            .iter()
            .filter(|record| !record.state.is_terminal() && !record.recovered)
            .count()
    }

    pub(crate) fn pending_confirmations(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.state == ActionState::Prepared)
            .count()
    }

    fn prune(&mut self) {
        let terminal = self
            .records
            .iter()
            .filter(|record| record.state.is_terminal() || record.recovered)
            .count();
        let mut excess = terminal.saturating_sub(MAX_RECENT_ACTIONS);
        // Oldest terminal records go first; holding a pidfd keeps a record.
        self.records.retain(|record| {
            let removable = (record.state.is_terminal() || record.recovered) && !record.handle_held;
            if excess > 0 && removable {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn tidy(&mut self) {
        self.prune();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::action::VerifiedIdentity;
    use dgxtop_core::identity::{Pid, PidNamespace};
    use dgxtop_core::testing::{boot_id, host_id};

    pub(crate) fn record(state: ActionState) -> ActionRecord {
        let target = ProcessKey {
            host_id: host_id("test-host"),
            boot_id: boot_id("fake-boot"),
            pid_namespace: PidNamespace::new("pid:[1]").unwrap(),
            pid: Pid::new(42).unwrap(),
            start_time_ticks: 7,
        };
        ActionRecord {
            prepare_request: Uuid::new_v4(),
            session_id: Some(SessionId::new("s1").unwrap()),
            session_digest: "d".into(),
            prepare_payload: "p".into(),
            target: target.clone(),
            signal: Signal::Sigterm,
            state,
            token: None,
            expires_ns: 1_000,
            policy_revision: 1,
            prepared: Some(PreparedProcess {
                handle: ActionHandle(1),
                verified: VerifiedIdentity {
                    key: target,
                    uid: 1000,
                    basename: "train.py".into(),
                },
                display_summary: "pid 42".into(),
            }),
            handle_held: false,
            release_wanted: false,
            prepare_deadline_ns: None,
            confirm: None,
            send_deadline_ns: None,
            exit: None,
            revoked: None,
            result: String::new(),
            updated_ns: 0,
            recovered: false,
        }
    }

    #[test]
    fn registry_keeps_bounded_terminal_history() {
        let mut registry = ActionRegistry::default();
        registry.push(record(ActionState::Prepared));
        for _ in 0..50 {
            registry.push(record(ActionState::ExitObserved));
        }
        assert_eq!(registry.len(), 1 + MAX_RECENT_ACTIONS);
        assert_eq!(registry.active(), 1);
        assert_eq!(registry.pending_confirmations(), 1);
        let mut held = record(ActionState::Cancelled);
        held.handle_held = true;
        registry.push(held);
        registry.tidy();
        assert!(registry.iter().any(|record| record.handle_held));
    }

    #[test]
    fn confirmation_view_hides_the_token_and_counts_down() {
        let mut prepared = record(ActionState::Prepared);
        prepared.token = Some(ConfirmationToken::new("ab".repeat(32)).unwrap());
        prepared.expires_ns = 5_000_000_000;
        let view = prepared
            .confirmation_view(1_000_000_000, irreversible_warning(Signal::Sigterm, 2))
            .unwrap();
        assert_eq!(view.expires_in_ms, 4_000);
        assert_eq!(view.uid, 1000);
        assert_eq!(view.basename, "train.py");
        assert!(view.warning.contains("cannot be undone"));
        assert!(view.warning.contains("2 earlier action(s)"));
        let serialized = serde_json::to_string(&view).unwrap();
        assert!(!serialized.contains(&"ab".repeat(32)));
        assert!(
            prepared
                .confirmation_view(6_000_000_000, String::new())
                .is_none()
        );
        assert!(irreversible_warning(Signal::Sigkill, 0).contains("no chance"));
    }
}
