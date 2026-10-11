//! AT10 (R10): authorization, stable identity and audit of process
//! termination.
//!
//! Success: a self-spawned same-UID disposable child, a held pidfd and a
//! valid confirmation give exactly one durable audit intent, then at most one
//! SIGTERM, with "signal sent" and "exit observed" reported separately.
//! Failure: root session, another user, another PID namespace, PID reuse,
//! expiry, read-only mode, no pidfd and no audit all send zero signals, and
//! no bare-PID path exists.

use std::os::unix::process::ExitStatusExt;

use dgxtop_core::action::{ActionState, Signal, SignalOutcome};
use dgxtop_core::commands::{CommandEnvelope, CommandStatus};
use dgxtop_core::errors::{ErrorCode, ResultCode};
use serde_json::json;
use uuid::Uuid;

use crate::child::{DisposableChild, ProcessFacts, foreign_user_process, pid_namespace};
use crate::expect::{
    assert_nothing_sent, assert_refusal_audited, assert_rejected, assert_signal_sent,
    assert_single_sigterm_through_held_pidfd,
};
use crate::journal::Step;
use crate::stack::{ActionStack, AuditBackend, EXIT_WAIT, Kernel, StackOptions};

/// A PID namespace this test process is not in.
const FOREIGN_PID_NAMESPACE: &str = "pid:[4026539999]";

fn ready_stack(options: StackOptions) -> ActionStack {
    let stack = ActionStack::start(options);
    stack.await_available();
    stack
}

#[test]
fn at10_confirmed_sigterm_reaches_only_the_disposable_child_after_one_durable_intent() {
    let mut child = DisposableChild::sleeping();
    let stack = ready_stack(StackOptions::default());
    let target = stack.publish(&child);

    // Prepare: identity re-verified on a held pidfd, a token, nothing sent.
    let prepared = stack.prepared(&target);
    let shown = stack
        .session()
        .await_actions("the pending confirmation", |view| {
            view.confirmation.is_some()
        });
    let confirmation = shown.confirmation.expect("confirmation shown");
    assert_eq!(
        confirmation.target, target,
        "the selected instance, not a row"
    );
    assert_eq!(confirmation.uid, stack.caller_euid());
    assert_eq!(confirmation.basename, "sleep");
    assert!(confirmation.warning.contains("cannot be undone"));
    assert!(child.is_running(), "preparing never signals");
    let held = stack.steps().pidfds_opened_for(child.pid());
    assert_eq!(
        held.len(),
        1,
        "prepare opens exactly one pidfd and holds it"
    );

    // Confirm: the confirmation itself answers "signal sent" (exit pending).
    let confirm_id = stack.confirm(&prepared.token);
    assert_signal_sent(&stack.result(confirm_id));
    let status = child.wait_for_exit(EXIT_WAIT);
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    // "Exit observed" is a separate, later report.
    let exited = stack.await_record(prepared.request_id, ActionState::ExitObserved);
    assert_eq!(exited.signal, Signal::Sigterm);

    stack.settle();
    let steps = stack.steps();
    assert_eq!(steps.intents_requested(), vec![confirm_id]);
    let committed = steps.intents_committed();
    assert_eq!(committed.len(), 1, "exactly one durable intent: {steps:#?}");
    let (_, intent_id) = committed[0];
    assert_eq!(steps.sends_dispatched(), 1);
    assert_single_sigterm_through_held_pidfd(&steps, child.pid());
    assert_eq!(
        steps.signal_calls()[0].fd,
        held[0],
        "the descriptor held since prepare"
    );
    let position = |what: fn(&Step) -> bool| steps.index_of(what).expect("step journaled");
    let durable = position(|step| matches!(step, Step::IntentCommitted { .. }));
    let requested = position(|step| matches!(step, Step::SendDispatched { .. }));
    let signalled = position(|step| matches!(step, Step::SignalSyscall { .. }));
    assert!(
        durable < requested && requested < signalled,
        "the send must follow the durable intent: {steps:#?}"
    );
    assert_eq!(
        steps.outcomes_for(confirm_id),
        [ActionState::SignalSent, ActionState::ExitObserved],
        "\"sent\" and \"exit observed\" are separate outcome rows"
    );

    let mut trail = stack.stop().audit_trail();
    let intent = trail.record(confirm_id).expect("the intent row exists");
    assert_eq!(intent.intent_id, Some(intent_id));
    assert_eq!(intent.state, ActionState::ExitObserved);
    assert_eq!(intent.result_code, "Ok");
    let prepare_row = trail
        .record(prepared.request_id)
        .expect("the prepare is audited");
    assert_eq!(
        prepare_row.intent_id, None,
        "a prepare never writes an intent"
    );
    assert!(
        trail.recover_unresolved().is_empty(),
        "no intent may be left without an outcome"
    );
}

