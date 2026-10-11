//! Assertions shared by the AT10 and AT32 scenarios.

use std::os::unix::process::ExitStatusExt;

use dgxtop_core::action::ActionState;
use dgxtop_core::commands::{CommandResult, CommandStatus};
use dgxtop_core::errors::{ErrorCode, ResultCode};
use uuid::Uuid;

use crate::audit_trail::AuditTrail;
use crate::child::DisposableChild;
use crate::journal::{SignalCall, Steps};
use crate::stack::{ActionStack, EXIT_WAIT};

pub fn assert_rejected(result: &CommandResult, code: ErrorCode) {
    assert_eq!(
        result.status,
        CommandStatus::Rejected,
        "expected a {code:?} rejection: {result:?}"
    );
    assert_eq!(result.code, ResultCode::from(code), "{result:?}");
    assert!(
        result.confirmation_token.is_none(),
        "a rejection never carries a token: {result:?}"
    );
}

/// "Signal sent" reported on its own: the process has not been seen to exit.
pub fn assert_signal_sent(result: &CommandResult) {
    assert_eq!(result.status, CommandStatus::Completed, "{result:?}");
    assert_eq!(result.code, ResultCode::OK, "{result:?}");
    assert_eq!(
        result.action_state,
        Some(ActionState::SignalSent),
        "{result:?}"
    );
    assert!(
        result.message.contains("exit not yet observed"),
        "{result:?}"
    );
}

/// Zero signals: no `ActionOp::Send` ever reached the action worker, no
/// `pidfd_send_signal` reached the kernel, and every child still runs.
pub fn assert_nothing_sent(stack: &ActionStack, children: &mut [&mut DisposableChild]) {
    stack.settle();
    let steps = stack.steps();
    assert_eq!(
        steps.sends_dispatched(),
        0,
        "no send may even be requested: {steps:#?}"
    );
    assert!(
        steps.signal_calls().is_empty(),
        "no signal may reach the kernel: {steps:#?}"
    );
    for child in children.iter_mut() {
        assert!(
            child.is_running(),
            "disposable child {} must still be alive",
            child.pid()
        );
    }
}

/// The child died of the one SIGTERM the stack ever issued, and that signal
/// went through a pidfd opened for the child at prepare time.
pub fn assert_one_sigterm_to(stack: &ActionStack, child: &mut DisposableChild) {
    let status = child.wait_for_exit(EXIT_WAIT);
    assert_eq!(
        status.signal(),
        Some(libc::SIGTERM),
        "the child must die of SIGTERM: {status:?}"
    );
    stack.settle();
    assert_single_sigterm_through_held_pidfd(&stack.steps(), child.pid());
}

/// Exactly one `pidfd_send_signal`: SIGTERM, on a descriptor `pidfd_open`
/// returned for `pid`, whose kernel-reported target is `pid`.
pub fn assert_single_sigterm_through_held_pidfd(steps: &Steps, pid: i32) {
    let calls = steps.signal_calls();
    let [call] = calls.as_slice() else {
        panic!("exactly one signal system call expected: {steps:#?}");
    };
    assert_eq!(
        *call,
        SignalCall {
            fd: call.fd,
            target_pid: Some(i64::from(pid)),
            signal: libc::SIGTERM,
        },
        "the signal must be SIGTERM to the disposable child only"
    );
    assert!(
        steps.pidfds_opened_for(pid).contains(&call.fd),
        "the signal must use the pidfd held since prepare, never a re-opened PID: {steps:#?}"
    );
}

/// The audit keeps the refusal as an attempt row, never as an intent.
pub fn assert_refusal_audited(trail: &mut AuditTrail, request_id: Uuid, code: ErrorCode) {
    let audited = trail
        .record(request_id)
        .unwrap_or_else(|| panic!("refusal {request_id} is audited"));
    assert_eq!(audited.intent_id, None, "a refusal never writes an intent");
    assert_eq!(audited.state, ActionState::Rejected, "{audited:?}");
    assert_eq!(audited.result_code, code.as_str(), "{audited:?}");
}
