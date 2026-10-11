//! AT10 / AT32 (manager side): authorization, stable identity, durable intent
//! before a single send, late audit receipts, cancellation, revocation,
//! unknown outcomes. Only fakes: no process is ever signalled.

use super::*;
use dgxtop_core::action::VerifiedIdentity;
use dgxtop_core::errors::ResultCode;
use dgxtop_core::identity::{Pid, PidNamespace};
use dgxtop_core::quality::SampleKind;
use dgxtop_core::testing::{CapturingDispatch, boot_id, host_id};

const SECOND: u64 = NANOS_PER_SECOND;
const MS: u64 = NANOS_PER_MILLI;
const EUID: u32 = 1000;
const SELF_PID: i32 = 4000;

struct Bench {
    service: ProcessService,
    audit: CapturingDispatch<AuditOp>,
    actions: CapturingDispatch<ActionOp>,
    now: Now,
    session: SessionId,
    open: BTreeSet<SessionId>,
}

fn identity(euid: u32) -> ActionIdentity {
    ActionIdentity {
        host_id: host_id("test-host"),
        boot_id: boot_id("fake-boot"),
        pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
        euid,
        self_pid: SELF_PID,
    }
}

fn bench_with(config: &Config, euid: u32) -> Bench {
    let audit = CapturingDispatch::new();
    let actions = CapturingDispatch::new();
    let session = SessionId::new("s1").unwrap();
    let service = ProcessService::new(
        ProcessPorts {
            audit: Box::new(audit.clone()),
            actions: Box::new(actions.clone()),
        },
        identity(euid),
        config,
    );
    let mut bench = Bench {
        service,
        audit,
        actions,
        now: Now {
            boot_ns: 1_000 * SECOND,
            utc_ns: 1_790_000_000_000_000_000,
        },
        open: BTreeSet::from([session.clone()]),
        session,
    };
    bench.service.start(bench.now);
    let startup = bench.audit.take();
    assert!(matches!(startup[0], AuditOp::RecoverUnknown { .. }));
    assert!(matches!(startup[1], AuditOp::Status));
    bench.audit_completion(AuditCompletion::Recovered {
        result: Ok(Vec::new()),
    });
    bench.audit_completion(AuditCompletion::Status(PersistenceStatus::healthy(
        PersistenceKind::Audit,
    )));
    bench
}

fn bench() -> Bench {
    bench_with(&Config::defaults().unwrap(), EUID)
}

fn target(pid: i64) -> ProcessKey {
    ProcessKey {
        host_id: host_id("test-host"),
        boot_id: boot_id("fake-boot"),
        pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
        pid: Pid::new(pid).unwrap(),
        start_time_ticks: 90_100,
    }
}

fn row(key: &ProcessKey, uid: u32) -> ProcessRow {
    ProcessRow {
        key: key.clone(),
        name: "train.py".into(),
        command: String::new(),
        uid: Some(uid),
        state: Some('S'),
        descendants: None,
        cpu_percent: Some(10.0),
        rss_bytes: Some(1 << 20),
        missing_reason: None,
        gpu: Vec::new(),
        sample_kind: SampleKind::Measured,
        read_finished_boot_ns: 0,
    }
}

fn result_for(fx: &Effects, request_id: Uuid) -> &CommandResult {
    fx.results
        .iter()
        .find(|result| result.request_id == request_id)
        .unwrap_or_else(|| panic!("no result for {request_id}: {:?}", fx.results))
}

impl Bench {
    fn advance_ms(&mut self, ms: u64) {
        self.now.boot_ns += ms * MS;
        self.now.utc_ns += i128::from(ms * MS);
    }

    fn tick(&mut self) -> Effects {
        let mut fx = Effects::default();
        self.service.tick(self.now, &mut fx);
        fx
    }

    fn audit_completion(&mut self, completion: AuditCompletion) -> Effects {
        let mut fx = Effects::default();
        self.service
            .on_audit(completion, self.now, &self.open, &mut fx);
        fx
    }

    fn action_completion(&mut self, completion: ActionCompletion) -> Effects {
        let mut fx = Effects::default();
        self.service
            .on_action(completion, self.now, &self.open, &mut fx);
        fx
    }

    fn prepare_with(
        &mut self,
        key: &ProcessKey,
        signal: Signal,
        uid: u32,
        protected: &[i32],
    ) -> (Uuid, Effects) {
        let request_id = Uuid::new_v4();
        let table_row = row(key, uid);
        let mut fx = Effects::default();
        let session = self.session.clone();
        self.service.prepare(
            PrepareRequest {
                request_id,
                session_id: &session,
                target: key.clone(),
                signal,
                payload_sha256: "prepare-digest".into(),
                row: Some(&table_row),
                protected_pids: protected,
            },
            self.now,
            &mut fx,
        );
        (request_id, fx)
    }

    /// Prepares pid 4242 and lets the worker verify it; returns the token.
    fn prepared(&mut self, signal: Signal) -> (Uuid, ConfirmationToken) {
        let key = target(4242);
        let (request_id, fx) = self.prepare_with(&key, signal, EUID, &[]);
        assert!(fx.results.is_empty(), "prepare waits for the worker");
        let ops = self.actions.take();
        let ActionOp::Prepare {
            request_id: op_id,
            context,
            key: op_key,
            ..
        } = &ops[0]
        else {
            panic!("prepare op expected, got {ops:?}");
        };
        assert_eq!(*op_id, request_id);
        assert_eq!(op_key, &key);
        assert_eq!(context.euid, EUID, "real euid, never from JSON");
        assert!(context.protected_pids.contains(&SELF_PID));
        let fx = self.action_completion(ActionCompletion::Prepared {
            request_id,
            result: Ok(prepared_process(&key, 7)),
        });
        let result = result_for(&fx, request_id);
        assert!(result.code.is_ok());
        assert_eq!(result.action_state, Some(ActionState::Prepared));
        let token = result.confirmation_token.clone().unwrap();
        assert_eq!(token.as_str().len(), 64);
        assert!(token.as_str().bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            result.expires_boot_ns,
            Some(self.now.boot_ns + 10 * SECOND),
            "confirmation_ttl_ms"
        );
        (request_id, token)
    }

