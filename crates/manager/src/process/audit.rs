//! Audit plumbing of the ProcessService: recovery of intents without
//! outcome, audit health, attempt records (best effort) and outcome records
//! (never dropped silently: an ordered, bounded backlog absorbs a busy
//! worker).

use super::*;

/// Facts of one attempt record.
#[derive(Debug, Clone, Copy)]
pub(super) struct AttemptFacts<'a> {
    pub(super) request_id: Uuid,
    pub(super) payload_sha256: &'a str,
    pub(super) session_digest: &'a str,
    pub(super) code: &'a str,
    pub(super) detail: &'a str,
}

impl ProcessService {
    pub(super) fn on_recovered(
        &mut self,
        result: Result<Vec<RecoveredIntent>, AuditFailure>,
        now: Now,
        fx: &mut Effects,
    ) {
        let intents = match result {
            Ok(intents) => intents,
            Err(failure) => {
                self.recovery_requested = false;
                self.note_audit_failure(&failure, now);
                return;
            }
        };
        self.recovered = true;
        let count = u32::try_from(intents.len()).unwrap_or(u32::MAX);
        self.unknown_outcomes = self.unknown_outcomes.saturating_add(count);
        if count > 0 {
            self.diagnostics
                .increment(names::ACTION_UNKNOWN_TOTAL, "recovered", u64::from(count));
        }
        for (position, intent) in intents.into_iter().enumerate() {
            let reason = format!(
                "outcome unknown after restart (was {}); never retried",
                sanitize_display(&intent.previous_status, 32)
            );
            if position < MAX_UNKNOWN_EVENTS {
                fx.event(
                    intent.request_id,
                    EventKind::ActionOutcomeUnknown(ActionEventPayload {
                        request_id: intent.request_id,
                        intent_id: Some(intent.intent_id),
                        reason: reason.clone(),
                    }),
                );
            }
            if position < MAX_RECENT_ACTIONS {
                self.records.push(recovered_record(intent, &reason, now));
            }
        }
    }
    pub(super) fn note_audit_failure(&mut self, failure: &AuditFailure, now: Now) {
        self.audit_status = Some(PersistenceStatus {
            state: PersistenceState::Failed,
            detail: Some(sanitize_message(&failure.message)),
            ..PersistenceStatus::healthy(PersistenceKind::Audit)
        });
        self.next_status_ns = now.boot_ns.saturating_add(AUDIT_FAILURE_RECHECK_NS);
    }
    pub(super) fn request_recovery(&mut self, now: Now) {
        let op = AuditOp::RecoverUnknown {
            now_boot_ns: now.boot_ns,
            now_utc: crate::time_format::format_utc(now.utc_ns),
        };
        self.recovery_requested = self.audit.try_dispatch(op).is_ok();
    }
    pub(super) fn request_status(&mut self, now: Now) {
        // A refused status request is simply retried at the next refresh.
        let _ = self.audit.try_dispatch(AuditOp::Status);
        self.next_status_ns = now.boot_ns.saturating_add(AUDIT_STATUS_REFRESH_NS);
    }
    pub(super) fn append_attempt(&mut self, facts: AttemptFacts<'_>, now: Now) {
        let attempt = ActionAttempt {
            request_id: facts.request_id,
            payload_sha256: facts.payload_sha256.to_owned(),
            session_digest: facts.session_digest.to_owned(),
            result_code: facts.code.to_owned(),
            detail: sanitize_message(facts.detail),
            occurred_utc: crate::time_format::format_utc(now.utc_ns),
        };
        // Attempts never produced an intent: best effort, never backlogged.
        match self.audit.try_dispatch(AuditOp::AppendAttempt(attempt)) {
            Ok(()) => self.audit_outstanding = self.audit_outstanding.saturating_add(1),
            Err(_) => self
                .diagnostics
                .increment(names::QUEUE_DROPPED_TOTAL, "audit_attempt", 1),
        }
    }
    /// Appends the outcome that closes a dispatched intent (never dropped
    /// silently: a busy worker gets a bounded, ordered backlog).
    pub(super) fn append_outcome(
        &mut self,
        index: usize,
        state: ActionState,
        code: &str,
        detail: &str,
        now: Now,
    ) {
        let Some(confirm) = self
            .records
            .get(index)
            .and_then(|record| record.confirm.clone())
            .filter(|confirm| confirm.intent_dispatched)
        else {
            return;
        };
        let op = AuditOp::AppendOutcome(ActionOutcomeRecord {
            event_id: Uuid::new_v4(),
            request_id: confirm.request_id,
            intent_id: confirm.intent_id,
            state,
            result_code: code.to_owned(),
            detail: sanitize_message(detail),
            occurred_boot_ns: now.boot_ns,
            occurred_utc: crate::time_format::format_utc(now.utc_ns),
        });
        if self.audit_backlog.is_empty() {
            match self.audit.try_dispatch(op.clone()) {
                Ok(()) => {
                    self.audit_outstanding = self.audit_outstanding.saturating_add(1);
                    return;
                }
                Err(DispatchError::Closed) => {
                    self.note_audit_failure(
                        &AuditFailure {
                            code: ErrorCode::Unavailable,
                            message: "audit worker stopped".to_owned(),
                            commit_uncertain: false,
                        },
                        now,
                    );
                    return;
                }
                Err(_) => {}
            }
        }
        if self.audit_backlog.len() >= AUDIT_BACKLOG_LIMIT {
            self.audit_backlog.pop_front();
            self.diagnostics
                .increment(names::QUEUE_DROPPED_TOTAL, "audit_outcome", 1);
        }
        self.audit_backlog.push_back(op);
        self.audit_retry_ns = now.boot_ns.saturating_add(AUDIT_RETRY_NS);
    }
    pub(super) fn retry_audit_backlog(&mut self, now: Now) {
        if self.audit_backlog.is_empty() || now.boot_ns < self.audit_retry_ns {
            return;
        }
        while let Some(op) = self.audit_backlog.front().cloned() {
            if self.audit.try_dispatch(op).is_err() {
                self.audit_retry_ns = now.boot_ns.saturating_add(AUDIT_RETRY_NS);
                break;
            }
            self.audit_backlog.pop_front();
            self.audit_outstanding = self.audit_outstanding.saturating_add(1);
        }
    }
}

fn recovered_record(intent: RecoveredIntent, reason: &str, now: Now) -> ActionRecord {
    ActionRecord {
        prepare_request: intent.request_id,
        session_id: None,
        session_digest: String::new(),
        prepare_payload: String::new(),
        target: intent.process_key,
        signal: intent.signal,
        state: ActionState::Unknown,
        token: None,
        expires_ns: 0,
        policy_revision: 0,
        prepared: None,
        handle_held: false,
        release_wanted: false,
        prepare_deadline_ns: None,
        confirm: None,
        send_deadline_ns: None,
        exit: None,
        revoked: None,
        result: sanitize_message(reason),
        updated_ns: now.boot_ns,
        recovered: true,
    }
}
