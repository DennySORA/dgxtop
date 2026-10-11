//! Execution of the Manager's control jobs (audit, process action,
//! configuration, history read) on their worker threads. Shared by the
//! composition root, the integration rigs and the benches, so they exercise
//! the production code path (including the last deadline check before a
//! signal is sent).

use dgxtop_core::action::{ActionCompletion, ActionOp, AuditCompletion, AuditOp, SignalOutcome};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::ports::{
    AuditRepository, ConfigJob, ConfigRepository, ControlCompletion, HistoryJob, OverviewReader,
    ProcessActionPort,
};

/// Runs one configuration job: a compare-and-swap of the candidate on the
/// expected revision.
pub fn run_config_job(repository: &mut dyn ConfigRepository, job: ConfigJob) -> ControlCompletion {
    let ConfigJob::Persist {
        request_id,
        expected_revision,
        candidate,
    } = job;
    let result = repository.compare_and_swap(expected_revision, &candidate);
    ControlCompletion::Config { request_id, result }
}

/// Runs one Overview History read for a session (D-82); the answer keeps
/// the job's session and generation so a stale one is dropped.
pub fn run_history_job(
    history: &dyn OverviewReader,
    job: HistoryJob,
    clock: &dyn TimeProvider,
) -> ControlCompletion {
    let result = history.read(&job.request, job.deadline_boot_ns, &job.cancel, clock);
    ControlCompletion::History {
        request_id: job.request_id,
        session_id: job.session_id,
        session_generation: job.session_generation,
        result,
    }
}

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
        AuditOp::Expire { before_utc } => AuditCompletion::Expired {
            result: repository.expire(&before_utc),
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
    use dgxtop_core::action::{PersistenceKind, PersistenceStatus};
    use dgxtop_core::config::Config;
    use dgxtop_core::errors::{ErrorCode, ErrorDetail};
    use dgxtop_core::history::SeriesInEveryBoot;
    use dgxtop_core::identity::{ProcessKey, SessionId};
    use dgxtop_core::metric::ids;
    use dgxtop_core::overview_history::{OverviewAnswer, OverviewRequest};
    use dgxtop_core::ports::{
        CancelFlag, ConfigLoadError, LoadedConfig, PersistError, PersistReceipt,
    };
    use dgxtop_core::testing::{FakeClock, metric_key};
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

    /// Config store holding one revision: a swap on it succeeds, any other
    /// is a conflict.
    struct RevisionStore {
        current: u32,
    }

    impl ConfigRepository for RevisionStore {
        fn load(&mut self) -> Result<LoadedConfig, ConfigLoadError> {
            unreachable!("not exercised")
        }

        fn compare_and_swap(
            &mut self,
            expected_revision: u32,
            _candidate: &Config,
        ) -> Result<PersistReceipt, PersistError> {
            if expected_revision != self.current {
                return Err(PersistError::Conflict {
                    current: self.current,
                });
            }
            self.current += 1;
            Ok(PersistReceipt {
                revision: self.current,
                sha256: String::new(),
                path: "/acme/config.json".to_owned(),
                file_bytes: 1,
            })
        }

        fn status(&self) -> PersistenceStatus {
            PersistenceStatus::healthy(PersistenceKind::Config)
        }
    }

    fn persist(expected_revision: u32) -> (Uuid, ConfigJob) {
        let request_id = Uuid::new_v4();
        let job = ConfigJob::Persist {
            request_id,
            expected_revision,
            candidate: Box::new(Config::defaults().unwrap()),
        };
        (request_id, job)
    }

    /// A swap on the current revision answers its receipt under the job's
    /// request; a stale revision answers the conflict, never a receipt.
    #[test]
    fn a_config_job_answers_the_swap_under_its_request() {
        let mut store = RevisionStore { current: 3 };
        let (request_id, job) = persist(3);
        let ControlCompletion::Config {
            request_id: answered,
            result,
        } = run_config_job(&mut store, job)
        else {
            panic!("a config job answers a config completion");
        };
        assert_eq!(answered, request_id);
        assert_eq!(result.map(|receipt| receipt.revision), Ok(4));
        let (_, stale) = persist(3);
        let ControlCompletion::Config { result, .. } = run_config_job(&mut store, stale) else {
            panic!("a config job answers a config completion");
        };
        assert_eq!(result, Err(PersistError::Conflict { current: 4 }));
    }

    /// History reader answering every read with no window, or an error once
    /// the job's deadline passed.
    struct DeadlineHistory;

    impl OverviewReader for DeadlineHistory {
        fn read(
            &self,
            _request: &OverviewRequest,
            deadline_boot_ns: u64,
            _cancel: &CancelFlag,
            clock: &dyn TimeProvider,
        ) -> Result<OverviewAnswer, ErrorDetail> {
            if clock.now_boot_ns() >= deadline_boot_ns {
                return Err(ErrorDetail::new(
                    ErrorCode::TimedOut,
                    "history deadline passed",
                ));
            }
            Ok(OverviewAnswer::Windows(Vec::new()))
        }
    }

    fn history_job(deadline_boot_ns: u64) -> HistoryJob {
        HistoryJob {
            request_id: Uuid::new_v4(),
            request: OverviewRequest::Windows {
                series: SeriesInEveryBoot::from(&metric_key("memory", ids::MEMORY_TOTAL, "system")),
                spans_ns: Vec::new(),
                at_utc_ns: 0,
            },
            session_id: SessionId::new("session-acme").unwrap(),
            session_generation: 2,
            deadline_boot_ns,
            cancel: CancelFlag::new(),
        }
    }

    /// A history read answers under the job's request, session and
    /// generation, its error included.
    #[test]
    fn a_history_job_answers_under_its_session_and_generation() {
        let clock = FakeClock::new(1_000);
        for (deadline, answered) in [(2_000, true), (1_000, false)] {
            let job = history_job(deadline);
            let request = job.request_id;
            let ControlCompletion::History {
                request_id,
                session_id,
                session_generation,
                result,
            } = run_history_job(&DeadlineHistory, job, clock.as_ref())
            else {
                panic!("a history job answers a history completion");
            };
            assert_eq!(request_id, request);
            assert_eq!(session_id.as_str(), "session-acme");
            assert_eq!(session_generation, 2);
            assert_eq!(result.is_ok(), answered);
        }
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