    fn confirm(&mut self, token: &ConfirmationToken) -> (Uuid, Effects) {
        let request_id = Uuid::new_v4();
        let mut fx = Effects::default();
        let session = self.session.clone();
        self.service.confirm(
            TokenCommand {
                request_id,
                session_id: &session,
                token,
                payload_sha256: format!("confirm-{request_id}"),
            },
            self.now,
            &mut fx,
        );
        (request_id, fx)
    }

    fn cancel(&mut self, token: &ConfirmationToken) -> (Uuid, Effects) {
        let request_id = Uuid::new_v4();
        let mut fx = Effects::default();
        let session = self.session.clone();
        self.service.cancel(
            TokenCommand {
                request_id,
                session_id: &session,
                token,
                payload_sha256: "cancel".into(),
            },
            self.now,
            &mut fx,
        );
        (request_id, fx)
    }

    fn intents(&self) -> Vec<ActionIntent> {
        self.audit
            .take()
            .into_iter()
            .filter_map(|op| match op {
                AuditOp::AppendIntent(intent) => Some(intent),
                _ => None,
            })
            .collect()
    }

    fn attempts(&self) -> Vec<ActionAttempt> {
        self.audit
            .take()
            .into_iter()
            .filter_map(|op| match op {
                AuditOp::AppendAttempt(attempt) => Some(attempt),
                _ => None,
            })
            .collect()
    }

    fn durable(&mut self, intent: &ActionIntent) -> Effects {
        self.audit_completion(AuditCompletion::IntentDurable {
            request_id: intent.request_id,
            intent_id: intent.intent_id,
            result: Ok(DurableReceipt {
                request_id: intent.request_id,
                intent_id: intent.intent_id,
                committed_boot_ns: self.now.boot_ns,
            }),
        })
    }

    fn sends(ops: &[ActionOp]) -> Vec<&ActionOp> {
        ops.iter()
            .filter(|op| matches!(op, ActionOp::Send { .. }))
            .collect()
    }

    fn outcomes(&self) -> Vec<ActionOutcomeRecord> {
        self.audit
            .take()
            .into_iter()
            .filter_map(|op| match op {
                AuditOp::AppendOutcome(outcome) => Some(outcome),
                _ => None,
            })
            .collect()
    }

    fn state_of(&self, prepare_request: Uuid) -> ActionState {
        self.service
            .view_for(&self.session, self.now.boot_ns)
            .recent
            .iter()
            .find(|record| record.request_id == prepare_request)
            .map(|record| record.state)
            .unwrap()
    }
}

fn prepared_process(key: &ProcessKey, handle: u64) -> PreparedProcess {
    PreparedProcess {
        handle: ActionHandle(handle),
        verified: VerifiedIdentity {
            key: key.clone(),
            uid: EUID,
            basename: "python3\u{1b}[2J".into(),
        },
        display_summary: key.summary(),
    }
}

#[test]
fn happy_path_sends_exactly_once_after_the_durable_intent() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    let view = bench.service.view_for(&bench.session, bench.now.boot_ns);
    let confirmation = view.confirmation.as_ref().unwrap();
    assert_eq!(confirmation.uid, EUID);
    assert_eq!(confirmation.expires_in_ms, 10_000);
    assert!(confirmation.warning.contains("cannot be undone"));
    assert!(
        !serde_json::to_string(&*view)
            .unwrap()
            .contains(token.as_str())
    );
    bench.audit.take();

    let (confirm_id, fx) = bench.confirm(&token);
    assert!(
        fx.results.is_empty(),
        "confirm waits for the durable intent"
    );
    assert!(bench.actions.is_empty(), "no signal before durability");
    let intents = bench.intents();
    assert_eq!(intents.len(), 1);
    let intent = &intents[0];
    assert_eq!(intent.request_id, confirm_id);
    assert_eq!(intent.uid, EUID);
    assert_eq!(
        intent.basename, "python3\u{1b}[2J",
        "the verified name travels unchanged; the audit store escapes it"
    );
    assert_eq!(intent.session_digest, sha256_hex(b"s1"));
    assert_eq!(intent.payload_sha256, format!("confirm-{confirm_id}"));
    assert_eq!(bench.state_of(prepare_id), ActionState::IntentPending);

    bench.advance_ms(5);
    bench.durable(intent);
    let ops = bench.actions.take();
    let sends = Bench::sends(&ops);
    assert_eq!(sends.len(), 1);
    let ActionOp::Send {
        handle,
        signal,
        send_deadline_boot_ns,
        intent_id,
        ..
    } = sends[0]
    else {
        unreachable!()
    };
    assert_eq!(*handle, ActionHandle(7), "the held pidfd, never a PID");
    assert_eq!(*signal, Signal::Sigterm);
    assert_eq!(*intent_id, intent.intent_id);
    assert_eq!(
        *send_deadline_boot_ns,
        1_010 * SECOND,
        "token expiry is the send deadline"
    );
    assert_eq!(bench.state_of(prepare_id), ActionState::Sending);

    let fx = bench.action_completion(ActionCompletion::Sent {
        request_id: confirm_id,
        intent_id: intent.intent_id,
        outcome: SignalOutcome::Sent,
        sent_boot_ns: bench.now.boot_ns,
    });
    let result = result_for(&fx, confirm_id);
    assert!(result.code.is_ok());
    assert_eq!(
        result.action_state,
        Some(ActionState::SignalSent),
        "sent is not exit"
    );
    assert_eq!(bench.state_of(prepare_id), ActionState::SignalSent);
    let outcomes = bench.outcomes();
    assert_eq!(outcomes.last().unwrap().state, ActionState::SignalSent);

    // Non-blocking exit observation every 250 ms.
    bench.advance_ms(250);
    bench.tick();
    let ops = bench.actions.take();
    assert!(matches!(
        ops[0],
        ActionOp::ObserveExit {
            handle: ActionHandle(7),
            ..
        }
    ));
    bench.action_completion(ActionCompletion::ExitObserved {
        request_id: confirm_id,
        observation: ExitObservation::Running,
    });
    bench.advance_ms(250);
    bench.tick();
    assert_eq!(bench.actions.take().len(), 1);
    bench.action_completion(ActionCompletion::ExitObserved {
        request_id: confirm_id,
        observation: ExitObservation::ExitObserved,
    });
    assert_eq!(bench.state_of(prepare_id), ActionState::ExitObserved);
    assert!(matches!(
        bench.actions.take()[..],
        [ActionOp::Release {
            handle: ActionHandle(7)
        }]
    ));
    assert_eq!(
        bench.outcomes().last().unwrap().state,
        ActionState::ExitObserved
    );
}

