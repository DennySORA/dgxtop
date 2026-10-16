//! ProcessService: the only path to a user-process signal (AT10/AT32).
//!
//! `requested → prepared → confirming → intent_pending → intent_committed →
//! sending → signal_sent → exit_observed | still_running`, with
//! `cancelled / expired / rejected / not_sent / unknown` exits. Every
//! transition happens here, on the single control thread, so a token can be
//! consumed exactly once and a signal is sent at most once:
//! * capability checks (real euid, namespace, host/boot, protected pids,
//!   read-only/disabled/root, SIGKILL policy, audit health) run *before* the
//!   action worker sees anything;
//! * a confirmation atomically moves `prepared → confirming` and requests a
//!   durable audit intent; only a durable receipt that arrives while the action
//!   is still `intent_pending`, inside both the intent deadline and the token
//!   TTL, with the same session and policy revision, dispatches the single
//!   `ActionOp::Send` — a late receipt never sends;
//! * `signal_sent` is not `exit_observed`: exit is polled non-blockingly every
//!   50 ms for at most 10 s, and an observed exit has the process table read
//!   again at once, so the process leaves it without waiting for the next
//!   slot (D-109);
//! * outcomes that cannot be proven are `unknown` and are never retried;
//!   intents found without outcome at startup are surfaced, never resent.

mod audit;
mod capability;
mod lifecycle;
mod record;

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use dgxtop_core::action::{
    ActionAttempt, ActionCompletion, ActionError, ActionHandle, ActionIntent, ActionOp,
    ActionOutcomeRecord, ActionState, AuditCompletion, AuditFailure, AuditOp, ConfirmationToken,
    DurableReceipt, ExitObservation, PersistenceKind, PersistenceState, PersistenceStatus,
    PreparedProcess, RecoveredIntent, Signal, SignalOutcome,
};
use dgxtop_core::canonical::ProcessRow;
use dgxtop_core::clock::{NANOS_PER_MILLI, NANOS_PER_SECOND, ms_to_ns};
use dgxtop_core::commands::CommandResult;
use dgxtop_core::config::Config;
use dgxtop_core::digest::sha256_hex;
use dgxtop_core::errors::{ErrorCode, ErrorDetail};
use dgxtop_core::identity::{ProcessKey, SessionId};
use dgxtop_core::ports::{Dispatch, DispatchError};
use dgxtop_core::text::sanitize_message;
use dgxtop_core::view::ActionsView;
use uuid::Uuid;

use crate::effects::Effects;
use audit::AttemptFacts;
pub(crate) use capability::{ActionIdentity, ActionPolicy};
use capability::{AuditReadiness, Denial, availability_problem, capability_context, check_target};
use record::{
    ActionRecord, ActionRegistry, ConfirmContext, ExitWatch, MAX_INFLIGHT_ACTIONS, RevokeReason,
    irreversible_warning,
};

/// The action worker must verify a target within this time.
const PREPARE_TIMEOUT_NS: u64 = 2 * NANOS_PER_SECOND;
/// After the send deadline, a missing `Sent` report means "outcome unknown".
const SEND_GRACE_NS: u64 = 2 * NANOS_PER_SECOND;
const EXIT_POLL_NS: u64 = 50 * NANOS_PER_MILLI;
const EXIT_WATCH_NS: u64 = 10 * NANOS_PER_SECOND;
/// An unanswered exit poll is re-issued after this time.
const EXIT_POLL_STUCK_NS: u64 = 2 * NANOS_PER_SECOND;
const AUDIT_STATUS_REFRESH_NS: u64 = 30 * NANOS_PER_SECOND;
/// Health re-check delay after an audit failure.
const AUDIT_FAILURE_RECHECK_NS: u64 = NANOS_PER_SECOND;
const AUDIT_RETRY_NS: u64 = 20 * NANOS_PER_MILLI;
/// Outcome records waiting for a busy audit worker.
const AUDIT_BACKLOG_LIMIT: usize = 64;

/// Current boot and wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Now {
    pub(crate) boot_ns: u64,
    pub(crate) utc_ns: i128,
}

/// Ports owned by the ProcessService (and nobody else).
pub(crate) struct ProcessPorts {
    pub(crate) audit: Box<dyn Dispatch<AuditOp>>,
    pub(crate) actions: Box<dyn Dispatch<ActionOp>>,
}

/// A validated `PrepareSignal`.
#[derive(Debug)]
pub(crate) struct PrepareRequest<'a> {
    pub(crate) request_id: Uuid,
    pub(crate) session_id: &'a SessionId,
    pub(crate) target: ProcessKey,
    pub(crate) signal: Signal,
    pub(crate) payload_sha256: String,
    /// The target's row of the current process table.
    pub(crate) row: Option<&'a ProcessRow>,
    /// Managed adapter host pids.
    pub(crate) protected_pids: &'a [i32],
}

/// A validated `ConfirmSignal` / `CancelSignal`.
#[derive(Debug)]
pub(crate) struct TokenCommand<'a> {
    pub(crate) request_id: Uuid,
    pub(crate) session_id: &'a SessionId,
    pub(crate) token: &'a ConfirmationToken,
    pub(crate) payload_sha256: String,
}

