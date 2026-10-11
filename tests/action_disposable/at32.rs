//! AT32 (R32): late audit results, cancellation and at-most-once actions.
//!
//! The confirmation token is single-use; send and cancel are linearized by
//! the control thread; the send happens only after a valid durable audit
//! ACK and a re-check of TTL and policy; a late ACK, a cancel, a policy
//! change or a crash and restart never cause a deferred send; an unknown
//! outcome is never retried; "sent" never waits for the process to exit.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use dgxtop_core::action::{ActionState, ConfirmationToken};
use dgxtop_core::commands::{CommandBody, CommandEnvelope, CommandResult, CommandStatus};
use dgxtop_core::diagnostics::names;
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::ports::SessionPort;
use uuid::Uuid;

use crate::child::DisposableChild;
use crate::expect::{
    assert_nothing_sent, assert_one_sigterm_to, assert_rejected, assert_signal_sent,
    assert_single_sigterm_through_held_pidfd, kinds,
};
use crate::stack::{ActionStack, Prepared, SessionDriver, StackOptions, StateDir};

/// The Manager watches for exit this long after "sent".
const EXIT_WATCH: Duration = Duration::from_secs(10);

/// A ready stack with one disposable child prepared for SIGTERM.
fn prepared_child(options: StackOptions) -> (ActionStack, DisposableChild, Prepared) {
    let child = DisposableChild::sleeping();
    let stack = ActionStack::start(options);
    stack.await_audit_ready();
    let target = stack.publish(&child);
    let prepared = stack.prepared(&target);
    (stack, child, prepared)
}

fn other_token() -> ConfirmationToken {
    ConfirmationToken::new("f".repeat(64)).expect("valid token syntax")
}

/// `count` threads submit the same confirmation at once.
fn confirm_concurrently(
    session: &SessionDriver,
    token: &ConfirmationToken,
    count: usize,
) -> Vec<CommandEnvelope> {
    let start = Arc::new(Barrier::new(count));
    let racers: Vec<_> = (0..count)
        .map(|_| {
            let handle = session.handle();
            let envelope = session.envelope(CommandBody::ConfirmSignal {
                confirmation_token: token.clone(),
            });
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                handle
                    .submit(envelope.clone())
                    .expect("a confirmation is accepted for processing");
                envelope
            })
        })
        .collect();
    racers
        .into_iter()
        .map(|racer| racer.join().expect("racer thread"))
        .collect()
}

#[test]
fn at32_token_is_single_use_across_sessions_concurrency_and_replays() {
    let (stack, mut child, prepared) = prepared_child(StackOptions {
        bystander_session: true,
        ..StackOptions::default()
    });

    // Bound to its session: another session's confirmation does not consume it.
    let foreign = stack.bystander().submit(CommandBody::ConfirmSignal {
        confirmation_token: prepared.token.clone(),
    });
    assert_rejected(
        &stack.bystander().result(foreign),
        ErrorCode::CapabilityDenied,
    );

    // Ten simultaneous confirmations: exactly one wins the token.
    let racers = confirm_concurrently(stack.session(), &prepared.token, 10);
    let results: Vec<CommandResult> = racers
        .iter()
        .map(|envelope| stack.result(envelope.request_id))
        .collect();
    let (winners, losers): (Vec<&CommandResult>, Vec<&CommandResult>) = results
        .iter()
        .partition(|result| result.status == CommandStatus::Completed);
    let [winner] = winners.as_slice() else {
        panic!("exactly one confirmation must win: {results:#?}");
    };
    assert_signal_sent(winner);
    for loser in &losers {
        assert_rejected(loser, ErrorCode::ConfirmationUsed);
    }
    assert_one_sigterm_to(&stack, &mut child);

    // Same request id and payload: the first result again. Other payload: Conflict.
    let winning = racers
        .iter()
        .find(|envelope| envelope.request_id == winner.request_id)
        .expect("winning envelope");
    stack.session().submit_envelope(winning);
    assert_eq!(&stack.session().nth_result(winning.request_id, 1), *winner);
    let mut altered = winning.clone();
    altered.body = CommandBody::ConfirmSignal {
        confirmation_token: other_token(),
    };
    stack.session().submit_envelope(&altered);
    assert_rejected(
        &stack.session().nth_result(winning.request_id, 2),
        ErrorCode::Conflict,
    );

    stack.settle();
    let steps = stack.steps();
    assert_eq!(steps.intents_requested(), vec![winning.request_id]);
    assert_eq!(steps.sends_dispatched(), 1);
    assert_single_sigterm_through_held_pidfd(&steps, child.pid());
    let mut trail = stack.stop().audit_trail();
    let with_intent: Vec<Uuid> = racers
        .iter()
        .map(|envelope| envelope.request_id)
        .filter(|request_id| {
            trail
                .record(*request_id)
                .and_then(|row| row.intent_id)
                .is_some()
        })
        .collect();
    assert_eq!(with_intent, vec![winning.request_id], "one durable intent");
}