#[test]
fn ten_concurrent_confirms_of_one_token_send_exactly_once() {
    let mut bench = bench();
    let (_, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    let mut used = 0;
    let mut first = None;
    for _ in 0..10 {
        let (request_id, fx) = bench.confirm(&token);
        match fx.results.first() {
            None => first = Some(request_id),
            Some(result) => {
                assert_eq!(result.code, ResultCode::from(ErrorCode::ConfirmationUsed));
                used += 1;
            }
        }
    }
    assert_eq!(used, 9);
    assert!(first.is_some());
    let intents = bench.intents();
    assert_eq!(intents.len(), 1, "one durable intent");
    bench.durable(&intents[0]);
    // A duplicated receipt does not send again.
    bench.durable(&intents[0]);
    let ops = bench.actions.take();
    assert_eq!(Bench::sends(&ops).len(), 1);
}

#[test]
fn expired_tokens_are_rejected_and_release_the_descriptor() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    bench.advance_ms(10_000);
    bench.tick();
    assert_eq!(bench.state_of(prepare_id), ActionState::Expired);
    assert!(matches!(
        bench.actions.take()[..],
        [ActionOp::Release {
            handle: ActionHandle(7)
        }]
    ));
    let (confirm_id, fx) = bench.confirm(&token);
    assert_eq!(
        result_for(&fx, confirm_id).code,
        ResultCode::from(ErrorCode::ConfirmationExpired)
    );
    assert!(bench.intents().is_empty());
    // A token never issued by this Manager is "expired" as well.
    let forged = ConfirmationToken::new("f".repeat(64)).unwrap();
    let (forged_id, fx) = bench.confirm(&forged);
    assert_eq!(
        result_for(&fx, forged_id).code,
        ResultCode::from(ErrorCode::ConfirmationExpired)
    );
}

#[test]
fn the_intent_carries_the_verified_command_name_unchanged_for_the_audit() {
    let mut bench = bench();
    let key = target(4242);
    let (request_id, _) = bench.prepare_with(&key, Signal::Sigterm, EUID, &[]);
    bench.actions.take();
    let mut prepared = prepared_process(&key, 7);
    prepared.verified.basename = "kworker/0:1H\u{1b}[2J".to_owned();
    let fx = bench.action_completion(ActionCompletion::Prepared {
        request_id,
        result: Ok(prepared),
    });
    let token = result_for(&fx, request_id)
        .confirmation_token
        .clone()
        .unwrap();
    bench.audit.take();
    bench.confirm(&token);
    let intents = bench.intents();
    // The store escapes it; splitting at '/' or stripping here would lose
    // the auditable form.
    assert_eq!(intents[0].basename, "kworker/0:1H\u{1b}[2J");
}

#[test]
fn expiry_is_audited_as_its_own_attempt_naming_the_prepare() {
    let mut bench = bench();
    let (prepare_id, _token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    bench.advance_ms(10_000);
    bench.tick();
    let attempts = bench.attempts();
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    let attempt = &attempts[0];
    // The prepare row owns its request id: reusing it would make the store
    // drop the ending as a duplicate of the prepare.
    assert_ne!(attempt.request_id, prepare_id);
    assert_eq!(attempt.result_code, ActionState::Expired.name());
    assert!(
        attempt.detail.contains(&prepare_id.to_string()),
        "{}",
        attempt.detail
    );
    assert_eq!(attempt.session_digest, session_digest(&bench.session));
    assert_eq!(attempt.payload_sha256.len(), 64);
}

#[test]
fn policy_revocation_of_an_unconfirmed_token_is_audited_as_its_own_attempt() {
    let mut bench = bench();
    let (prepare_id, _token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    let mut stricter = Config::defaults().unwrap();
    stricter.actions.confirmation_ttl_ms = 5000;
    let mut fx = Effects::default();
    bench
        .service
        .apply_policy(&stricter, true, bench.now, &mut fx);
    let attempts = bench.attempts();
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert_ne!(attempts[0].request_id, prepare_id);
    assert_eq!(attempts[0].result_code, ActionState::Rejected.name());
    assert!(attempts[0].detail.contains(&prepare_id.to_string()));
}

#[test]
fn refused_confirmations_are_audited_under_their_own_request() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    bench.advance_ms(10_001);
    let (confirm_id, _) = bench.confirm(&token);
    let attempts = bench.attempts();
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert_eq!(attempts[0].request_id, confirm_id);
    assert_eq!(attempts[0].payload_sha256, format!("confirm-{confirm_id}"));
    assert_eq!(
        attempts[0].result_code,
        ErrorCode::ConfirmationExpired.as_str()
    );
    assert!(attempts[0].detail.contains(&prepare_id.to_string()));
    assert_eq!(bench.state_of(prepare_id), ActionState::Expired);
    // An unknown token is a refused confirmation as well.
    let forged = ConfirmationToken::new("e".repeat(64)).unwrap();
    let (forged_id, _) = bench.confirm(&forged);
    let attempts = bench.attempts();
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert_eq!(attempts[0].request_id, forged_id);
    assert_eq!(
        attempts[0].result_code,
        ErrorCode::ConfirmationExpired.as_str()
    );
}

#[test]
fn a_confirmation_from_another_session_is_audited_and_the_token_stays_valid() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    let other = SessionId::new("s2").unwrap();
    let request_id = Uuid::new_v4();
    let mut fx = Effects::default();
    bench.service.confirm(
        TokenCommand {
            request_id,
            session_id: &other,
            token: &token,
            payload_sha256: "foreign".into(),
        },
        bench.now,
        &mut fx,
    );
    assert_eq!(
        result_for(&fx, request_id).code,
        ResultCode::from(ErrorCode::CapabilityDenied)
    );
    let attempts = bench.attempts();
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert_eq!(attempts[0].request_id, request_id);
    assert_eq!(
        attempts[0].result_code,
        ErrorCode::CapabilityDenied.as_str()
    );
    assert_eq!(attempts[0].session_digest, session_digest(&other));
    assert_eq!(bench.state_of(prepare_id), ActionState::Prepared);
}