pub(crate) struct ProcessService {
    audit: Box<dyn Dispatch<AuditOp>>,
    actions: Box<dyn Dispatch<ActionOp>>,
    identity: ActionIdentity,
    policy: ActionPolicy,
    policy_revision: u32,
    audit_status: Option<PersistenceStatus>,
    recovered: bool,
    recovery_requested: bool,
    audit_backlog: VecDeque<AuditOp>,
    audit_retry_ns: u64,
    audit_outstanding: usize,
    audit_expiry: audit::AuditExpirySchedule,
    records: ActionRegistry,
    unknown_outcomes: u32,
    shutting_down: bool,
    started: bool,
    next_status_ns: u64,
}

impl ProcessService {
    pub(crate) fn new(ports: ProcessPorts, identity: ActionIdentity, config: &Config) -> Self {
        Self {
            audit: ports.audit,
            actions: ports.actions,
            identity,
            policy: ActionPolicy::from_config(config),
            policy_revision: 1,
            audit_status: None,
            recovered: false,
            recovery_requested: false,
            audit_backlog: VecDeque::new(),
            audit_retry_ns: 0,
            audit_outstanding: 0,
            audit_expiry: audit::AuditExpirySchedule::new(config.store.audit_retention_days),
            records: ActionRegistry::default(),
            unknown_outcomes: 0,
            shutting_down: false,
            started: false,
            next_status_ns: 0,
        }
    }

    /// Startup: recover intents without outcome and ask for audit health.
    pub(crate) fn start(&mut self, now: Now) {
        if self.started {
            return;
        }
        self.started = true;
        self.request_recovery(now);
        self.request_status(now);
    }

    #[cfg(test)]
    pub(crate) fn unknown_outcomes(&self) -> u32 {
        self.unknown_outcomes
    }

    // ----- commands -------------------------------------------------------

    pub(crate) fn prepare(&mut self, request: PrepareRequest<'_>, now: Now, fx: &mut Effects) {
        let digest = session_digest(request.session_id);
        if let Some(denial) = self.prepare_denial(&request) {
            self.deny_prepare(&request, &digest, &denial, now, fx);
            return;
        }
        let context = capability_context(
            &self.identity,
            &self.policy,
            request.session_id,
            self.policy_revision,
            request.protected_pids,
        );
        let op = ActionOp::Prepare {
            request_id: request.request_id,
            key: request.target.clone(),
            signal: request.signal,
            context,
        };
        if let Err(error) = self.actions.try_dispatch(op) {
            let denial = Denial {
                code: dispatch_code(&error),
                reason: format!("action worker unavailable: {error}"),
            };
            self.deny_prepare(&request, &digest, &denial, now, fx);
            return;
        }
        self.records.push(ActionRecord {
            prepare_request: request.request_id,
            session_id: Some(request.session_id.clone()),
            session_digest: digest,
            prepare_payload: request.payload_sha256,
            target: request.target,
            signal: request.signal,
            state: ActionState::Requested,
            token: None,
            expires_ns: 0,
            policy_revision: self.policy_revision,
            prepared: None,
            handle_held: false,
            release_wanted: false,
            prepare_deadline_ns: Some(now.boot_ns.saturating_add(PREPARE_TIMEOUT_NS)),
            confirm: None,
            send_deadline_ns: None,
            exit: None,
            revoked: None,
        });
        fx.mark_all_dirty();
    }

    pub(crate) fn confirm(&mut self, command: TokenCommand<'_>, now: Now, fx: &mut Effects) {
        let Some(index) = self.records.by_token(command.token) else {
            let denial = Denial {
                code: ErrorCode::ConfirmationExpired,
                reason: "unknown or expired confirmation token".to_owned(),
            };
            self.audit_refused_confirm(&command, &denial, None, now);
            fx.reply(CommandResult::rejected(
                command.request_id,
                denial.code,
                &denial.reason,
            ));
            return;
        };
        if let Some((denial, end_state)) = self.confirm_denial(index, &command, now) {
            let prepare_request = self.records.get(index).map(|record| record.prepare_request);
            self.audit_refused_confirm(&command, &denial, prepare_request, now);
            if let Some(state) = end_state {
                self.close_unconfirmed(index, state);
            }
            fx.reply(CommandResult::rejected(
                command.request_id,
                denial.code,
                &denial.reason,
            ));
            fx.mark_all_dirty();
            return;
        }
        self.begin_intent(index, &command, now, fx);
        fx.mark_all_dirty();
    }

    pub(crate) fn cancel(&mut self, command: TokenCommand<'_>, now: Now, fx: &mut Effects) {
        let Some(index) = self.records.by_token(command.token) else {
            fx.reply(CommandResult::completed(
                command.request_id,
                "no pending confirmation for this token; nothing was sent",
            ));
            return;
        };
        let Some(record) = self.records.get(index) else {
            return;
        };
        if record.session_id.as_ref() != Some(command.session_id) {
            fx.reply(CommandResult::rejected(
                command.request_id,
                ErrorCode::CapabilityDenied,
                "confirmation belongs to another session",
            ));
            return;
        }
        let state = record.state;
        if record.is_revocable() && state != ActionState::Requested {
            self.cancel_unsent(index, &command, now, fx);
        } else if state.is_past_point_of_no_return() {
            fx.reply(
                CommandResult::in_progress(
                    command.request_id,
                    "too late: the signal was already sent or is being sent; it cannot be revoked",
                )
                .with_action_state(state),
            );
        } else {
            fx.reply(
                CommandResult::completed(
                    command.request_id,
                    &format!("nothing to cancel ({})", state.name()),
                )
                .with_action_state(state),
            );
        }
        fx.mark_all_dirty();
    }

