//! AT39 (error layering and safe shutdown) with real workers:
//!
//! * a Manager invariant failure (a panic on the control thread) must
//!   fail-stop — frontends told to stop, nothing accepted, nothing sent;
//! * a shutdown while the config and audit storage hang stays inside the
//!   3 s control budget, answers every accepted command honestly
//!   ("on-disk state unknown", never "unchanged"), and an intent that
//!   becomes durable after the revocation is surfaced as `unknown` on the
//!   next start, never sent.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dgxtop_core::action::ActionState;
use dgxtop_core::commands::{CommandBody, CommandStatus};
use dgxtop_core::errors::{ErrorCode, ResultCode};
use dgxtop_core::ports::{AuditRepository, ConfigRepository, SessionPort};
use dgxtop_manager::ManagerPorts;
use dgxtop_store::config::FileConfigRepository;
use dgxtop_store::fs::RealFileSystem;
use dgxtop_store::sqlite::format_utc_rfc3339;

use crate::support::action_rig::{ActionRig, action_config};
use crate::support::filesystem::FaultyFileSystem;
use crate::support::manager_rig::RESULT_WAIT;
use crate::support::probes::{
    AdjustableFreeSpace, Gate, TempDir, serial, thread_exists, wait_until,
};
use crate::support::workers::spawn_config_worker;

/// The Manager's bounded shutdown wait (RUNTIME.md §8).
const CONTROL_BUDGET: Duration = Duration::from_secs(3);
/// Scheduling slack on top of a budget.
const SLACK: Duration = Duration::from_millis(500);
/// Per-worker stop bound used by the composition root.
const WORKER_STOP: Duration = Duration::from_millis(500);

#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at39_manager_invariant_failure_fails_stop_and_never_sends() {
    let _serial = serial();
    let mut rig = ActionRig::start(
        &action_config(),
        AdjustableFreeSpace::plenty(),
        Gate::open(),
    );
    rig.wait_until_audit_is_healthy();
    let token = rig.prepare();

    rig.snapshots.inject_invariant_failure();
    let outcome = rig.manager.join_within(Duration::from_secs(2));
    assert!(
        outcome.is_err(),
        "the control thread stops on the invariant failure"
    );
    // Fail-stop: every frontend is told to stop (it would otherwise keep
    // rendering a frozen screen) and no command is accepted any more.
    assert!(
        rig.manager.session.shutdown_requested(),
        "frontends must be told to stop after a Manager invariant failure"
    );
    let late = rig
        .manager
        .try_submit(CommandBody::ConfirmSignal {
            confirmation_token: token,
        })
        .expect_err("a dead control loop accepts nothing");
    assert_eq!(late.code, ErrorCode::Unavailable);
    assert_eq!(rig.actions.sends(), 0, "no signal after a fail-stop");
    // The rest of the composition still stops within its bounds.
    for (name, report) in rig.workers.stop_all(WORKER_STOP) {
        assert_eq!(report.detached, 0, "{name} stopped");
    }
}

#[test]
#[ignore = "fault-injection run (opt-in): cargo test --test fault_injection -- --ignored"]
fn at39_shutdown_with_hung_config_and_audit_storage_is_bounded_and_honest() {
    let _serial = serial();
    let config_dir = TempDir::new("shutdown-config");
    let config_path = config_dir.join("config.json");
    let fsync_gate = Gate::open();
    let filesystem = Arc::new(FaultyFileSystem::with_fsync_gate(fsync_gate.clone()));
    let mut repository = FileConfigRepository::new(config_path.clone(), filesystem);
    let saved = repository.load().expect("defaults").config;
    let intent_gate = Gate::open();
    let mut rig = ActionRig::start_with(
        &saved,
        AdjustableFreeSpace::plenty(),
        intent_gate.clone(),
        |completions, workers, ports| {
            let config = spawn_config_worker(repository, completions.clone());
            workers.track(&config);
            ManagerPorts {
                config: Box::new(config),
                ..ports
            }
        },
    );
    rig.wait_until_audit_is_healthy();
    let token = rig.prepare();

    // The audit disk hangs under a confirmed action, the config disk under
    // a configuration change (inside the temp-file fsync, before the rename).
    intent_gate.close();
    let confirm = rig.manager.submit(CommandBody::ConfirmSignal {
        confirmation_token: token,
    });
    wait_until("the audit worker to hang", RESULT_WAIT, || {
        intent_gate.blocked() == 1
    });
    fsync_gate.close();
    let persist = rig.manager.submit(CommandBody::SetSamplingInterval {
        interval_ms: 2_000,
        expected_config_revision: 1,
    });
    wait_until("the config worker to hang", RESULT_WAIT, || {
        fsync_gate.blocked() == 1
    });

    let (summary, elapsed) = rig
        .manager
        .shut_down("operator quit", CONTROL_BUDGET + Duration::from_secs(1));
    assert!(
        elapsed <= CONTROL_BUDGET + SLACK,
        "control shutdown took {elapsed:?}"
    );
    assert!(summary.timed_out);
    let unfinished = summary.unfinished.join("\n");
    assert!(unfinished.contains("config persist"), "{unfinished}");
    assert!(unfinished.contains("audit intent"), "{unfinished}");
    assert!(rig.manager.session.shutdown_requested());
    let persist_result = rig.manager.result_for(persist, RESULT_WAIT);
    assert_eq!(
        persist_result.code,
        ResultCode::from(ErrorCode::PersistFailed)
    );
    assert!(persist_result.message.contains("commit_uncertain"));
    assert!(persist_result.message.contains("on-disk state unknown"));
    let confirm_result = rig.manager.result_for(confirm, RESULT_WAIT);
    assert_eq!(confirm_result.status, CommandStatus::Rejected);
    assert_eq!(confirm_result.action_state, Some(ActionState::Rejected));

    // Hung workers are detached at their bound instead of joined forever.
    let stopping = Instant::now();
    let reports = rig.workers.stop_all(WORKER_STOP);
    assert!(stopping.elapsed() < 4 * (WORKER_STOP + SLACK));
    for (name, report) in &reports {
        let hung = name == "config" || name == "audit";
        assert_eq!(report.detached, usize::from(hung), "{name}: {report:?}");
    }

    // The storage comes back after dgxtop gave up on it.
    fsync_gate.release();
    intent_gate.release();
    wait_until("the hung workers to finish", Duration::from_secs(5), || {
        !thread_exists("config-0") && !thread_exists("audit-0")
    });
    // The late config commit did land: "unknown" was the honest answer.
    let mut restarted = FileConfigRepository::new(config_path, Arc::new(RealFileSystem));
    let on_disk = restarted.load().expect("complete file").config;
    assert_eq!((on_disk.revision, on_disk.sampling.interval_ms), (2, 2_000));
    // The late intent is durable without any outcome: the next start marks
    // it unknown and never sends it.
    let mut audit = rig.reopen_audit();
    let recovered = audit
        .recover_unknown(
            rig.clock.now_boot_ns(),
            &format_utc_rfc3339(rig.clock.now_utc_ns()),
        )
        .expect("recovery");
    assert!(
        recovered.iter().any(|intent| intent.request_id == confirm),
        "{recovered:?}"
    );
    assert_eq!(rig.actions.sends(), 0);
}