#[test]
fn at10_root_session_sends_nothing() {
    let mut child = DisposableChild::sleeping();
    let stack = ActionStack::start(StackOptions {
        caller_euid: Some(0),
        ..StackOptions::default()
    });
    stack.await_unavailable("root sessions");
    let target = stack.publish(&child);
    let prepare_id = stack.prepare(&target, Signal::Sigterm);
    let refused = stack.result(prepare_id);
    assert_rejected(&refused, ErrorCode::CapabilityDenied);
    assert!(refused.message.contains("root"), "{refused:?}");
    assert_nothing_sent(&stack, &mut [&mut child]);
    assert_eq!(
        stack.steps().prepares_dispatched(),
        0,
        "refused before the worker"
    );
    let mut trail = stack.stop().audit_trail();
    assert_refusal_audited(&mut trail, prepare_id, ErrorCode::CapabilityDenied);
}

#[test]
fn at10_read_only_or_disabled_actions_send_nothing() {
    let cases = [
        (
            StackOptions {
                read_only: true,
                ..StackOptions::default()
            },
            "read-only",
        ),
        (
            StackOptions {
                actions_disabled: true,
                ..StackOptions::default()
            },
            "disabled",
        ),
    ];
    for (options, reason) in cases {
        let mut child = DisposableChild::sleeping();
        let stack = ActionStack::start(options);
        stack.await_unavailable(reason);
        let target = stack.publish(&child);
        let prepare_id = stack.prepare(&target, Signal::Sigterm);
        assert_rejected(&stack.result(prepare_id), ErrorCode::CapabilityDenied);
        assert_nothing_sent(&stack, &mut [&mut child]);
        assert_eq!(stack.steps().prepares_dispatched(), 0, "{reason}");
        let mut trail = stack.stop().audit_trail();
        assert_refusal_audited(&mut trail, prepare_id, ErrorCode::CapabilityDenied);
    }
}

#[test]
fn at10_target_listed_under_another_uid_sends_nothing() {
    let mut child = DisposableChild::sleeping();
    let stack = ready_stack(StackOptions::default());
    let facts = ProcessFacts::read(child.pid()).expect("child facts");
    let target = stack.key_for(&facts, &pid_namespace(child.pid()));
    let another_user = facts.real_uid.checked_add(1).unwrap_or(0);
    stack.publish_row(&target, Some(another_user));
    let prepare_id = stack.prepare(&target, Signal::Sigterm);
    let refused = stack.result(prepare_id);
    assert_rejected(&refused, ErrorCode::CapabilityDenied);
    assert!(refused.message.contains("another user"), "{refused:?}");
    assert_nothing_sent(&stack, &mut [&mut child]);
    assert_eq!(
        stack.steps().prepares_dispatched(),
        0,
        "refused before the worker"
    );
    let mut trail = stack.stop().audit_trail();
    assert_refusal_audited(&mut trail, prepare_id, ErrorCode::CapabilityDenied);
}

#[test]
fn at10_process_of_another_user_is_refused_by_the_kernel_identity_check() {
    let stack = ready_stack(StackOptions::default());
    let foreign = foreign_user_process(stack.caller_euid())
        .expect("this test needs a visible process of another user, e.g. a root daemon");
    // Only the refusal path is exercised: no confirmation is ever submitted.
    let target = stack.key_for(&foreign, stack.pid_namespace());
    stack.publish_row(&target, None);
    let prepare_id = stack.prepare(&target, Signal::Sigterm);
    let refused = stack.result(prepare_id);
    assert_rejected(&refused, ErrorCode::CapabilityDenied);
    assert!(refused.message.contains("another user"), "{refused:?}");
    stack.settle();
    let steps = stack.steps();
    assert_eq!(
        steps.prepare_errors_for(foreign.pid),
        vec![Some(ErrorCode::CapabilityDenied)],
        "the uid is checked against /proc while the pidfd is held: {steps:#?}"
    );
    assert_eq!(steps.pidfds_opened_for(foreign.pid).len(), 1);
    assert_eq!(steps.sends_dispatched(), 0);
    assert!(
        steps.signal_calls().is_empty(),
        "no signal reached the kernel"
    );
    let mut trail = stack.stop().audit_trail();
    assert_refusal_audited(&mut trail, prepare_id, ErrorCode::CapabilityDenied);
}