    // ----- completions ----------------------------------------------------

    pub(crate) fn on_action(
        &mut self,
        completion: ActionCompletion,
        now: Now,
        open: &BTreeSet<SessionId>,
        fx: &mut Effects,
    ) {
        match completion {
            ActionCompletion::Prepared { request_id, result } => {
                self.on_prepared(request_id, result, now, open, fx);
            }
            ActionCompletion::Sent {
                request_id,
                intent_id,
                outcome,
                ..
            } => self.on_sent(request_id, intent_id, outcome, now, fx),
            ActionCompletion::ExitObserved {
                request_id,
                observation,
            } => {
                // A process still running changes nothing shown.
                if !self.on_exit(request_id, observation, now, fx) {
                    return;
                }
            }
            ActionCompletion::Released { handle } => {
                if let Some(record) = self
                    .records
                    .by_handle(handle)
                    .and_then(|index| self.records.get_mut(index))
                {
                    record.handle_held = false;
                    record.release_wanted = false;
                }
            }
        }
        fx.mark_all_dirty();
    }

    pub(crate) fn on_audit(
        &mut self,
        completion: AuditCompletion,
        now: Now,
        open: &BTreeSet<SessionId>,
        fx: &mut Effects,
    ) {
        match completion {
            AuditCompletion::IntentDurable {
                request_id,
                intent_id,
                result,
            } => {
                self.audit_outstanding = self.audit_outstanding.saturating_sub(1);
                self.on_intent(request_id, intent_id, result, now, open, fx);
            }
            AuditCompletion::OutcomeRecorded { result, .. }
            | AuditCompletion::AttemptRecorded { result, .. } => {
                self.audit_outstanding = self.audit_outstanding.saturating_sub(1);
                if let Err(failure) = result {
                    self.note_audit_failure(&failure, now);
                }
            }
            AuditCompletion::Recovered { result } => {
                self.on_recovered(result, now);
                self.request_expiry(now);
            }
            AuditCompletion::Expired { result } => self.on_expired(result, now),
            AuditCompletion::Status(status) => self.audit_status = Some(status),
        }
        fx.mark_all_dirty();
    }

    // ----- policy, sessions, shutdown ---------------------------------------

    /// Applies the effective action policy; a change revokes every token that
    /// has not reached `sending`.
    pub(crate) fn apply_policy(
        &mut self,
        config: &Config,
        changed: bool,
        now: Now,
        fx: &mut Effects,
    ) {
        self.policy = ActionPolicy::from_config(config);
        if changed {
            self.policy_revision = self.policy_revision.saturating_add(1);
            self.revoke(RevokeReason::PolicyChanged, None, now, fx);
        }
    }

    pub(crate) fn revoke_session(&mut self, session_id: &SessionId, now: Now, fx: &mut Effects) {
        self.revoke(RevokeReason::SessionClosed, Some(session_id), now, fx);
    }

    /// Revokes every token not yet sending; returns how many were revoked.
    pub(crate) fn begin_shutdown(&mut self, now: Now, fx: &mut Effects) -> u32 {
        self.shutting_down = true;
        let revoked = self.revoke(RevokeReason::Shutdown, None, now, fx);
        let watched: Vec<usize> = (0..self.records.len())
            .filter(|index| {
                self.records
                    .get(*index)
                    .is_some_and(|record| record.state == ActionState::SignalSent)
            })
            .collect();
        for index in watched {
            self.conclude_watch(index, "exit not observed before shutdown", now);
        }
        revoked
    }

    /// Whether audit/action completions are still worth waiting for.
    pub(crate) fn awaiting_control(&self) -> bool {
        self.audit_outstanding > 0
            || !self.audit_backlog.is_empty()
            || self
                .records
                .iter()
                .any(|record| record.state == ActionState::Sending)
    }

    /// Final answers at the end of the bounded shutdown; lists what is unfinished.
    pub(crate) fn finish_shutdown(&mut self, now: Now, fx: &mut Effects) -> Vec<String> {
        let mut unfinished = Vec::new();
        for index in 0..self.records.len() {
            let Some(record) = self.records.get(index) else {
                continue;
            };
            match record.state {
                ActionState::Requested => {
                    let request_id = record.prepare_request;
                    unfinished.push(format!(
                        "prepare {request_id}: action worker did not answer"
                    ));
                    self.reject_prepare(
                        index,
                        &ErrorDetail::new(
                            ErrorCode::Unavailable,
                            "shutdown before the target was verified",
                        ),
                        now,
                        fx,
                    );
                }
                ActionState::Sending => {
                    if let Some(request_id) = record.confirm_request() {
                        unfinished.push(format!("action {request_id}: signal outcome unknown"));
                    }
                    self.mark_unknown(
                        index,
                        "shutdown before the send outcome was reported",
                        now,
                        fx,
                    );
                }
                _ => {}
            }
            if let Some(confirm) = self
                .records
                .get(index)
                .and_then(|record| record.confirm.clone())
                && confirm.intent_dispatched
                && !confirm.intent_answered
            {
                unfinished.push(format!(
                    "audit intent {}: durability not confirmed",
                    confirm.intent_id
                ));
            }
        }
        if !self.audit_backlog.is_empty() {
            unfinished.push(format!(
                "{} audit outcome record(s) not written",
                self.audit_backlog.len()
            ));
        }
        if self.audit_outstanding > 0 {
            unfinished.push(format!(
                "{} audit write(s) not confirmed",
                self.audit_outstanding
            ));
        }
        unfinished
    }

