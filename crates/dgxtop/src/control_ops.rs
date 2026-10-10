//! Execution of the Manager's audit and process-action operations on their
//! worker threads. Shared by the composition root and the disposable-child
//! suite, so the suite exercises the production code path (including the
//! last deadline check before a signal is sent).

use dgxtop_core::action::{ActionCompletion, ActionOp, AuditCompletion, AuditOp, SignalOutcome};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::ports::{AuditRepository, ProcessActionPort};

/// Runs one audit operation against the repository.
pub fn run_audit_op(repository: &mut dyn AuditRepository, op: AuditOp) -> AuditCompletion {
    match op {
        AuditOp::AppendIntent(intent) => AuditCompletion::IntentDurable {
            request_id: intent.request_id,
            intent_id: intent.intent_id,
            result: repository.append_intent(&intent),
        },
        AuditOp::AppendOutcome(outcome) => AuditCompletion::OutcomeRecorded {
            request_id: outcome.request_id,
            result: repository.append_outcome(&outcome),
        },
        AuditOp::AppendAttempt(attempt) => AuditCompletion::AttemptRecorded {
            request_id: attempt.request_id,
            result: repository.append_attempt(&attempt),
        },
        AuditOp::RecoverUnknown {
            now_boot_ns,
            now_utc,
        } => AuditCompletion::Recovered {
            result: repository.recover_unknown(now_boot_ns, &now_utc),
        },
        AuditOp::Lookup { request_id } => AuditCompletion::Lookup {
            request_id,
            result: repository.lookup(request_id),
        },
        AuditOp::Status => AuditCompletion::Status(repository.status()),
    }
}

/// Runs one process-action operation. A send is refused when the
/// confirmation deadline passed while the operation waited in the queue, or
/// when `halted` names a fail-stop (no signal with unknown state).
pub fn run_action_op(
    port: &mut dyn ProcessActionPort,
    op: ActionOp,
    clock: &dyn TimeProvider,
    halted: Option<&str>,
) -> ActionCompletion {
    match op {
        ActionOp::Prepare {
            request_id,
            key,
            signal,
            context,
        } => ActionCompletion::Prepared {
            request_id,
            result: port.prepare(&key, signal, &context),
        },
        ActionOp::Send {
            request_id,
            intent_id,
            handle,
            signal,
            receipt,
            send_deadline_boot_ns,
        } => {
            // Re-check fail-stop and the token deadline right before the
            // single send.
            let outcome = if let Some(reason) = halted {
                SignalOutcome::NotSent {
                    reason: format!("fail-stop ({reason}); signal not sent"),
                }
            } else if clock.now_boot_ns() >= send_deadline_boot_ns {
                SignalOutcome::NotSent {
                    reason: "confirmation deadline passed before sending".to_owned(),
                }
            } else {
                port.send(handle, signal, &receipt)
            };
            ActionCompletion::Sent {
                request_id,
                intent_id,
                outcome,
                sent_boot_ns: clock.now_boot_ns(),
            }
        }
        ActionOp::ObserveExit { request_id, handle } => ActionCompletion::ExitObserved {
            request_id,
            observation: port.observe_exit(handle),
        },
        ActionOp::Release { handle } => {
            port.release(handle);
            ActionCompletion::Released { handle }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::action::{
        ActionError, ActionHandle, CapabilityContext, DurableReceipt, ExitObservation,
        PreparedProcess, Signal,
    };
    use dgxtop_core::identity::ProcessKey;
    use dgxtop_core::testing::FakeClock;
    use uuid::Uuid;

    /// Counts sends; every other operation is unused by these tests.
    #[derive(Default)]
    struct CountingPort {
        sends: usize,
    }

    impl ProcessActionPort for CountingPort {
        fn prepare(
            &mut self,
            _key: &ProcessKey,
            _signal: Signal,
            _context: &CapabilityContext,
        ) -> Result<PreparedProcess, ActionError> {
            unreachable!("not exercised")
        }

        fn send(
            &mut self,
            _handle: ActionHandle,
            _signal: Signal,
            _receipt: &DurableReceipt,
        ) -> SignalOutcome {
            self.sends += 1;
            SignalOutcome::Sent
        }

        fn observe_exit(&mut self, _handle: ActionHandle) -> ExitObservation {
            ExitObservation::Running
        }

        fn release(&mut self, _handle: ActionHandle) {}
    }

    fn send_op(send_deadline_boot_ns: u64) -> ActionOp {
        let request_id = Uuid::new_v4();
        let intent_id = Uuid::new_v4();
        ActionOp::Send {
            request_id,
            intent_id,
            handle: ActionHandle(1),
            signal: Signal::Sigterm,
            receipt: DurableReceipt {
                request_id,
                intent_id,
                committed_boot_ns: 1,
            },
            send_deadline_boot_ns,
        }
    }

    fn outcome_of(completion: ActionCompletion) -> SignalOutcome {
        match completion {
            ActionCompletion::Sent { outcome, .. } => outcome,
            other => panic!("unexpected completion {other:?}"),
        }
    }

    #[test]
    fn a_send_before_the_deadline_reaches_the_port_once() {
        let clock = FakeClock::new(1_000);
        let mut port = CountingPort::default();
        let outcome = outcome_of(run_action_op(
            &mut port,
            send_op(2_000),
            clock.as_ref(),
            None,
        ));
        assert_eq!(outcome, SignalOutcome::Sent);
        assert_eq!(port.sends, 1);
    }

    #[test]
    fn a_fail_stop_refuses_every_later_send() {
        let clock = FakeClock::new(1_000);
        let mut port = CountingPort::default();
        let outcome = outcome_of(run_action_op(
            &mut port,
            send_op(2_000),
            clock.as_ref(),
            Some("manager: invariant failure"),
        ));
        let SignalOutcome::NotSent { reason } = outcome else {
            panic!("a fail-stop must refuse the send");
        };
        assert!(reason.contains("fail-stop"), "{reason}");
        assert_eq!(port.sends, 0);
    }

    #[test]
    fn a_send_at_or_after_the_deadline_never_reaches_the_port() {
        let clock = FakeClock::new(2_000);
        let mut port = CountingPort::default();
        let at_deadline = outcome_of(run_action_op(
            &mut port,
            send_op(2_000),
            clock.as_ref(),
            None,
        ));
        assert!(matches!(at_deadline, SignalOutcome::NotSent { .. }));
        clock.advance_ms(1);
        let after = outcome_of(run_action_op(
            &mut port,
            send_op(2_000),
            clock.as_ref(),
            None,
        ));
        assert!(matches!(after, SignalOutcome::NotSent { .. }));
        assert_eq!(port.sends, 0);
    }
}