#[test]
fn at10_target_in_another_pid_namespace_sends_nothing() {
    let mut child = DisposableChild::sleeping();
    let stack = ready_stack(StackOptions::default());
    assert_ne!(pid_namespace(child.pid()), FOREIGN_PID_NAMESPACE);
    let facts = ProcessFacts::read(child.pid()).expect("child facts");
    let target = stack.key_for(&facts, FOREIGN_PID_NAMESPACE);
    stack.publish_row(&target, Some(facts.real_uid));
    let prepare_id = stack.prepare(&target, Signal::Sigterm);
    assert_rejected(&stack.result(prepare_id), ErrorCode::NamespaceMismatch);
    assert_nothing_sent(&stack, &mut [&mut child]);
    assert_eq!(
        stack.steps().prepares_dispatched(),
        0,
        "refused before the worker"
    );
    let mut trail = stack.stop().audit_trail();
    assert_refusal_audited(&mut trail, prepare_id, ErrorCode::NamespaceMismatch);
}

#[test]
fn at10_reused_pid_with_another_start_time_sends_nothing() {
    let mut child = DisposableChild::sleeping();
    let stack = ready_stack(StackOptions::default());
    let facts = ProcessFacts::read(child.pid()).expect("child facts");
    // The table still names the PID's previous owner: same PID, earlier start.
    let previous_owner = ProcessFacts {
        start_time_ticks: facts
            .start_time_ticks
            .checked_sub(1)
            .expect("started after boot"),
        ..facts.clone()
    };
    let stale = stack.key_for(&previous_owner, &pid_namespace(child.pid()));
    stack.publish_row(&stale, Some(facts.real_uid));
    let prepare_id = stack.prepare(&stale, Signal::Sigterm);
    assert_rejected(&stack.result(prepare_id), ErrorCode::ProcessIdentityChanged);
    assert_nothing_sent(&stack, &mut [&mut child]);
    let steps = stack.steps();
    assert_eq!(
        steps.prepare_errors_for(child.pid()),
        vec![Some(ErrorCode::ProcessIdentityChanged)],
        "the start time is re-read after the pidfd was opened: {steps:#?}"
    );
    assert_eq!(steps.pidfds_opened_for(child.pid()).len(), 1);
    let mut trail = stack.stop().audit_trail();
    assert_refusal_audited(&mut trail, prepare_id, ErrorCode::ProcessIdentityChanged);
}

#[test]
fn at10_target_gone_after_prepare_is_never_signalled_through_its_pidfd() {
    let mut child = DisposableChild::sleeping();
    let stack = ready_stack(StackOptions::default());
    let target = stack.publish(&child);
    let prepared = stack.prepared(&target);
    // The selected instance disappears; its PID may now be reused.
    child.end_from_the_test();
    let confirm_id = stack.confirm(&prepared.token);
    let refused = stack.result(confirm_id);
    assert_eq!(refused.status, CommandStatus::Rejected, "{refused:?}");
    let gone = [
        ResultCode::from(ErrorCode::ProcessExited),
        ResultCode::from(ErrorCode::ProcessIdentityChanged),
    ];
    assert!(gone.contains(&refused.code), "{refused:?}");
    stack.settle();
    let steps = stack.steps();
    assert_eq!(
        steps.intents_committed().len(),
        1,
        "the intent still came first"
    );
    assert_eq!(steps.sends_dispatched(), 1);
    assert!(
        matches!(
            steps.send_outcomes().as_slice(),
            [SignalOutcome::Exited | SignalOutcome::Changed]
        ),
        "the adapter re-verified and refused: {steps:#?}"
    );
    assert!(
        steps.signal_calls().is_empty(),
        "nothing reached the kernel"
    );
    assert_eq!(steps.outcomes_for(confirm_id), [ActionState::NotSent]);
    let mut trail = stack.stop().audit_trail();
    let record = trail.record(confirm_id).expect("the intent row");
    assert_eq!(
        record.state,
        ActionState::Rejected,
        "not_sent is stored as rejected"
    );
    assert_eq!(record.result_code, refused.code.as_str());
}

#[test]
fn at10_expired_confirmation_sends_nothing() {
    let mut child = DisposableChild::sleeping();
    let stack = ready_stack(StackOptions::default());
    let target = stack.publish(&child);
    let prepared = stack.prepared(&target);
    stack
        .clock()
        .advance_to(prepared.expires_boot_ns + 1_000_000);
    let confirm_id = stack.confirm(&prepared.token);
    assert_rejected(&stack.result(confirm_id), ErrorCode::ConfirmationExpired);
    stack.await_record(prepared.request_id, ActionState::Expired);
    assert_nothing_sent(&stack, &mut [&mut child]);
    assert!(
        stack.steps().intents_requested().is_empty(),
        "an expired token never reaches the audit"
    );
    let mut trail = stack.stop().audit_trail();
    // The refused confirmation is audited under its own request id, as an
    // attempt without an intent.
    let refusal = trail
        .record(confirm_id)
        .expect("the refused confirmation is audited");
    assert_eq!(refusal.intent_id, None);
    assert_eq!(refusal.result_code, ErrorCode::ConfirmationExpired.as_str());
}