    // ----- timers ---------------------------------------------------------

    pub(crate) fn tick(&mut self, now: Now, fx: &mut Effects) {
        self.retry_audit_backlog(now);
        self.request_expiry(now);
        if now.boot_ns >= self.next_status_ns {
            self.request_status(now);
            if !self.recovered && !self.recovery_requested {
                self.request_recovery(now);
            }
        }
        for index in 0..self.records.len() {
            self.tick_record(index, now, fx);
            self.try_release(index);
        }
        self.records.tidy();
    }

    pub(crate) fn next_deadline(&self) -> Option<u64> {
        let records = self.records.iter().filter_map(|record| match record.state {
            ActionState::Requested => record.prepare_deadline_ns,
            ActionState::Prepared => Some(record.expires_ns),
            ActionState::IntentPending => record
                .confirm
                .as_ref()
                .map(|confirm| confirm.intent_deadline_ns),
            ActionState::Sending => record.send_deadline_ns,
            ActionState::SignalSent => record.exit.map(|watch| {
                // While a poll is outstanding the next instant is its
                // re-issue time, never the (already passed) poll time.
                let poll = watch
                    .outstanding_since_ns
                    .map_or(watch.next_poll_ns, |since| {
                        since.saturating_add(EXIT_POLL_STUCK_NS)
                    });
                poll.min(watch.until_ns)
            }),
            _ => None,
        });
        let retry = (!self.audit_backlog.is_empty()).then_some(self.audit_retry_ns);
        records
            .chain(std::iter::once(self.next_status_ns).filter(|_| self.started))
            .chain(retry)
            .min()
    }

    /// The projection for one session (never contains the token).
    pub(crate) fn view_for(&self, session_id: &SessionId, now_ns: u64) -> Arc<ActionsView> {
        let confirmation = self
            .records
            .iter()
            .rev()
            .filter(|record| record.session_id.as_ref() == Some(session_id))
            .find_map(|record| {
                record.confirmation_view(
                    now_ns,
                    irreversible_warning(record.signal, self.unknown_outcomes),
                )
            });
        Arc::new(ActionsView {
            confirmation,
            audit: self
                .audit_status
                .clone()
                .unwrap_or_else(|| PersistenceStatus {
                    state: PersistenceState::Degraded,
                    detail: Some("audit status pending".to_owned()),
                    ..PersistenceStatus::healthy(PersistenceKind::Audit)
                }),
            unknown_outcomes: self.unknown_outcomes,
        })
    }

    // ----- internals: prepare ------------------------------------------------

    fn availability(&self) -> Option<Denial> {
        if self.shutting_down {
            return Some(Denial {
                code: ErrorCode::Unavailable,
                reason: "dgxtop is shutting down".to_owned(),
            });
        }
        availability_problem(
            &self.identity,
            &self.policy,
            AuditReadiness {
                status: self.audit_status.as_ref(),
                recovered: self.recovered,
                backlog: !self.audit_backlog.is_empty(),
            },
        )
    }

    fn prepare_denial(&self, request: &PrepareRequest<'_>) -> Option<Denial> {
        if let Some(denial) = self.availability() {
            return Some(denial);
        }
        if let Err(denial) = check_target(
            &self.identity,
            &self.policy,
            &request.target,
            request.signal,
            request.row,
            request.protected_pids,
        ) {
            return Some(denial);
        }
        if self.records.pending_confirmations() >= self.policy.max_pending_confirmations {
            return Some(Denial {
                code: ErrorCode::CapacityExceeded,
                reason: "too many pending confirmations".to_owned(),
            });
        }
        if self.records.active() >= MAX_INFLIGHT_ACTIONS {
            return Some(Denial {
                code: ErrorCode::CapacityExceeded,
                reason: "too many actions in flight".to_owned(),
            });
        }
        None
    }

    fn deny_prepare(
        &mut self,
        request: &PrepareRequest<'_>,
        digest: &str,
        denial: &Denial,
        now: Now,
        fx: &mut Effects,
    ) {
        fx.reply(CommandResult::rejected(
            request.request_id,
            denial.code,
            &denial.reason,
        ));
        self.append_attempt(
            AttemptFacts {
                request_id: request.request_id,
                payload_sha256: &request.payload_sha256,
                session_digest: digest,
                code: denial.code.as_str(),
                detail: &denial.reason,
            },
            now,
        );
    }