#[test]
fn ttl_passing_between_ticks_is_still_enforced_at_confirm() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    bench.advance_ms(10_001);
    let (confirm_id, fx) = bench.confirm(&token);
    assert_eq!(
        result_for(&fx, confirm_id).code,
        ResultCode::from(ErrorCode::ConfirmationExpired)
    );
    assert_eq!(bench.state_of(prepare_id), ActionState::Expired);
}

#[test]
fn cancel_before_send_wins_over_a_later_durable_intent() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    let (confirm_id, _) = bench.confirm(&token);
    let intents = bench.intents();
    let (cancel_id, fx) = bench.cancel(&token);
    let cancel = result_for(&fx, cancel_id);
    assert!(cancel.code.is_ok());
    assert_eq!(cancel.action_state, Some(ActionState::Cancelled));
    assert_eq!(
        result_for(&fx, confirm_id).code,
        ResultCode::from(ErrorCode::Cancelled)
    );
    assert_eq!(bench.state_of(prepare_id), ActionState::Cancelled);
    bench.durable(&intents[0]);
    let ops = bench.actions.take();
    assert!(
        Bench::sends(&ops).is_empty(),
        "late receipt after cancel never sends"
    );
    assert!(ops.iter().any(|op| matches!(op, ActionOp::Release { .. })));
    let outcomes = bench.outcomes();
    assert!(
        outcomes
            .iter()
            .any(|outcome| outcome.state == ActionState::Cancelled)
    );
    // Cancelling again is idempotent.
    let (again_id, fx) = bench.cancel(&token);
    assert!(result_for(&fx, again_id).code.is_ok());
}

#[test]
fn cancel_of_a_prepared_token_closes_the_descriptor() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    let (cancel_id, fx) = bench.cancel(&token);
    assert!(result_for(&fx, cancel_id).code.is_ok());
    assert_eq!(bench.state_of(prepare_id), ActionState::Cancelled);
    assert!(matches!(
        bench.actions.take()[..],
        [ActionOp::Release {
            handle: ActionHandle(7)
        }]
    ));
    let (confirm_id, fx) = bench.confirm(&token);
    assert_eq!(
        result_for(&fx, confirm_id).code,
        ResultCode::from(ErrorCode::ConfirmationExpired)
    );
    let unknown = ConfirmationToken::new("0".repeat(64)).unwrap();
    let (unknown_id, fx) = bench.cancel(&unknown);
    assert!(
        result_for(&fx, unknown_id).code.is_ok(),
        "cancel is idempotent"
    );
}

#[test]
fn cancel_after_sending_is_too_late() {
    let mut bench = bench();
    let (_, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    bench.confirm(&token);
    let intents = bench.intents();
    bench.durable(&intents[0]);
    let (cancel_id, fx) = bench.cancel(&token);
    let result = result_for(&fx, cancel_id);
    assert_eq!(
        result.status,
        dgxtop_core::commands::CommandStatus::InProgress
    );
    assert!(result.message.contains("too late"));
    assert_eq!(result.action_state, Some(ActionState::Sending));
}

#[test]
fn late_audit_receipt_after_the_intent_deadline_never_sends() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    let (confirm_id, _) = bench.confirm(&token);
    let intents = bench.intents();
    bench.advance_ms(1_000);
    let fx = bench.tick();
    let result = result_for(&fx, confirm_id);
    assert_eq!(result.code, ResultCode::from(ErrorCode::TimedOut));
    assert_eq!(result.action_state, Some(ActionState::Rejected));
    assert_eq!(bench.state_of(prepare_id), ActionState::NotSent);
    let outcomes = bench.outcomes();
    assert_eq!(outcomes[0].state, ActionState::NotSent);
    bench.actions.take();
    bench.durable(&intents[0]);
    assert!(Bench::sends(&bench.actions.take()).is_empty());
}

#[test]
fn receipt_arriving_after_the_deadline_but_before_a_tick_does_not_send() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    let (confirm_id, _) = bench.confirm(&token);
    let intents = bench.intents();
    bench.actions.take();
    // No tick has timed the intent out: the receipt itself must be refused.
    bench.advance_ms(1_000);
    let fx = bench.durable(&intents[0]);
    let ops = bench.actions.take();
    assert!(Bench::sends(&ops).is_empty());
    assert!(ops.iter().any(|op| matches!(op, ActionOp::Release { .. })));
    let result = result_for(&fx, confirm_id);
    assert_eq!(result.code, ResultCode::from(ErrorCode::TimedOut));
    assert_eq!(result.action_state, Some(ActionState::Rejected));
    assert_eq!(bench.state_of(prepare_id), ActionState::NotSent);
    assert_eq!(bench.outcomes()[0].state, ActionState::NotSent);
}