#[test]
fn at32_crash_after_a_durable_intent_is_unknown_after_restart_and_never_resent() {
    let mut child = DisposableChild::sleeping();
    let state = StateDir::create("restart");

    // Run 1: the process dies right after the intent's COMMIT.
    let first = ActionStack::start_in(Arc::clone(&state), StackOptions::default());
    first.await_audit_ready();
    let target = first.publish(&child);
    let prepared = first.prepared(&target);
    first.faults().audit.crash_after_next_intent_commit();
    let lost_confirm = first.confirm(&prepared.token);
    let refused = first.result(lost_confirm);
    assert_rejected(&refused, ErrorCode::PersistFailed);
    assert!(refused.message.contains("commit_uncertain"), "{refused:?}");
    assert_nothing_sent(&first, &mut [&mut child]);
    let run_one = first.steps();
    assert_eq!(
        run_one.intents_committed().len(),
        1,
        "the intent reached disk"
    );
    assert!(
        run_one.outcomes_for(lost_confirm).is_empty(),
        "nothing after the COMMIT did"
    );
    let stopped = first.stop();
    let dangling = stopped
        .audit_trail()
        .record(lost_confirm)
        .expect("the committed intent survived the crash");
    assert_eq!(
        dangling.state,
        ActionState::IntentCommitted,
        "no outcome before the crash"
    );

    // Run 2: recovered as unknown, shown, and never resent.
    let second = ActionStack::start_in(stopped.state(), StackOptions::default());
    second.await_audit_ready();
    second
        .session()
        .await_actions("the recovered unknown outcome", |view| {
            view.unknown_outcomes == 1
        });
    let recovered = second
        .steps()
        .recovered_intents()
        .expect("the recovery succeeded");
    assert_eq!(recovered.len(), 1, "{recovered:?}");
    assert_eq!(recovered[0].request_id, lost_confirm);
    assert_eq!(recovered[0].process_key, target);
    let recovery_events = second.action_events(&[lost_confirm]);
    assert_eq!(kinds(&recovery_events), ["ActionOutcomeUnknown"]);
    assert!(
        recovery_events[0].reason.contains("never retried"),
        "{recovery_events:?}"
    );
    assert_eq!(
        second
            .diagnostics()
            .counter(names::ACTION_UNKNOWN_TOTAL, "recovered"),
        1
    );
    let stale = second.confirm(&prepared.token);
    assert_rejected(&second.result(stale), ErrorCode::ConfirmationExpired);
    assert_nothing_sent(&second, &mut [&mut child]);

    // Acting again takes a new preparation whose confirmation warns about it.
    let fresh = second.prepared(&second.publish(&child));
    let shown = second
        .session()
        .await_actions("a warning about the unknown outcome", |view| {
            view.confirmation.as_ref().is_some_and(|confirmation| {
                confirmation
                    .warning
                    .contains("1 earlier action(s) have unknown outcomes")
            })
        });
    assert!(shown.confirmation.is_some());
    let fresh_confirm = second.confirm(&fresh.token);
    assert_signal_sent(&second.result(fresh_confirm));
    assert_one_sigterm_to(&second, &mut child);
    second.await_outcome(fresh_confirm, ActionState::ExitObserved);
    let mut trail = second.stop().audit_trail();
    let lost = trail.record(lost_confirm).expect("the lost intent");
    assert_eq!(lost.state, ActionState::Unknown);
    assert_eq!(lost.result_code, ErrorCode::UnknownOutcome.as_str());
    let acted = trail.record(fresh_confirm).expect("the new intent");
    assert_eq!(acted.state, ActionState::ExitObserved);
}

#[test]
fn at32_signal_sent_is_reported_without_waiting_for_exit() {
    let mut child = DisposableChild::ignoring_sigterm();
    let stack = ActionStack::start(StackOptions::default());
    stack.await_audit_ready();
    let prepared = stack.prepared(&stack.publish(&child));

    let confirm_id = stack.confirm(&prepared.token);
    assert_signal_sent(&stack.result(confirm_id));
    assert!(
        child.is_running(),
        "\"sent\" is reported while the process lives"
    );
    stack.await_outcome(confirm_id, ActionState::SignalSent);

    // Exit is watched for at most 10 s, then reported apart as still running.
    stack
        .clock()
        .advance(EXIT_WATCH + Duration::from_millis(500));
    let concluded = stack.await_outcome(confirm_id, ActionState::StillRunning);
    assert!(concluded.contains("still running"), "{concluded:?}");
    assert!(child.is_running());
    stack.settle();
    let steps = stack.steps();
    assert_single_sigterm_through_held_pidfd(&steps, child.pid());
    assert_eq!(
        steps.outcomes_for(confirm_id),
        [ActionState::SignalSent, ActionState::StillRunning],
        "\"sent\" and \"still running\" are separate outcome rows"
    );
    assert_eq!(
        kinds(&stack.action_events(&[prepared.request_id, confirm_id])),
        [
            "ActionPrepared",
            "ActionIntentCommitted",
            "ActionSignalSent"
        ]
    );
    let mut trail = stack.stop().audit_trail();
    let intent = trail.record(confirm_id).expect("the intent");
    assert_eq!(intent.state, ActionState::StillRunning);
}