    fn on_prepared(
        &mut self,
        request_id: Uuid,
        result: Result<PreparedProcess, ActionError>,
        now: Now,
        open: &BTreeSet<SessionId>,
        fx: &mut Effects,
    ) {
        let index = self.records.by_prepare(request_id).filter(|index| {
            self.records
                .get(*index)
                .is_some_and(|record| record.state == ActionState::Requested)
        });
        let Some(index) = index else {
            // A late or unknown answer: never keep a descriptor nobody tracks.
            if let Ok(prepared) = result {
                self.release_untracked(prepared.handle);
            }
            return;
        };
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                self.reject_prepare(index, &ErrorDetail::from(error), now, fx);
                return;
            }
        };
        let blocker = self.prepared_blocker(index, &prepared, open);
        if let Some(record) = self.records.get_mut(index) {
            record.prepared = Some(prepared);
            record.handle_held = true;
            record.prepare_deadline_ns = None;
        }
        if let Some(denial) = blocker {
            self.release(index);
            self.reject_prepare(
                index,
                &ErrorDetail::new(denial.code, &denial.reason),
                now,
                fx,
            );
            return;
        }
        self.issue_token(index, now, fx);
    }

    fn prepared_blocker(
        &self,
        index: usize,
        prepared: &PreparedProcess,
        open: &BTreeSet<SessionId>,
    ) -> Option<Denial> {
        let record = self.records.get(index)?;
        let deny = |code: ErrorCode, reason: &str| {
            Some(Denial {
                code,
                reason: reason.to_owned(),
            })
        };
        if prepared.verified.key != record.target {
            return deny(
                ErrorCode::ProcessIdentityChanged,
                "verified identity differs from the selected process",
            );
        }
        if self.policy.same_uid_only && prepared.verified.uid != self.identity.euid {
            return deny(
                ErrorCode::CapabilityDenied,
                "target belongs to another user",
            );
        }
        if let Some(reason) = record.revoked {
            return deny(reason.code(), reason.message());
        }
        if record.policy_revision != self.policy_revision {
            return deny(ErrorCode::CapabilityDenied, "action policy changed");
        }
        if !record
            .session_id
            .as_ref()
            .is_some_and(|id| open.contains(id))
        {
            return deny(ErrorCode::CapabilityDenied, "session closed");
        }
        self.availability()
    }

    fn issue_token(&mut self, index: usize, now: Now, fx: &mut Effects) {
        let Ok(token) = new_token() else {
            self.release(index);
            self.reject_prepare(
                index,
                &ErrorDetail::new(ErrorCode::InternalError, "token generation failed"),
                now,
                fx,
            );
            return;
        };
        let expires_ns = now
            .boot_ns
            .saturating_add(ms_to_ns(u64::from(self.policy.confirmation_ttl_ms)));
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        record.token = Some(token.clone());
        record.expires_ns = expires_ns;
        record.state = ActionState::Prepared;
        let request_id = record.prepare_request;
        let payload = record.prepare_payload.clone();
        let digest = record.session_digest.clone();
        let mut result = CommandResult::completed(request_id, "confirmation required")
            .with_action_state(ActionState::Prepared);
        result.confirmation_token = Some(token);
        result.expires_boot_ns = Some(expires_ns);
        fx.reply(result);
        self.append_attempt(
            AttemptFacts {
                request_id,
                payload_sha256: &payload,
                session_digest: &digest,
                code: "Ok",
                detail: "prepared; awaiting confirmation",
            },
            now,
        );
    }

    fn reject_prepare(&mut self, index: usize, detail: &ErrorDetail, now: Now, fx: &mut Effects) {
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        record.prepare_deadline_ns = None;
        record.state = ActionState::Rejected;
        let request_id = record.prepare_request;
        let payload = record.prepare_payload.clone();
        let digest = record.session_digest.clone();
        fx.reply(
            CommandResult::from_error(request_id, detail).with_action_state(ActionState::Rejected),
        );
        self.append_attempt(
            AttemptFacts {
                request_id,
                payload_sha256: &payload,
                session_digest: &digest,
                code: detail.code.as_str(),
                detail: &detail.message,
            },
            now,
        );
    }

    // ----- internals: confirm -------------------------------------------------

    /// Why a confirmation is refused, and the terminal state it implies.
    fn confirm_denial(
        &self,
        index: usize,
        command: &TokenCommand<'_>,
        now: Now,
    ) -> Option<(Denial, Option<ActionState>)> {
        let record = self.records.get(index)?;
        let deny = |code: ErrorCode, reason: &str, end: Option<ActionState>| {
            Some((
                Denial {
                    code,
                    reason: reason.to_owned(),
                },
                end,
            ))
        };
        if record.session_id.as_ref() != Some(command.session_id) {
            return deny(
                ErrorCode::CapabilityDenied,
                "confirmation belongs to another session",
                None,
            );
        }
        if record.confirm.is_some() {
            return deny(
                ErrorCode::ConfirmationUsed,
                "confirmation already used",
                None,
            );
        }
        if let Some(reason) = record.revoked {
            return deny(reason.code(), reason.message(), None);
        }
        if record.state != ActionState::Prepared {
            let reason = format!("confirmation no longer valid ({})", record.state.name());
            return deny(ErrorCode::ConfirmationExpired, &reason, None);
        }
        if now.boot_ns >= record.expires_ns {
            return deny(
                ErrorCode::ConfirmationExpired,
                "confirmation expired",
                Some(ActionState::Expired),
            );
        }
        if record.policy_revision != self.policy_revision {
            return deny(
                ErrorCode::CapabilityDenied,
                "action policy changed",
                Some(ActionState::Rejected),
            );
        }
        self.availability()
            .map(|denial| (denial, Some(ActionState::Rejected)))
    }

    /// Ends a prepared (never confirmed) action on an internal event (expiry,
    /// policy revocation) and audits the ending. The prepare row already owns
    /// the prepare request id, so the ending is a new attempt row that names
    /// the prepare (reusing the id would be dropped as a duplicate).
    fn end_unconfirmed(&mut self, index: usize, state: ActionState, reason: &str, now: Now) {
        let Some((prepare_request, digest)) = self.close_unconfirmed(index, state) else {
            return;
        };
        let detail = format!("{reason} (prepare request {prepare_request})");
        let payload = sha256_hex(format!("{prepare_request}:{}", state.name()).as_bytes());
        self.append_attempt(
            AttemptFacts {
                request_id: Uuid::new_v4(),
                payload_sha256: &payload,
                session_digest: &digest,
                code: state.name(),
                detail: &detail,
            },
            now,
        );
    }

    /// Moves a prepared action to its terminal state and closes its
    /// descriptor; returns the prepare request id and session digest.
    fn close_unconfirmed(&mut self, index: usize, state: ActionState) -> Option<(Uuid, String)> {
        let record = self.records.get_mut(index)?;
        record.state = state;
        let facts = (record.prepare_request, record.session_digest.clone());
        self.release(index);
        Some(facts)
    }

    /// Audits a refused confirmation under its own request id: it never
    /// produced an intent (`contracts/audit.sql`, `action_attempts`).
    fn audit_refused_confirm(
        &mut self,
        command: &TokenCommand<'_>,
        denial: &Denial,
        prepare_request: Option<Uuid>,
        now: Now,
    ) {
        let detail = match prepare_request {
            Some(prepare) => format!("{} (prepare request {prepare})", denial.reason),
            None => denial.reason.clone(),
        };
        self.append_attempt(
            AttemptFacts {
                request_id: command.request_id,
                payload_sha256: &command.payload_sha256,
                session_digest: &session_digest(command.session_id),
                code: denial.code.as_str(),
                detail: &detail,
            },
            now,
        );
    }

    fn begin_intent(
        &mut self,
        index: usize,
        command: &TokenCommand<'_>,
        now: Now,
        fx: &mut Effects,
    ) {
        let intent_deadline = now
            .boot_ns
            .saturating_add(ms_to_ns(u64::from(self.policy.intent_deadline_ms)));
        let policy_revision = self.policy_revision;
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        let Some(prepared) = record.prepared.clone() else {
            fx.reply(CommandResult::rejected(
                command.request_id,
                ErrorCode::InternalError,
                "prepared action lost its verified identity",
            ));
            return;
        };
        // The atomic consumption of the token: prepared -> confirming.
        record.state = ActionState::Confirming;
        let intent_id = Uuid::new_v4();
        record.confirm = Some(ConfirmContext {
            request_id: command.request_id,
            payload_sha256: command.payload_sha256.clone(),
            intent_id,
            intent_deadline_ns: intent_deadline.min(record.expires_ns),
            intent_dispatched: false,
            intent_answered: false,
            answered: false,
        });
        let intent = ActionIntent {
            request_id: command.request_id,
            intent_id,
            session_digest: record.session_digest.clone(),
            process_key: record.target.clone(),
            uid: prepared.verified.uid,
            basename: prepared.verified.basename.clone(),
            signal: record.signal,
            policy_revision,
            payload_sha256: command.payload_sha256.clone(),
            created_boot_ns: now.boot_ns,
            created_utc: crate::time_format::format_utc(now.utc_ns),
        };
        match self.audit.try_dispatch(AuditOp::AppendIntent(intent)) {
            Ok(()) => {
                self.audit_outstanding = self.audit_outstanding.saturating_add(1);
                if let Some(record) = self.records.get_mut(index) {
                    if let Some(confirm) = record.confirm.as_mut() {
                        confirm.intent_dispatched = true;
                    }
                    record.state = ActionState::IntentPending;
                }
            }
            Err(error) => {
                let denial = Denial {
                    code: dispatch_code(&error),
                    reason: format!("audit worker unavailable ({error}); signal not sent"),
                };
                self.finish_not_sent(index, &denial, ActionState::Rejected, now, fx);
            }
        }
    }

    fn cancel_unsent(
        &mut self,
        index: usize,
        command: &TokenCommand<'_>,
        now: Now,
        fx: &mut Effects,
    ) {
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        record.state = ActionState::Cancelled;
        let pending_confirm = record
            .confirm
            .as_mut()
            .filter(|confirm| !confirm.answered)
            .map(|confirm| {
                confirm.answered = true;
                confirm.request_id
            });
        let had_intent = record
            .confirm
            .as_ref()
            .is_some_and(|confirm| confirm.intent_dispatched);
        let digest = record.session_digest.clone();
        fx.reply(
            CommandResult::completed(command.request_id, "cancelled; no signal was sent")
                .with_action_state(ActionState::Cancelled),
        );
        if let Some(confirm_request) = pending_confirm {
            fx.reply(
                CommandResult::rejected(
                    confirm_request,
                    ErrorCode::Cancelled,
                    "cancelled before sending; no signal sent",
                )
                .with_action_state(ActionState::Cancelled),
            );
        }
        self.release(index);
        if had_intent {
            self.append_outcome(
                index,
                ActionState::Cancelled,
                "Cancelled",
                "cancelled before sending",
                now,
            );
        } else {
            self.append_attempt(
                AttemptFacts {
                    request_id: command.request_id,
                    payload_sha256: &command.payload_sha256,
                    session_digest: &digest,
                    code: "Cancelled",
                    detail: "cancelled before confirmation",
                },
                now,
            );
        }
    }

    // ----- internals: intent and send ------------------------------------------

    fn on_intent(
        &mut self,
        request_id: Uuid,
        intent_id: Uuid,
        result: Result<DurableReceipt, AuditFailure>,
        now: Now,
        open: &BTreeSet<SessionId>,
        fx: &mut Effects,
    ) {
        let Some(index) = self.records.by_confirm(request_id, Some(intent_id)) else {
            return;
        };
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        let state = record.state;
        let Some(confirm) = record.confirm.as_mut() else {
            return;
        };
        if confirm.intent_answered {
            return;
        }
        confirm.intent_answered = true;
        match result {
            Err(failure) => {
                // A definite refusal left no intent row for an outcome to
                // close; an uncertain commit may have, so it keeps its outcome.
                let refused = (!failure.commit_uncertain).then(|| {
                    confirm.intent_dispatched = false;
                    (
                        confirm.request_id,
                        confirm.payload_sha256.clone(),
                        record.session_digest.clone(),
                    )
                });
                self.note_audit_failure(&failure, now);
                if state == ActionState::IntentPending {
                    let marker = if failure.commit_uncertain {
                        " (commit_uncertain)"
                    } else {
                        ""
                    };
                    let denial = Denial {
                        code: ErrorCode::PersistFailed,
                        reason: format!(
                            "audit intent not durable{marker}: {}; signal not sent",
                            failure.message
                        ),
                    };
                    self.finish_not_sent(index, &denial, ActionState::Rejected, now, fx);
                    if let Some((request_id, payload, digest)) = refused {
                        self.append_attempt(
                            AttemptFacts {
                                request_id,
                                payload_sha256: &payload,
                                session_digest: &digest,
                                code: denial.code.as_str(),
                                detail: &denial.reason,
                            },
                            now,
                        );
                    }
                }
            }
            Ok(receipt) if state == ActionState::IntentPending => {
                match self.send_blocker(index, &receipt, now, open) {
                    Some(denial) => {
                        self.finish_not_sent(index, &denial, ActionState::NotSent, now, fx);
                    }
                    None => self.send(index, receipt, now, fx),
                }
            }
            // A late receipt for an action that ended without a signal only
            // gets bookkeeping: its not_sent outcome was already appended.
            Ok(_) => {}
        }
    }

    fn send_blocker(
        &self,
        index: usize,
        receipt: &DurableReceipt,
        now: Now,
        open: &BTreeSet<SessionId>,
    ) -> Option<Denial> {
        let record = self.records.get(index)?;
        let confirm = record.confirm.as_ref()?;
        let deny = |code: ErrorCode, reason: &str| {
            Some(Denial {
                code,
                reason: reason.to_owned(),
            })
        };
        if receipt.request_id != confirm.request_id || receipt.intent_id != confirm.intent_id {
            return deny(
                ErrorCode::InternalError,
                "audit receipt does not match the intent",
            );
        }
        if let Some(reason) = record.revoked {
            return deny(reason.code(), reason.message());
        }
        if self.shutting_down {
            return deny(
                ErrorCode::Unavailable,
                "dgxtop is shutting down; signal not sent",
            );
        }
        if !record
            .session_id
            .as_ref()
            .is_some_and(|id| open.contains(id))
        {
            return deny(
                ErrorCode::Cancelled,
                "session closed before sending; signal not sent",
            );
        }
        if record.policy_revision != self.policy_revision {
            return deny(
                ErrorCode::CapabilityDenied,
                "action policy changed; signal not sent",
            );
        }
        if now.boot_ns >= confirm.intent_deadline_ns {
            return deny(
                ErrorCode::TimedOut,
                "audit intent became durable after its deadline; signal not sent",
            );
        }
        if now.boot_ns >= record.expires_ns {
            return deny(
                ErrorCode::ConfirmationExpired,
                "confirmation expired before sending; signal not sent",
            );
        }
        if !record.handle_held {
            return deny(
                ErrorCode::InternalError,
                "no held process descriptor; signal not sent",
            );
        }
        self.availability()
    }

    /// Dispatches the single `ActionOp::Send` of an action.
    fn send(&mut self, index: usize, receipt: DurableReceipt, now: Now, fx: &mut Effects) {
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        let (Some(confirm), Some(handle)) = (record.confirm.clone(), record.handle()) else {
            return;
        };
        record.state = ActionState::IntentCommitted;
        let send_deadline = record.expires_ns;
        let op = ActionOp::Send {
            request_id: confirm.request_id,
            intent_id: confirm.intent_id,
            handle,
            signal: record.signal,
            receipt,
            send_deadline_boot_ns: send_deadline,
        };
        match self.actions.try_dispatch(op) {
            Ok(()) => {
                if let Some(record) = self.records.get_mut(index) {
                    record.state = ActionState::Sending;
                    record.send_deadline_ns = Some(send_deadline.saturating_add(SEND_GRACE_NS));
                }
            }
            Err(error) => {
                let denial = Denial {
                    code: dispatch_code(&error),
                    reason: format!("action worker unavailable ({error}); signal not sent"),
                };
                self.finish_not_sent(index, &denial, ActionState::NotSent, now, fx);
            }
        }
    }

    /// Ends a confirmed action without a signal.
    fn finish_not_sent(
        &mut self,
        index: usize,
        denial: &Denial,
        state: ActionState,
        now: Now,
        fx: &mut Effects,
    ) {
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        record.state = state;
        let pending_confirm = record
            .confirm
            .as_mut()
            .filter(|confirm| !confirm.answered)
            .map(|confirm| {
                confirm.answered = true;
                confirm.request_id
            });
        if let Some(request_id) = pending_confirm {
            fx.reply(
                CommandResult::rejected(request_id, denial.code, &denial.reason)
                    .with_action_state(state),
            );
        }
        self.release(index);
        self.append_outcome(
            index,
            ActionState::NotSent,
            denial.code.as_str(),
            &denial.reason,
            now,
        );
    }

    /// The process and signal of an action that `completion` says was
    /// sent (D-76: the process history notes why a run ended).
    pub(crate) fn signal_sent(
        &self,
        completion: &ActionCompletion,
    ) -> Option<(ProcessKey, Signal)> {
        let ActionCompletion::Sent {
            request_id,
            intent_id,
            outcome: SignalOutcome::Sent,
            ..
        } = completion
        else {
            return None;
        };
        let record = self
            .records
            .get(self.records.by_confirm(*request_id, Some(*intent_id))?)?;
        Some((record.target.clone(), record.signal))
    }

    fn on_sent(
        &mut self,
        request_id: Uuid,
        intent_id: Uuid,
        outcome: SignalOutcome,
        now: Now,
        fx: &mut Effects,
    ) {
        let Some(index) = self.records.by_confirm(request_id, Some(intent_id)) else {
            return;
        };
        let Some(record) = self.records.get_mut(index) else {
            return;
        };
        let previous = record.state;
        if previous != ActionState::Sending && previous != ActionState::Unknown {
            return;
        }
        record.send_deadline_ns = None;
        if outcome != SignalOutcome::Sent {
            let (code, reason) = signal_failure(&outcome);
            let denial = Denial { code, reason };
            self.finish_not_sent(index, &denial, ActionState::NotSent, now, fx);
            return;
        }
        if previous == ActionState::Unknown {
            // Reported after the unknown verdict: record the truth, but the
            // descriptor is gone, so exit cannot be watched any more.
            record.state = ActionState::StillRunning;
            self.append_outcome(
                index,
                ActionState::SignalSent,
                "Ok",
                "signal sent (late report)",
                now,
            );
            return;
        }
        record.state = ActionState::SignalSent;
        record.exit = Some(ExitWatch {
            next_poll_ns: now.boot_ns.saturating_add(EXIT_POLL_NS),
            until_ns: now.boot_ns.saturating_add(EXIT_WATCH_NS),
            outstanding_since_ns: None,
            last: None,
        });
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
                CommandResult::completed(confirm_request, "signal sent; exit not yet observed")
                    .with_action_state(ActionState::SignalSent),
            );
        }
        self.append_outcome(index, ActionState::SignalSent, "Ok", "signal sent", now);
    }

    /// An exit poll's answer; whether the action ended (exited, or still
    /// running when the watch is over).
    fn on_exit(
        &mut self,
        request_id: Uuid,
        observation: ExitObservation,
        now: Now,
        fx: &mut Effects,
    ) -> bool {
        let Some(index) = self.records.by_confirm(request_id, None) else {
            return false;
        };
        let Some(record) = self.records.get_mut(index) else {
            return false;
        };
        let Some(watch) = record.exit.as_mut() else {
            return false;
        };
        if record.state != ActionState::SignalSent {
            return false;
        }
        watch.outstanding_since_ns = None;
        watch.last = Some(observation);
        if observation == ExitObservation::ExitObserved {
            record.exit = None;
            record.state = ActionState::ExitObserved;
            self.release(index);
            self.append_outcome(
                index,
                ActionState::ExitObserved,
                "Ok",
                "process exit observed",
                now,
            );
            fx.reread_processes();
            return true;
        }
        if now.boot_ns >= watch.until_ns {
            self.conclude_watch(index, "process still running after 10 s", now);
            return true;
        }
        watch.next_poll_ns = now.boot_ns.saturating_add(EXIT_POLL_NS);
        false
    }
}