#[test]
fn policy_change_revokes_tokens_that_have_not_reached_sending() {
    let mut bench = bench();
    let (prepared_id, prepared_token) = bench.prepared(Signal::Sigterm);
    let (_, pending_token) = {
        let key = target(5151);
        let (request_id, _) = bench.prepare_with(&key, Signal::Sigterm, EUID, &[]);
        bench.actions.take();
        let fx = bench.action_completion(ActionCompletion::Prepared {
            request_id,
            result: Ok(prepared_process(&key, 9)),
        });
        (
            request_id,
            result_for(&fx, request_id)
                .confirmation_token
                .clone()
                .unwrap(),
        )
    };
    bench.audit.take();
    let (pending_confirm, _) = bench.confirm(&pending_token);
    let intents = bench.intents();
    let mut stricter = Config::defaults().unwrap();
    stricter.actions.confirmation_ttl_ms = 5000;
    let mut fx = Effects::default();
    bench
        .service
        .apply_policy(&stricter, true, bench.now, &mut fx);
    assert_eq!(
        result_for(&fx, pending_confirm).code,
        ResultCode::from(ErrorCode::CapabilityDenied)
    );
    assert_eq!(bench.state_of(prepared_id), ActionState::Rejected);
    let releases = bench
        .actions
        .take()
        .into_iter()
        .filter(|op| matches!(op, ActionOp::Release { .. }))
        .count();
    assert_eq!(releases, 2);
    let (confirm_id, fx) = bench.confirm(&prepared_token);
    assert_eq!(
        result_for(&fx, confirm_id).code,
        ResultCode::from(ErrorCode::CapabilityDenied)
    );
    bench.durable(&intents[0]);
    assert!(Bench::sends(&bench.actions.take()).is_empty());
    // An unchanged policy revokes nothing.
    let mut fx = Effects::default();
    bench
        .service
        .apply_policy(&stricter, false, bench.now, &mut fx);
    assert!(fx.results.is_empty());
}

#[test]
fn forbidden_requests_are_rejected_before_the_worker_sees_them() {
    let defaults = Config::defaults().unwrap();
    let mut read_only = defaults.clone();
    read_only.actions.read_only = true;
    let mut disabled = defaults.clone();
    disabled.actions.enabled = false;
    // SIGKILL is allowed by default (D-71); a configuration that turns it
    // off still refuses it.
    let mut no_force = defaults.clone();
    no_force.actions.allow_force = false;
    struct Case {
        name: &'static str,
        config: Config,
        euid: u32,
        key: ProcessKey,
        signal: Signal,
        uid: u32,
        protected: Vec<i32>,
        code: ErrorCode,
    }
    let mut other_ns = target(4242);
    other_ns.pid_namespace = PidNamespace::new("pid:[4026532000]").unwrap();
    let mut other_host = target(4242);
    other_host.host_id = host_id("elsewhere");
    let mut other_boot = target(4242);
    other_boot.boot_id = boot_id("previous-boot");
    let cases = vec![
        Case {
            name: "root",
            config: defaults.clone(),
            euid: 0,
            key: target(4242),
            signal: Signal::Sigterm,
            uid: 0,
            protected: vec![],
            code: ErrorCode::CapabilityDenied,
        },
        Case {
            name: "read_only",
            config: read_only,
            euid: EUID,
            key: target(4242),
            signal: Signal::Sigterm,
            uid: EUID,
            protected: vec![],
            code: ErrorCode::CapabilityDenied,
        },
        Case {
            name: "disabled",
            config: disabled,
            euid: EUID,
            key: target(4242),
            signal: Signal::Sigterm,
            uid: EUID,
            protected: vec![],
            code: ErrorCode::CapabilityDenied,
        },
        Case {
            name: "force",
            config: no_force,
            euid: EUID,
            key: target(4242),
            signal: Signal::Sigkill,
            uid: EUID,
            protected: vec![],
            code: ErrorCode::CapabilityDenied,
        },
        Case {
            name: "namespace",
            config: defaults.clone(),
            euid: EUID,
            key: other_ns,
            signal: Signal::Sigterm,
            uid: EUID,
            protected: vec![],
            code: ErrorCode::NamespaceMismatch,
        },
        Case {
            name: "host",
            config: defaults.clone(),
            euid: EUID,
            key: other_host,
            signal: Signal::Sigterm,
            uid: EUID,
            protected: vec![],
            code: ErrorCode::NamespaceMismatch,
        },
        Case {
            name: "boot",
            config: defaults.clone(),
            euid: EUID,
            key: other_boot,
            signal: Signal::Sigterm,
            uid: EUID,
            protected: vec![],
            code: ErrorCode::NamespaceMismatch,
        },
        Case {
            name: "self",
            config: defaults.clone(),
            euid: EUID,
            key: target(i64::from(SELF_PID)),
            signal: Signal::Sigterm,
            uid: EUID,
            protected: vec![],
            code: ErrorCode::CapabilityDenied,
        },
        Case {
            name: "pid1",
            config: defaults.clone(),
            euid: EUID,
            key: target(1),
            signal: Signal::Sigterm,
            uid: EUID,
            protected: vec![],
            code: ErrorCode::CapabilityDenied,
        },
        Case {
            name: "adapter_host",
            config: defaults.clone(),
            euid: EUID,
            key: target(5555),
            signal: Signal::Sigterm,
            uid: EUID,
            protected: vec![5555],
            code: ErrorCode::CapabilityDenied,
        },
        Case {
            name: "other_uid",
            config: defaults.clone(),
            euid: EUID,
            key: target(4242),
            signal: Signal::Sigterm,
            uid: 0,
            protected: vec![],
            code: ErrorCode::CapabilityDenied,
        },
    ];
    for case in cases {
        let mut bench = bench_with(&case.config, case.euid);
        bench.audit.take();
        let (request_id, fx) =
            bench.prepare_with(&case.key, case.signal, case.uid, &case.protected);
        assert_eq!(
            result_for(&fx, request_id).code,
            ResultCode::from(case.code),
            "{}",
            case.name
        );
        assert!(
            bench.actions.is_empty(),
            "{}: worker must not see it",
            case.name
        );
        let audit = bench.audit.take();
        assert!(
            matches!(audit[..], [AuditOp::AppendAttempt(_)]),
            "{}: the refusal is audited",
            case.name
        );
    }
}

#[test]
fn sigkill_is_allowed_only_with_allow_force() {
    let mut config = Config::defaults().unwrap();
    config.actions.allow_force = true;
    let mut bench = bench_with(&config, EUID);
    let (_, token) = bench.prepared(Signal::Sigkill);
    let view = bench.service.view_for(&bench.session, bench.now.boot_ns);
    assert!(
        view.confirmation
            .as_ref()
            .unwrap()
            .warning
            .contains("SIGKILL")
    );
    assert_eq!(token.as_str().len(), 64);
}