#[test]
fn at10_unavailable_audit_store_sends_nothing() {
    let mut child = DisposableChild::sleeping();
    let stack = ActionStack::start(StackOptions {
        audit: AuditBackend::Unavailable,
        ..StackOptions::default()
    });
    stack.await_unavailable("audit store unavailable");
    let target = stack.publish(&child);
    let prepare_id = stack.prepare(&target, Signal::Sigterm);
    assert_rejected(&stack.result(prepare_id), ErrorCode::Unavailable);
    assert_nothing_sent(&stack, &mut [&mut child]);
    let steps = stack.steps();
    assert_eq!(steps.prepares_dispatched(), 0, "refused before the worker");
    assert!(steps.intents_requested().is_empty());
    stack.stop();
}

#[test]
fn at10_audit_refusing_the_intent_sends_nothing() {
    let mut child = DisposableChild::sleeping();
    let stack = ready_stack(StackOptions::default());
    let target = stack.publish(&child);
    let prepared = stack.prepared(&target);
    // The prepare's audit row is written before the disk "fills up".
    stack.settle();
    stack.faults().disk.fill_up();
    let confirm_id = stack.confirm(&prepared.token);
    let refused = stack.result(confirm_id);
    assert_rejected(&refused, ErrorCode::PersistFailed);
    assert!(refused.message.contains("not durable"), "{refused:?}");
    assert_nothing_sent(&stack, &mut [&mut child]);
    let steps = stack.steps();
    assert!(steps.contains(&Step::IntentRefused {
        request_id: confirm_id,
        code: ErrorCode::CapacityExceeded,
    }));
    assert!(
        steps.intents_committed().is_empty(),
        "no durable receipt was issued"
    );
    let mut trail = stack.stop().audit_trail();
    assert!(
        trail
            .record(confirm_id)
            .and_then(|row| row.intent_id)
            .is_none()
    );
}

#[test]
fn at10_without_pidfd_support_nothing_is_sent_and_there_is_no_pid_fallback() {
    let mut child = DisposableChild::sleeping();
    let stack = ready_stack(StackOptions {
        kernel: Kernel::WithoutPidfd,
        ..StackOptions::default()
    });
    let target = stack.publish(&child);
    let prepare_id = stack.prepare(&target, Signal::Sigterm);
    let refused = stack.result(prepare_id);
    assert_rejected(&refused, ErrorCode::Unsupported);
    assert!(refused.message.contains("pidfd"), "{refused:?}");
    assert_nothing_sent(&stack, &mut [&mut child]);
    let steps = stack.steps();
    assert!(
        steps.contains(&Step::PidfdOpened {
            pid: child.pid(),
            fd: None,
        }),
        "the adapter insisted on a pidfd: {steps:#?}"
    );
    assert_eq!(
        steps.prepare_errors_for(child.pid()),
        vec![Some(ErrorCode::Unsupported)]
    );
    let mut trail = stack.stop().audit_trail();
    assert_refusal_audited(&mut trail, prepare_id, ErrorCode::Unsupported);
}

#[test]
fn at10_bare_pid_targets_cannot_be_expressed_as_commands() {
    let prepare = |target: serde_json::Value| {
        json!({
            "schema_version": 2,
            "request_id": Uuid::new_v4().to_string(),
            "session_id": "session-test",
            "body": {"type": "PrepareSignal", "target": target, "signal": "SIGTERM"}
        })
    };
    let key = json!({
        "host_id": "host-test",
        "boot_id": "boot-test",
        "pid_namespace": "pid:[4026531836]",
        "pid": 4242,
        "start_time_ticks": "90100"
    });
    assert!(
        serde_json::from_value::<CommandEnvelope>(prepare(key.clone())).is_ok(),
        "a full process key decodes"
    );
    let mut without_start_time = key;
    without_start_time
        .as_object_mut()
        .expect("key object")
        .remove("start_time_ticks");
    for bare in [json!({"pid": 4242}), json!(4242), without_start_time] {
        assert!(
            serde_json::from_value::<CommandEnvelope>(prepare(bare.clone())).is_err(),
            "a target without a full identity must be rejected: {bare}"
        );
    }
}