/// 64 random hex characters (two v4 UUIDs).
fn new_token() -> Result<ConfirmationToken, ActionError> {
    ConfirmationToken::new(format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    ))
}

/// Audit digest of a session id (the id itself is only a routing key).
fn session_digest(session_id: &SessionId) -> String {
    sha256_hex(session_id.as_str().as_bytes())
}

fn dispatch_code(error: &DispatchError) -> ErrorCode {
    match error {
        DispatchError::Closed => ErrorCode::Unavailable,
        DispatchError::Busy | DispatchError::Capacity(_) => ErrorCode::Busy,
    }
}

fn signal_failure(outcome: &SignalOutcome) -> (ErrorCode, String) {
    match outcome {
        SignalOutcome::Sent => (ErrorCode::InternalError, "signal sent".to_owned()),
        SignalOutcome::PermissionDenied => (
            ErrorCode::PermissionDenied,
            "the kernel refused the signal; not sent".to_owned(),
        ),
        SignalOutcome::Exited => (
            ErrorCode::ProcessExited,
            "the process exited before the signal; not sent".to_owned(),
        ),
        SignalOutcome::Changed => (
            ErrorCode::ProcessIdentityChanged,
            "the process identity changed; not sent".to_owned(),
        ),
        SignalOutcome::Unavailable { reason } => (
            ErrorCode::Unavailable,
            format!("action adapter unavailable: {reason}; not sent"),
        ),
        SignalOutcome::NotSent { reason } => (
            ErrorCode::ConfirmationExpired,
            format!("not sent: {reason}"),
        ),
        SignalOutcome::Error { errno } => (
            ErrorCode::InternalError,
            format!("signal failed with errno {errno}; not sent"),
        ),
    }
}

#[cfg(test)]
mod tests;