#[test]
fn audit_trouble_fails_closed() {
    let mut bench = bench();
    bench.audit_completion(AuditCompletion::Status(PersistenceStatus {
        state: PersistenceState::Failed,
        detail: Some("disk full".into()),
        ..PersistenceStatus::healthy(PersistenceKind::Audit)
    }));
    let (request_id, fx) = bench.prepare_with(&target(4242), Signal::Sigterm, EUID, &[]);
    assert_eq!(
        result_for(&fx, request_id).code,
        ResultCode::from(ErrorCode::Unavailable)
    );
    assert!(bench.actions.is_empty());
    let view = bench.service.view_for(&bench.session, bench.now.boot_ns);
    assert!(!view.actions_available);
    assert!(
        view.unavailable_reason
            .as_deref()
            .unwrap()
            .contains("disk full")
    );
    // Before the first status report actions are unavailable as well.
    let audit = CapturingDispatch::new();
    let actions = CapturingDispatch::new();
    let mut fresh = ProcessService::new(
        ProcessPorts {
            audit: Box::new(audit.clone()),
            actions: Box::new(actions.clone()),
        },
        identity(EUID),
        &Config::defaults().unwrap(),
    );
    fresh.start(bench.now);
    let mut fx = Effects::default();
    let session = bench.session.clone();
    let key = target(4242);
    fresh.prepare(
        PrepareRequest {
            request_id: Uuid::new_v4(),
            session_id: &session,
            target: key.clone(),
            signal: Signal::Sigterm,
            payload_sha256: "x".into(),
            row: None,
            protected_pids: &[],
        },
        bench.now,
        &mut fx,
    );
    assert_eq!(fx.results[0].code, ResultCode::from(ErrorCode::Unavailable));
    assert!(actions.is_empty());
}

#[test]
fn intent_write_failure_never_sends() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    let (confirm_id, _) = bench.confirm(&token);
    let intents = bench.intents();
    let fx = bench.audit_completion(AuditCompletion::IntentDurable {
        request_id: intents[0].request_id,
        intent_id: intents[0].intent_id,
        result: Err(AuditFailure {
            code: ErrorCode::PersistFailed,
            message: "WAL limit".into(),
            commit_uncertain: true,
        }),
    });
    let result = result_for(&fx, confirm_id);
    assert_eq!(result.code, ResultCode::from(ErrorCode::PersistFailed));
    assert!(result.message.contains("commit_uncertain"));
    assert_eq!(bench.state_of(prepare_id), ActionState::Rejected);
    let ops = bench.actions.take();
    assert!(Bench::sends(&ops).is_empty());
    assert!(ops.iter().any(|op| matches!(op, ActionOp::Release { .. })));
    assert!(
        !bench
            .service
            .view_for(&bench.session, bench.now.boot_ns)
            .actions_available
    );
}

#[test]
fn a_refused_intent_is_audited_as_an_attempt_not_as_an_outcome_of_a_missing_intent() {
    let mut bench = bench();
    let (_, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    let (confirm_id, _) = bench.confirm(&token);
    let intents = bench.intents();
    bench.audit_completion(AuditCompletion::IntentDurable {
        request_id: intents[0].request_id,
        intent_id: intents[0].intent_id,
        result: Err(AuditFailure {
            code: ErrorCode::PersistFailed,
            message: "free disk below the low-water mark".into(),
            commit_uncertain: false,
        }),
    });
    let ops = bench.audit.take();
    assert!(
        !ops.iter().any(|op| matches!(op, AuditOp::AppendOutcome(_))),
        "no intent row exists for an outcome to close: {ops:?}"
    );
    let attempts: Vec<_> = ops
        .iter()
        .filter_map(|op| match op {
            AuditOp::AppendAttempt(attempt) => Some(attempt),
            _ => None,
        })
        .collect();
    assert_eq!(attempts.len(), 1, "{ops:?}");
    assert_eq!(attempts[0].request_id, confirm_id);
    assert_eq!(attempts[0].result_code, ErrorCode::PersistFailed.as_str());
    assert!(Bench::sends(&bench.actions.take()).is_empty());
}

#[test]
fn an_uncertain_intent_commit_is_closed_with_a_not_sent_outcome() {
    let mut bench = bench();
    let (_, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    bench.confirm(&token);
    let intents = bench.intents();
    bench.audit_completion(AuditCompletion::IntentDurable {
        request_id: intents[0].request_id,
        intent_id: intents[0].intent_id,
        result: Err(AuditFailure {
            code: ErrorCode::PersistFailed,
            message: "fsync failed after COMMIT".into(),
            commit_uncertain: true,
        }),
    });
    // The intent row may exist; the outcome closes it as not sent.
    let outcomes = bench.outcomes();
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0].intent_id, intents[0].intent_id);
    assert_eq!(outcomes[0].state, ActionState::NotSent);
}

#[test]
fn missing_send_report_is_unknown_and_never_retried() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    let (confirm_id, _) = bench.confirm(&token);
    let intents = bench.intents();
    bench.durable(&intents[0]);
    bench.actions.take();
    bench.advance_ms(12_000);
    let fx = bench.tick();
    let result = result_for(&fx, confirm_id);
    assert_eq!(result.code, ResultCode::from(ErrorCode::UnknownOutcome));
    assert_eq!(bench.state_of(prepare_id), ActionState::Unknown);
    for _ in 0..20 {
        bench.advance_ms(1_000);
        bench.tick();
    }
    assert!(
        Bench::sends(&bench.actions.take()).is_empty(),
        "never resent"
    );
    // The late report is recorded, still without any resend.
    bench.action_completion(ActionCompletion::Sent {
        request_id: confirm_id,
        intent_id: intents[0].intent_id,
        outcome: SignalOutcome::Sent,
        sent_boot_ns: bench.now.boot_ns,
    });
    assert_eq!(bench.state_of(prepare_id), ActionState::StillRunning);
    assert!(
        bench
            .outcomes()
            .iter()
            .any(|outcome| outcome.state == ActionState::SignalSent)
    );
}

#[test]
fn exit_not_observed_within_ten_seconds_is_still_running() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    let (confirm_id, _) = bench.confirm(&token);
    let intents = bench.intents();
    bench.durable(&intents[0]);
    bench.action_completion(ActionCompletion::Sent {
        request_id: confirm_id,
        intent_id: intents[0].intent_id,
        outcome: SignalOutcome::Sent,
        sent_boot_ns: bench.now.boot_ns,
    });
    bench.actions.take();
    let mut polls = 0;
    for _ in 0..50 {
        bench.advance_ms(250);
        bench.tick();
        for op in bench.actions.take() {
            if matches!(op, ActionOp::ObserveExit { .. }) {
                polls += 1;
                bench.action_completion(ActionCompletion::ExitObserved {
                    request_id: confirm_id,
                    observation: ExitObservation::Running,
                });
            }
        }
    }
    assert!((35..=41).contains(&polls), "polls {polls}");
    assert_eq!(bench.state_of(prepare_id), ActionState::StillRunning);
}

#[test]
fn signal_failures_are_not_sent_with_specific_codes() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    let (confirm_id, _) = bench.confirm(&token);
    let intents = bench.intents();
    bench.durable(&intents[0]);
    let fx = bench.action_completion(ActionCompletion::Sent {
        request_id: confirm_id,
        intent_id: intents[0].intent_id,
        outcome: SignalOutcome::Exited,
        sent_boot_ns: bench.now.boot_ns,
    });
    assert_eq!(
        result_for(&fx, confirm_id).code,
        ResultCode::from(ErrorCode::ProcessExited)
    );
    assert_eq!(bench.state_of(prepare_id), ActionState::NotSent);
    assert_eq!(
        signal_failure(&SignalOutcome::Changed).0,
        ErrorCode::ProcessIdentityChanged
    );
    assert_eq!(
        signal_failure(&SignalOutcome::Error { errno: 22 }).0,
        ErrorCode::InternalError
    );
}

#[test]
fn unknown_outcomes_from_recovery_are_surfaced_and_never_resent() {
    let mut bench = bench();
    let recovered = RecoveredIntent {
        request_id: Uuid::new_v4(),
        intent_id: Uuid::new_v4(),
        process_key: target(777),
        signal: Signal::Sigterm,
        previous_status: "intent_committed".into(),
    };
    bench.audit_completion(AuditCompletion::Recovered {
        result: Ok(vec![recovered.clone()]),
    });
    assert_eq!(bench.service.unknown_outcomes(), 1);
    let view = bench.service.view_for(&bench.session, bench.now.boot_ns);
    assert_eq!(view.unknown_outcomes, 1);
    let record = view
        .recent
        .iter()
        .find(|record| record.request_id == recovered.request_id)
        .unwrap();
    assert_eq!(record.state, ActionState::Unknown);
    assert!(record.result.contains("never retried"));
    for _ in 0..30 {
        bench.advance_ms(1_000);
        bench.tick();
    }
    assert!(Bench::sends(&bench.actions.take()).is_empty());
    // New actions still work but show the earlier unknown outcome warning.
    let (_, _token) = bench.prepared(Signal::Sigterm);
    let view = bench.service.view_for(&bench.session, bench.now.boot_ns);
    assert!(
        view.confirmation
            .as_ref()
            .unwrap()
            .warning
            .contains("1 earlier action(s) have unknown outcomes")
    );
}

#[test]
fn failed_recovery_blocks_actions_and_is_retried() {
    let audit = CapturingDispatch::new();
    let actions = CapturingDispatch::new();
    let mut service = ProcessService::new(
        ProcessPorts {
            audit: Box::new(audit.clone()),
            actions: Box::new(actions.clone()),
        },
        identity(EUID),
        &Config::defaults().unwrap(),
    );
    let mut now = Now {
        boot_ns: 10 * SECOND,
        utc_ns: 0,
    };
    let open = BTreeSet::new();
    service.start(now);
    audit.take();
    let mut fx = Effects::default();
    service.on_audit(
        AuditCompletion::Status(PersistenceStatus::healthy(PersistenceKind::Audit)),
        now,
        &open,
        &mut fx,
    );
    service.on_audit(
        AuditCompletion::Recovered {
            result: Err(AuditFailure {
                code: ErrorCode::PersistFailed,
                message: "locked".into(),
                commit_uncertain: false,
            }),
        },
        now,
        &open,
        &mut fx,
    );
    let session = SessionId::new("s1").unwrap();
    assert!(!service.view_for(&session, now.boot_ns).actions_available);
    now.boot_ns += 2 * SECOND;
    service.tick(now, &mut fx);
    let ops = audit.take();
    assert!(
        ops.iter()
            .any(|op| matches!(op, AuditOp::RecoverUnknown { .. }))
    );
    assert!(ops.iter().any(|op| matches!(op, AuditOp::Status)));
}

#[test]
fn confirmations_are_bound_to_their_session() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    let intruder = SessionId::new("s2").unwrap();
    let mut fx = Effects::default();
    let request_id = Uuid::new_v4();
    bench.service.confirm(
        TokenCommand {
            request_id,
            session_id: &intruder,
            token: &token,
            payload_sha256: "x".into(),
        },
        bench.now,
        &mut fx,
    );
    assert_eq!(
        result_for(&fx, request_id).code,
        ResultCode::from(ErrorCode::CapabilityDenied)
    );
    assert_eq!(
        bench.state_of(prepare_id),
        ActionState::Prepared,
        "untouched"
    );
    let mut fx = Effects::default();
    bench.service.cancel(
        TokenCommand {
            request_id: Uuid::new_v4(),
            session_id: &intruder,
            token: &token,
            payload_sha256: "x".into(),
        },
        bench.now,
        &mut fx,
    );
    assert_eq!(
        fx.results[0].code,
        ResultCode::from(ErrorCode::CapabilityDenied)
    );
    // A closed session cannot use its token any more.
    let mut fx = Effects::default();
    let session = bench.session.clone();
    bench.service.revoke_session(&session, bench.now, &mut fx);
    assert_eq!(bench.state_of(prepare_id), ActionState::Rejected);
    let (confirm_id, fx) = bench.confirm(&token);
    assert_eq!(
        result_for(&fx, confirm_id).code,
        ResultCode::from(ErrorCode::CapabilityDenied)
    );
}

#[test]
fn session_closing_before_the_receipt_prevents_the_send() {
    let mut bench = bench();
    let (_, token) = bench.prepared(Signal::Sigterm);
    bench.audit.take();
    let (confirm_id, _) = bench.confirm(&token);
    let intents = bench.intents();
    bench.open.clear();
    let fx = bench.durable(&intents[0]);
    assert_eq!(
        result_for(&fx, confirm_id).code,
        ResultCode::from(ErrorCode::Cancelled)
    );
    assert!(Bench::sends(&bench.actions.take()).is_empty());
}

#[test]
fn pending_confirmations_and_inflight_actions_are_bounded() {
    let mut config = Config::defaults().unwrap();
    config.actions.max_pending_confirmations = 2;
    let mut bench = bench_with(&config, EUID);
    for pid in [4242, 4243] {
        let key = target(pid);
        let (request_id, _) = bench.prepare_with(&key, Signal::Sigterm, EUID, &[]);
        bench.actions.take();
        bench.action_completion(ActionCompletion::Prepared {
            request_id,
            result: Ok(prepared_process(&key, u64::try_from(pid).unwrap())),
        });
    }
    let (request_id, fx) = bench.prepare_with(&target(4244), Signal::Sigterm, EUID, &[]);
    assert_eq!(
        result_for(&fx, request_id).code,
        ResultCode::from(ErrorCode::CapacityExceeded)
    );
    assert!(bench.actions.is_empty());
}

#[test]
fn worker_answers_are_checked_and_late_descriptors_released() {
    let mut bench = bench();
    let key = target(4242);
    // Identity mismatch: rejected and released.
    let (request_id, _) = bench.prepare_with(&key, Signal::Sigterm, EUID, &[]);
    bench.actions.take();
    let fx = bench.action_completion(ActionCompletion::Prepared {
        request_id,
        result: Ok(prepared_process(&target(4243), 3)),
    });
    assert_eq!(
        result_for(&fx, request_id).code,
        ResultCode::from(ErrorCode::ProcessIdentityChanged)
    );
    assert!(matches!(
        bench.actions.take()[..],
        [ActionOp::Release {
            handle: ActionHandle(3)
        }]
    ));
    // Worker error is passed through with its code.
    let (request_id, _) = bench.prepare_with(&key, Signal::Sigterm, EUID, &[]);
    bench.actions.take();
    let fx = bench.action_completion(ActionCompletion::Prepared {
        request_id,
        result: Err(ActionError::Exited),
    });
    assert_eq!(
        result_for(&fx, request_id).code,
        ResultCode::from(ErrorCode::ProcessExited)
    );
    // No answer within 2 s: TimedOut; the late descriptor is released.
    let (request_id, _) = bench.prepare_with(&key, Signal::Sigterm, EUID, &[]);
    bench.actions.take();
    bench.advance_ms(2_000);
    let fx = bench.tick();
    assert_eq!(
        result_for(&fx, request_id).code,
        ResultCode::from(ErrorCode::TimedOut)
    );
    bench.action_completion(ActionCompletion::Prepared {
        request_id,
        result: Ok(prepared_process(&key, 11)),
    });
    assert!(matches!(
        bench.actions.take()[..],
        [ActionOp::Release {
            handle: ActionHandle(11)
        }]
    ));
}

#[test]
fn busy_action_worker_at_send_time_means_not_sent() {
    let mut bench = bench();
    let (prepare_id, token) = bench.prepared(Signal::Sigterm);
    let (confirm_id, _) = bench.confirm(&token);
    let intents = bench.intents();
    bench.actions.set_busy(true);
    let fx = bench.durable(&intents[0]);
    assert_eq!(
        result_for(&fx, confirm_id).code,
        ResultCode::from(ErrorCode::Busy)
    );
    assert_eq!(bench.state_of(prepare_id), ActionState::NotSent);
    bench.actions.set_busy(false);
    bench.tick();
    let ops = bench.actions.take();
    assert!(Bench::sends(&ops).is_empty());
    assert!(
        ops.iter().any(|op| matches!(op, ActionOp::Release { .. })),
        "release retried"
    );
}

#[test]
fn shutdown_revokes_and_reports_unfinished_work() {
    let mut bench = bench();
    let (_, prepared_token) = bench.prepared(Signal::Sigterm);
    let key = target(6000);
    let (sending_prepare, _) = bench.prepare_with(&key, Signal::Sigterm, EUID, &[]);
    bench.actions.take();
    let fx = bench.action_completion(ActionCompletion::Prepared {
        request_id: sending_prepare,
        result: Ok(prepared_process(&key, 12)),
    });
    let sending_token = result_for(&fx, sending_prepare)
        .confirmation_token
        .clone()
        .unwrap();
    bench.audit.take();
    let (sending_confirm, _) = bench.confirm(&sending_token);
    let intents = bench.intents();
    bench.durable(&intents[0]);
    let mut fx = Effects::default();
    assert_eq!(bench.service.begin_shutdown(bench.now, &mut fx), 1);
    assert!(
        bench.service.awaiting_control(),
        "a send is still in flight"
    );
    let (confirm_id, fx) = bench.confirm(&prepared_token);
    assert_eq!(
        result_for(&fx, confirm_id).code,
        ResultCode::from(ErrorCode::Unavailable)
    );
    let mut fx = Effects::default();
    let unfinished = bench.service.finish_shutdown(bench.now, &mut fx);
    assert!(
        unfinished
            .iter()
            .any(|entry| entry.contains("signal outcome unknown"))
    );
    assert_eq!(
        result_for(&fx, sending_confirm).code,
        ResultCode::from(ErrorCode::UnknownOutcome)
    );
}
