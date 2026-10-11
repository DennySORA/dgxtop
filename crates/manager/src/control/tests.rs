//! Manager-level tests: the real control loop with every port faked.
//! AT05 (ingestion handshake), AT08 (config CAS), AT10/AT13 (actions,
//! stale targets), AT22 (scheduling/epochs), AT23 (fairness, flood,
//! idempotency), AT30 (overload effects), AT39 (bounded shutdown) and
//! projection pacing/accounting.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dgxtop_core::action::{
    ActionCompletion, ActionHandle, ActionOp, ActionState, AuditCompletion, AuditOp,
    DurableReceipt, PreparedProcess, Signal, SignalOutcome, VerifiedIdentity,
};
use dgxtop_core::canonical::{
    AckStatus, ArchiveState, CollectionBatch, CollectionPlan, CollectorState, ProcessRow,
    ProcessTable, StoreAck,
};
use dgxtop_core::commands::{CommandBody, CommandResult, CommandStatus, SortKey, ViewKind};
use dgxtop_core::config::Config;
use dgxtop_core::diagnostics::names;
use dgxtop_core::errors::{ErrorCode, ResultCode};
use dgxtop_core::event::EventKind;
use dgxtop_core::identity::{Pid, PidNamespace, ProcessKey};
use dgxtop_core::observation::{
    BatchStatus, IPC_PROTOCOL_VERSION, Observation, ObservationBatch, PAGE_CONTINUES, ReadPlan,
    split_page,
};
use dgxtop_core::ports::{
    AdapterJob, ConfigJob, ControlCompletion, IngestionCompletion, IngressBatch, PersistReceipt,
    SessionPort, StoreJob,
};
use dgxtop_core::quality::SampleKind;
use dgxtop_core::testing::{boot_id, host_id, metric_key};
use uuid::Uuid;

use crate::error::ManagerError;
use crate::manager::Manager;
use crate::test_support::{
    HARNESS_EUID, Harness, HarnessOptions, committed_snapshot, harness_ports, harness_setup,
    manifest, sample_entry,
};

const MS: u64 = 1_000_000;

fn batch_reply(
    plan: &ReadPlan,
    records: Vec<Observation>,
    continues: bool,
    now: u64,
) -> IngestionCompletion {
    let batch = ObservationBatch {
        protocol_version: IPC_PROTOCOL_VERSION,
        adapter_id: plan.adapter_id.clone(),
        host_generation: plan.host_generation,
        catalog_revision: plan.catalog_revision,
        config_revision: plan.config_revision,
        request_id: plan.request_id,
        cycle_id: plan.cycle_id,
        read_group: plan.read_group.clone(),
        started_boot_ns: now.saturating_sub(MS),
        finished_boot_ns: now,
        records,
        status: if continues {
            BatchStatus::Partial
        } else {
            BatchStatus::Ok
        },
        error_code: continues.then(|| PAGE_CONTINUES.to_owned()),
    };
    IngestionCompletion::Batch {
        adapter_id: plan.adapter_id.clone(),
        request_id: plan.request_id,
        host_generation: plan.host_generation,
        result: Ok(Arc::new(IngressBatch::new(batch, None))),
    }
}

fn group_of(plan: &ReadPlan) -> String {
    split_page(plan.read_group.as_str()).0.to_owned()
}

/// Harness with procfs up and its first discovery answered.
fn ready_harness(options: HarnessOptions) -> Harness {
    let mut harness = Harness::new(options);
    harness.host_up("procfs", 1, Some(5000));
    let reads = harness.answer_discovery(Vec::new());
    assert!(reads.is_empty());
    harness
}

/// Advances time until a read of `group` is dispatched; other reads get an
/// immediate empty final reply.
fn next_read_of(harness: &mut Harness, group: &str) -> ReadPlan {
    for _ in 0..500 {
        harness.ports.clock.advance_ms(10);
        harness.settle();
        let mut found = None;
        for job in harness.ports.adapters.take() {
            match job {
                AdapterJob::Read(plan) if group_of(&plan) == group && found.is_none() => {
                    found = Some(plan);
                }
                AdapterJob::Read(plan) => {
                    let now = harness.now_ns();
                    harness.ingest(batch_reply(&plan, Vec::new(), false, now));
                }
                AdapterJob::Discover(_) => {}
            }
        }
        harness.settle();
        if let Some(plan) = found {
            return plan;
        }
    }
    panic!("no read of {group}");
}

/// Answers reads for up to `limit_ms`; returns whether a discovery request
/// was dispatched meanwhile.
fn discovery_within(harness: &mut Harness, limit_ms: u64) -> bool {
    for _ in 0..limit_ms / 10 {
        for job in harness.ports.adapters.take() {
            match job {
                AdapterJob::Discover(_) => return true,
                AdapterJob::Read(plan) => {
                    let now = harness.now_ns();
                    harness.ingest(batch_reply(&plan, Vec::new(), false, now));
                }
            }
        }
        harness.ports.clock.advance_ms(10);
        harness.settle();
    }
    false
}

fn collected(plan: &CollectionPlan) -> Box<CollectionBatch> {
    Box::new(CollectionBatch {
        batch_id: plan.batch_id,
        collector_id: plan.collector_id.clone(),
        cycle_id: plan.cycle_id,
        epoch: plan.epoch,
        config_revision: plan.config_revision,
        catalog_revision: plan.catalog.revision,
        policy_revision: plan.policy.revision,
        state_base_revision: plan.state_revision,
        canonical: Vec::new(),
        explanations: Vec::new(),
        counter_intervals: Vec::new(),
        inputs: plan.inputs.clone(),
        process_table: None,
        next_state: CollectorState::default(),
        failures: Vec::new(),
        events: Vec::new(),
    })
}

fn navigation(view: ViewKind, process: Option<ProcessKey>, filter: &str) -> CommandBody {
    CommandBody::UpdateViewSession {
        view,
        selected_entity_id: None,
        selected_process: process,
        filter: filter.into(),
        sort: SortKey::Pid,
        ascending: true,
        frame_revision: 0,
        overview_bar: None,
        process_bar: None,
    }
}

fn target(pid: i64) -> ProcessKey {
    ProcessKey {
        host_id: host_id("test-host"),
        boot_id: boot_id("fake-boot"),
        pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
        pid: Pid::new(pid).unwrap(),
        start_time_ticks: 777,
    }
}

fn with_process_table(harness: &Harness, pids: &[i64]) {
    let rows = pids
        .iter()
        .map(|pid| ProcessRow {
            key: target(*pid),
            name: "train.py".into(),
            command: String::new(),
            uid: Some(HARNESS_EUID),
            state: Some('S'),
            ppid: None,
            threads: None,
            descendants: None,
            cpu_percent: Some(1.0),
            rss_bytes: Some(1 << 20),
            missing_reason: None,
            gpu: Vec::new(),
            sample_kind: SampleKind::Measured,
            read_finished_boot_ns: 0,
        })
        .collect();
    harness.ports.snapshots.update(|snapshot| {
        snapshot.process_table = Arc::new(ProcessTable {
            rows,
            ..ProcessTable::default()
        });
    });
}

fn events(harness: &Harness) -> Vec<EventKind> {
    harness
        .manager
        .event_feed()
        .recent(512)
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

// ----- AT22 -----------------------------------------------------------------

#[test]
fn at22_at08_late_reply_of_the_old_epoch_is_dropped_after_a_config_change() {
    let mut harness = ready_harness(HarnessOptions::default());
    let stale_plan = next_read_of(&mut harness, "memory");
    let request = harness
        .submit(CommandBody::SetSamplingInterval {
            interval_ms: 2000,
            expected_config_revision: 1,
        })
        .unwrap();
    harness.settle();
    let jobs = harness.ports.config.take();
    let ConfigJob::Persist { request_id, .. } = &jobs[0];
    assert_eq!(*request_id, request);
    // Before the durable receipt nothing changed.
    assert!(
        !events(&harness)
            .iter()
            .any(|kind| matches!(kind, EventKind::ConfigApplied { .. }))
    );
    harness.control(ControlCompletion::Config {
        request_id: request,
        result: Ok(PersistReceipt {
            revision: 2,
            sha256: "0".repeat(64),
            path: "/x".into(),
        }),
    });
    harness.settle();
    let result = harness.result_for(request);
    assert!(result.code.is_ok());
    assert_eq!(result.config_revision, Some(2));
    assert_eq!(result.effective_interval_ms, Some(2000));
    assert!(events(&harness).iter().any(|kind| matches!(
        kind,
        EventKind::ConfigApplied {
            revision: 2,
            epoch: 2
        }
    )));
    // The reply to the old-epoch request arrives late: dropped and counted.
    let collectors_before = harness.ports.collectors.len();
    let now = harness.now_ns();
    harness.ingest(batch_reply(&stale_plan, Vec::new(), false, now));
    harness.settle();
    assert_eq!(
        harness
            .ports
            .diagnostics
            .counter(names::STALE_RESULT_DROPPED_TOTAL, "old_epoch"),
        1
    );
    assert_eq!(
        harness.ports.collectors.len(),
        collectors_before,
        "no pollution"
    );
    let intervals = harness.manager.core().scheduler.intervals();
    let memory = intervals
        .iter()
        .find(|view| view.read_group == "memory")
        .unwrap();
    assert_eq!((memory.configured_ms, memory.effective_ms), (2000, 2000));
}

/// Applies a durable `SetSamplingInterval` of `interval_ms`.
fn apply_interval(harness: &mut Harness, interval_ms: u32) {
    let request = harness
        .submit(CommandBody::SetSamplingInterval {
            interval_ms,
            expected_config_revision: 1,
        })
        .unwrap();
    harness.settle();
    harness.ports.config.take();
    harness.control(ControlCompletion::Config {
        request_id: request,
        result: Ok(PersistReceipt {
            revision: 2,
            sha256: "0".repeat(64),
            path: "/x".into(),
        }),
    });
    harness.settle();
    assert!(harness.result_for(request).code.is_ok());
}

/// The sampling configurations the store was told to apply.
fn applied_samplings(harness: &Harness) -> Vec<u32> {
    harness
        .ports
        .store
        .take()
        .into_iter()
        .filter_map(|job| match job {
            StoreJob::ApplySampling(sampling) => Some(sampling.interval_ms),
            _ => None,
        })
        .collect()
}

/// The state keeps its own sampling timing (stale windows, the history's
/// finalize grace), so a durable interval change must reach it.
#[test]
fn a_sampling_change_is_sent_to_the_state() {
    let mut harness = ready_harness(HarnessOptions::default());
    harness.ports.store.take();
    apply_interval(&mut harness, 2000);
    assert_eq!(applied_samplings(&harness), vec![2000]);
}

/// A busy store must not lose the change: the state would keep the old
/// timing for the rest of the run.
#[test]
fn a_sampling_change_refused_by_a_busy_store_is_sent_again() {
    let mut harness = ready_harness(HarnessOptions::default());
    harness.ports.store.take();
    harness.ports.store.set_busy(true);
    apply_interval(&mut harness, 2000);
    assert!(applied_samplings(&harness).is_empty());
    harness.ports.store.set_busy(false);
    harness.ports.clock.advance_ms(1_100);
    harness.settle();
    assert_eq!(applied_samplings(&harness), vec![2000]);
    harness.ports.clock.advance_ms(1_100);
    harness.settle();
    assert!(applied_samplings(&harness).is_empty(), "sent once");
}

#[test]
fn at22_host_restart_drops_the_request_records_a_gap_and_rediscovers() {
    let mut harness = ready_harness(HarnessOptions::default());
    let lost = next_read_of(&mut harness, "cpu");
    harness.ports.store.take();
    harness.ports.clock.advance_ms(2_000);
    harness.host_up("procfs", 2, Some(6000));
    let gaps: Vec<_> = harness
        .ports
        .store
        .take()
        .into_iter()
        .filter_map(|job| match job {
            StoreJob::Gap(gap) => Some(gap),
            _ => None,
        })
        .collect();
    assert!(gaps.iter().any(|gap| gap.reason == "host_restarted"));
    assert!(
        discovery_within(&mut harness, 200),
        "the new incarnation is rediscovered early (earliest-deadline order)"
    );
    let now = harness.now_ns();
    harness.ingest(batch_reply(&lost, Vec::new(), false, now));
    harness.settle();
    assert!(
        harness
            .ports
            .diagnostics
            .counter(names::STALE_RESULT_DROPPED_TOTAL, "other_request")
            + harness
                .ports
                .diagnostics
                .counter(names::STALE_RESULT_DROPPED_TOTAL, "no_request")
            >= 1
    );
}

fn counter_total(harness: &Harness, metric: &str) -> u64 {
    harness
        .ports
        .diagnostics
        .snapshot()
        .counters
        .get(metric)
        .map_or(0, |series| series.values().sum())
}

#[test]
fn a_refused_read_dispatch_is_deferred_and_retried_never_counted_as_a_drop() {
    let mut harness = ready_harness(HarnessOptions::default());
    harness.ports.store.take();
    harness.ports.adapters.set_busy(true);
    let deferred = |harness: &Harness| {
        harness
            .ports
            .diagnostics
            .counter(names::ADAPTER_DISPATCH_DEFERRED_TOTAL, "worker_busy")
    };
    for _ in 0..200 {
        if deferred(&harness) > 0 {
            break;
        }
        harness.ports.clock.advance_ms(10);
        harness.settle();
    }
    assert!(deferred(&harness) > 0, "a due read met the busy worker");
    assert_eq!(
        counter_total(&harness, names::QUEUE_DROPPED_TOTAL),
        0,
        "the scheduler keeps refused work, so nothing was dropped"
    );
    harness.ports.adapters.set_busy(false);
    harness.ports.clock.advance_ms(30);
    harness.settle();
    let retried = harness
        .ports
        .adapters
        .take()
        .into_iter()
        .any(|job| matches!(job, AdapterJob::Read(_)));
    assert!(
        retried,
        "the deferred read goes out once the worker accepts"
    );
    let gaps = harness
        .ports
        .store
        .take()
        .into_iter()
        .filter(|job| matches!(job, StoreJob::Gap(_)))
        .count();
    assert_eq!(gaps, 0, "a short deferral misses no slot");
}

// ----- AT05 -----------------------------------------------------------------

#[test]
fn at05_pages_accumulate_into_one_job_and_state_advances_only_after_the_ack() {
    let mut harness = ready_harness(HarnessOptions::default());
    let first = next_read_of(&mut harness, "cpu");
    harness.ports.collectors.take();
    let now = harness.now_ns();
    harness.ingest(batch_reply(&first, Vec::new(), true, now));
    harness.settle();
    // The next page of the same cycle is requested immediately.
    let jobs = harness.ports.adapters.take();
    let AdapterJob::Read(second) = &jobs[0] else {
        panic!("page request expected: {jobs:?}");
    };
    assert_eq!(second.page, 1);
    assert_eq!(second.cycle_id, first.cycle_id);
    assert_eq!(second.read_group.as_str(), "cpu:1");
    assert!(
        harness
            .ports
            .collectors
            .take()
            .iter()
            .all(|plan| plan.collector_id.as_str() != "cpu"),
        "no cpu job before the last page"
    );
    harness.ingest(batch_reply(second, Vec::new(), false, now));
    harness.settle();
    let plans: Vec<_> = harness
        .ports
        .collectors
        .take()
        .into_iter()
        .filter(|plan| plan.collector_id.as_str() == "cpu")
        .collect();
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert_eq!(plan.collector_id.as_str(), "cpu");
    assert_eq!(plan.inputs.len(), 2, "both pages in one job");
    assert_eq!(plan.state_revision, 0);
    // Collected -> guarded commit.
    harness.ingest(IngestionCompletion::Collected(collected(plan)));
    harness.settle();
    let commits: Vec<_> = harness
        .ports
        .store
        .take()
        .into_iter()
        .filter_map(|job| match job {
            StoreJob::Commit { guard, .. } => Some(guard),
            _ => None,
        })
        .collect();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].expected_epoch, 1);
    assert_eq!(commits[0].state_base_revision, 0);
    // The store adopted next_state; the ack lets the collector run again.
    let cpu = metric_key("cpu", "cpu.utilization.total", "host");
    harness.ports.snapshots.set(committed_snapshot(
        2,
        vec![("cpu", sample_entry(&cpu, Some(42.0), now))],
    ));
    harness.ports.snapshots.set_collector_state("cpu", 1);
    harness.ingest(IngestionCompletion::Committed(StoreAck {
        batch_id: plan.batch_id,
        collector_id: plan.collector_id.clone(),
        status: AckStatus::Committed,
        committed_revision: 2,
        state_revision: 1,
        archive: ArchiveState::Disabled,
        reason: None,
    }));
    harness.settle();
    assert!(events(&harness).iter().any(|kind| matches!(
        kind,
        EventKind::CollectionCommitted {
            snapshot_revision: 2,
            ..
        }
    )));
    let trend = harness.manager.core().trends.book().trend(&cpu, 10);
    assert_eq!(trend.points, vec![Some(42.0)]);
    let next = next_read_of(&mut harness, "cpu");
    let now = harness.now_ns();
    harness.ingest(batch_reply(&next, Vec::new(), false, now));
    harness.settle();
    let plans = harness.ports.collectors.take();
    let cpu_plan = plans
        .iter()
        .find(|plan| plan.collector_id.as_str() == "cpu")
        .unwrap();
    assert_eq!(cpu_plan.state_revision, 1, "new base only after the ack");
}

// ----- AT23 -----------------------------------------------------------------

#[test]
fn at23_iteration_is_fair_8_control_8_commands_4_ingestion() {
    let mut harness = ready_harness(HarnessOptions::default());
    for _ in 0..20 {
        harness.control(ControlCompletion::History {
            request_id: Uuid::new_v4(),
            session_id: harness.session.session_id().clone(),
            session_generation: 99,
            result: Err(dgxtop_core::errors::ErrorDetail::new(
                ErrorCode::TimedOut,
                "x",
            )),
        });
        harness
            .submit(navigation(ViewKind::Overview, None, ""))
            .unwrap();
        harness.ingest(IngestionCompletion::HostStatus(
            dgxtop_core::view::HostStatus {
                adapter_id: "ghost".into(),
                state: dgxtop_core::view::HostState::Healthy,
                generation: 1,
                pid: None,
                last_success_boot_ns: None,
                timeouts_total: 0,
                consecutive_failures: 0,
                restarts_total: 0,
                backoff_until_boot_ns: None,
                detail: None,
            },
        ));
    }
    let iteration = harness.step();
    assert!(iteration.more_ready);
    let (control, commands, ingestion) = harness.manager.queued();
    assert!((12..20).contains(&control), "control left {control}");
    assert!((12..20).contains(&commands), "commands left {commands}");
    assert!((16..20).contains(&ingestion), "ingestion left {ingestion}");
    harness.settle();
    assert_eq!(harness.manager.queued(), (0, 0, 0));
    assert_eq!(harness.results().len(), 20);
}

#[test]
fn at23_command_flood_is_busy_without_blocking_timers_and_shutdown_still_run() {
    let mut harness = ready_harness(HarnessOptions::default());
    let started = Instant::now();
    let mut accepted = BTreeSet::new();
    let mut answered = BTreeSet::new();
    let mut busy = 0u32;
    let mut reads = 0u32;
    for _ in 0..100 {
        for _ in 0..1000 {
            match harness.submit(navigation(ViewKind::Overview, None, "")) {
                Ok(request_id) => {
                    accepted.insert(request_id);
                }
                Err(rejection) => {
                    assert_eq!(rejection.code, ErrorCode::Busy);
                    busy += 1;
                }
            }
        }
        harness.ports.clock.advance_ms(10);
        harness.step();
        for result in harness.results() {
            assert!(accepted.contains(&result.request_id));
            assert!(
                answered.insert(result.request_id),
                "exactly one result each"
            );
        }
        for job in harness.ports.adapters.take() {
            if let AdapterJob::Read(plan) = job {
                reads += 1;
                let now = harness.now_ns();
                harness.ingest(batch_reply(&plan, Vec::new(), false, now));
            }
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "submit never blocks"
    );
    assert!(busy > 90_000, "busy {busy}");
    assert!(
        reads >= 3,
        "timers kept firing during the flood ({reads} reads)"
    );
    // Shutdown is out of band: it is not queued behind the flood.
    harness.session.request_shutdown("flood test");
    harness.step();
    assert!(harness.session.shutdown_requested());
    harness.settle();
    let summary = harness.manager.summary().expect("shutdown finished");
    assert!(!summary.timed_out);
    for result in harness.results() {
        assert!(
            answered.insert(result.request_id),
            "exactly one result each"
        );
    }
    assert_eq!(answered, accepted, "every accepted command got its result");
}

// ----- AT08 -----------------------------------------------------------------

#[test]
fn at08_restart_only_and_stale_revisions_never_persist() {
    let mut harness = ready_harness(HarnessOptions::default());
    let mut candidate = Config::defaults().unwrap();
    candidate.store.max_series = 100;
    let restart = harness
        .submit(CommandBody::ReplaceConfig {
            expected_config_revision: 1,
            candidate: Box::new(candidate),
        })
        .unwrap();
    let stale = harness
        .submit(CommandBody::SetSamplingInterval {
            interval_ms: 500,
            expected_config_revision: 9,
        })
        .unwrap();
    harness.settle();
    let results = harness.results();
    let code_of = |id: Uuid| results.iter().find(|r| r.request_id == id).unwrap().code;
    assert_eq!(
        code_of(restart),
        ResultCode::from(ErrorCode::RequiresRestart)
    );
    assert_eq!(code_of(stale), ResultCode::from(ErrorCode::Conflict));
    assert!(harness.ports.config.is_empty());
}

#[test]
fn at08_persist_failure_keeps_the_running_configuration() {
    let mut harness = ready_harness(HarnessOptions::default());
    let request = harness
        .submit(CommandBody::SetSamplingInterval {
            interval_ms: 3000,
            expected_config_revision: 1,
        })
        .unwrap();
    harness.settle();
    harness.ports.config.take();
    harness.control(ControlCompletion::Config {
        request_id: request,
        result: Err(dgxtop_core::ports::PersistError::Io(
            dgxtop_core::errors::InfraError::DiskFull {
                operation: "config.write",
            },
        )),
    });
    harness.settle();
    assert_eq!(
        harness.result_for(request).code,
        ResultCode::from(ErrorCode::PersistFailed)
    );
    assert_eq!(harness.manager.core().scheduler.epoch(), 1, "no new epoch");
    assert_eq!(
        harness
            .manager
            .core()
            .config
            .effective()
            .sampling
            .interval_ms,
        1000
    );
}

// ----- AT10 / AT13 -----------------------------------------------------------

fn prepared_for(key: &ProcessKey) -> PreparedProcess {
    PreparedProcess {
        handle: ActionHandle(21),
        verified: VerifiedIdentity {
            key: key.clone(),
            uid: HARNESS_EUID,
            basename: "python3".into(),
        },
        display_summary: key.summary(),
    }
}

#[test]
fn at10_full_action_path_through_commands_sends_once() {
    let mut harness = ready_harness(HarnessOptions::default());
    with_process_table(&harness, &[4242]);
    let key = target(4242);
    let select = harness
        .submit(navigation(ViewKind::Processes, Some(key.clone()), ""))
        .unwrap();
    harness.settle();
    assert!(harness.result_for(select).code.is_ok());
    let prepare = harness
        .submit(CommandBody::PrepareSignal {
            target: key.clone(),
            signal: Signal::Sigterm,
        })
        .unwrap();
    harness.settle();
    let ops = harness.ports.actions.take();
    assert!(matches!(&ops[..], [ActionOp::Prepare { request_id, .. }] if *request_id == prepare));
    harness.control(ControlCompletion::Action(ActionCompletion::Prepared {
        request_id: prepare,
        result: Ok(prepared_for(&key)),
    }));
    harness.settle();
    let prepared = harness.result_for(prepare);
    assert_eq!(prepared.action_state, Some(ActionState::Prepared));
    let token = prepared.confirmation_token.clone().unwrap();
    harness.ports.audit.take();
    let confirm = harness
        .submit(CommandBody::ConfirmSignal {
            confirmation_token: token,
        })
        .unwrap();
    harness.settle();
    let intent = harness
        .ports
        .audit
        .take()
        .into_iter()
        .find_map(|op| match op {
            AuditOp::AppendIntent(intent) => Some(intent),
            _ => None,
        })
        .unwrap();
    assert_eq!(intent.request_id, confirm);
    assert!(
        harness.ports.actions.is_empty(),
        "no signal before durability"
    );
    harness.control(ControlCompletion::Audit(AuditCompletion::IntentDurable {
        request_id: intent.request_id,
        intent_id: intent.intent_id,
        result: Ok(DurableReceipt {
            request_id: intent.request_id,
            intent_id: intent.intent_id,
            committed_boot_ns: harness.now_ns(),
        }),
    }));
    harness.settle();
    let sends: Vec<_> = harness
        .ports
        .actions
        .take()
        .into_iter()
        .filter(|op| matches!(op, ActionOp::Send { .. }))
        .collect();
    assert_eq!(sends.len(), 1);
    harness.control(ControlCompletion::Action(ActionCompletion::Sent {
        request_id: confirm,
        intent_id: intent.intent_id,
        outcome: SignalOutcome::Sent,
        sent_boot_ns: harness.now_ns(),
    }));
    harness.settle();
    let result = harness.result_for(confirm);
    assert_eq!(result.status, CommandStatus::Completed);
    assert_eq!(result.action_state, Some(ActionState::SignalSent));
    assert!(
        events(&harness)
            .iter()
            .any(|kind| matches!(kind, EventKind::ActionSignalSent(_)))
    );
}

#[test]
fn at13_absent_or_unselected_targets_are_rejected_and_never_retargeted() {
    let mut harness = ready_harness(HarnessOptions::default());
    with_process_table(&harness, &[10, 11]);
    let gone = harness
        .submit(CommandBody::PrepareSignal {
            target: target(12),
            signal: Signal::Sigterm,
        })
        .unwrap();
    harness.settle();
    assert_eq!(
        harness.result_for(gone).code,
        ResultCode::from(ErrorCode::ProcessExited)
    );
    harness
        .submit(navigation(ViewKind::Processes, Some(target(10)), ""))
        .unwrap();
    harness.settle();
    harness.results();
    let other = harness
        .submit(CommandBody::PrepareSignal {
            target: target(11),
            signal: Signal::Sigterm,
        })
        .unwrap();
    harness.settle();
    assert_eq!(
        harness.result_for(other).code,
        ResultCode::from(ErrorCode::StaleView)
    );
    assert!(
        harness.ports.actions.is_empty(),
        "the worker never saw them"
    );
}

#[test]
fn at10_managed_host_pids_are_protected_targets() {
    let mut harness = Harness::new(HarnessOptions::default());
    harness.host_up("procfs", 1, Some(4242));
    with_process_table(&harness, &[4242]);
    let request = harness
        .submit(CommandBody::PrepareSignal {
            target: target(4242),
            signal: Signal::Sigterm,
        })
        .unwrap();
    harness.settle();
    assert_eq!(
        harness.result_for(request).code,
        ResultCode::from(ErrorCode::CapabilityDenied)
    );
    assert!(harness.ports.actions.is_empty());
}

// ----- AT39 -----------------------------------------------------------------

#[test]
fn at39_clean_shutdown_finishes_at_once() {
    let mut harness = ready_harness(HarnessOptions::default());
    harness.manager.shutdown_trigger().trigger("test");
    harness.step();
    let summary = harness.manager.summary().expect("finished");
    assert!(!summary.timed_out);
    assert!(summary.unfinished.is_empty());
    assert_eq!(summary.reason, "test");
    assert!(harness.session.shutdown_requested());
    let late = harness.submit(navigation(ViewKind::Overview, None, ""));
    assert_eq!(late.unwrap_err().code, ErrorCode::Unavailable);
}

#[test]
fn at39_shutdown_waits_at_most_three_seconds_and_reports_unfinished_work() {
    let mut harness = ready_harness(HarnessOptions::default());
    let persist = harness
        .submit(CommandBody::SetSamplingInterval {
            interval_ms: 2000,
            expected_config_revision: 1,
        })
        .unwrap();
    harness.settle();
    assert_eq!(harness.ports.config.len(), 1);
    harness.manager.shutdown_trigger().trigger("bounded");
    harness.step();
    assert!(harness.manager.summary().is_none(), "waits for the receipt");
    // Accepted during the drain: answered, never lost.
    let during = harness
        .submit(navigation(ViewKind::Overview, None, ""))
        .unwrap();
    harness.ports.clock.advance_ms(1_000);
    harness.step();
    assert!(harness.manager.summary().is_none());
    harness.ports.clock.advance_ms(2_000);
    harness.step();
    let summary = harness.manager.summary().expect("bounded wait ended");
    assert!(summary.timed_out);
    assert!(
        summary
            .unfinished
            .iter()
            .any(|entry| entry.contains("config persist"))
    );
    assert!(summary.finished_boot_ns - summary.started_boot_ns <= 3_000 * MS);
    let results = harness.results();
    let find = |id: Uuid| results.iter().find(|r| r.request_id == id).unwrap().clone();
    let persist_result: CommandResult = find(persist);
    assert_eq!(
        persist_result.code,
        ResultCode::from(ErrorCode::PersistFailed)
    );
    assert!(persist_result.message.contains("commit_uncertain"));
    assert_eq!(find(during).code, ResultCode::from(ErrorCode::Unavailable));
}

// ----- presentation ------------------------------------------------------------

/// Answers every read until each collector holds a job.
fn jobs_of_every_collector(harness: &mut Harness) -> Vec<CollectionPlan> {
    let mut plans: Vec<CollectionPlan> = Vec::new();
    for _ in 0..500 {
        harness.ports.clock.advance_ms(10);
        harness.settle();
        for job in harness.ports.adapters.take() {
            if let AdapterJob::Read(plan) = job {
                let now = harness.now_ns();
                harness.ingest(batch_reply(&plan, Vec::new(), false, now));
            }
        }
        harness.settle();
        plans.extend(harness.ports.collectors.take());
        let collectors: BTreeSet<_> = plans.iter().map(|plan| &plan.collector_id).collect();
        if collectors.len() == 3 {
            return plans;
        }
    }
    panic!("not every collector got a job: {plans:?}");
}

/// Collects and commits `plan` without moving the clock.
fn commit(harness: &mut Harness, plan: &CollectionPlan, revision: u64) {
    harness.ingest(IngestionCompletion::Collected(collected(plan)));
    harness.settle();
    harness.ports.store.take();
    harness.ingest(IngestionCompletion::Committed(StoreAck {
        batch_id: plan.batch_id,
        collector_id: plan.collector_id.clone(),
        status: AckStatus::Committed,
        committed_revision: revision,
        state_revision: 0,
        archive: ArchiveState::Disabled,
        reason: None,
    }));
    harness.settle();
}

/// Takes every pending projection; returns how many there were.
fn projections(harness: &Harness) -> usize {
    std::iter::from_fn(|| harness.session.take_projection()).count()
}

/// Every collector's commit used to build a projection; the commits of one
/// cycle now share one, built as soon as the last has committed.
#[test]
fn the_commits_of_one_cycle_share_one_projection() {
    let mut harness = ready_harness(HarnessOptions::default());
    let plans = jobs_of_every_collector(&mut harness);
    // Past the input spacing, so only the pacing can hold a frame back.
    harness.ports.clock.advance_ms(60);
    harness.settle();
    assert!(
        harness.ports.adapters.take().is_empty(),
        "no read in flight"
    );
    projections(&harness);
    let (last, first) = plans.split_last().unwrap();
    for (revision, plan) in (2..).zip(first) {
        commit(&mut harness, plan, revision);
        assert_eq!(projections(&harness), 0, "the cycle is still committing");
    }
    commit(&mut harness, last, 9);
    assert_eq!(projections(&harness), 1, "one frame for the whole cycle");
}

/// A collector that takes long holds the cycle's frame back for at most
/// the settle window.
#[test]
fn a_slow_collector_delays_the_projection_by_at_most_the_settle_window() {
    let mut harness = ready_harness(HarnessOptions::default());
    let plans = jobs_of_every_collector(&mut harness);
    projections(&harness);
    commit(&mut harness, &plans[0], 2);
    harness.ports.clock.advance_ms(49);
    harness.settle();
    assert_eq!(projections(&harness), 0);
    harness.ports.clock.advance_ms(1);
    harness.settle();
    assert_eq!(projections(&harness), 1);
}

/// Input never waits for a settling cycle.
#[test]
fn a_view_change_gets_a_projection_while_a_cycle_settles() {
    let mut harness = ready_harness(HarnessOptions::default());
    let plans = jobs_of_every_collector(&mut harness);
    harness.ports.clock.advance_ms(50);
    harness.settle();
    projections(&harness);
    commit(&mut harness, &plans[0], 2);
    assert_eq!(projections(&harness), 0);
    harness
        .submit(navigation(ViewKind::Processes, None, ""))
        .unwrap();
    harness.settle();
    let projection = harness.session.take_projection().expect("prompt frame");
    assert_eq!(projection.session.view, ViewKind::Processes);
}

// ----- history -----------------------------------------------------------------

#[test]
fn history_view_queries_and_drops_stale_generations() {
    let mut harness = ready_harness(HarnessOptions::default());
    // The history view charts the series the snapshot reports (D-46).
    let cpu = metric_key("cpu", "cpu.utilization.total", "host");
    let now = harness.now_ns();
    harness.ports.snapshots.set(committed_snapshot(
        2,
        vec![("cpu", sample_entry(&cpu, Some(42.0), now))],
    ));
    harness
        .submit(navigation(ViewKind::History, None, ""))
        .unwrap();
    harness.settle();
    let jobs = harness.ports.history.take();
    assert!(!jobs.is_empty());
    let job = &jobs[0];
    assert_eq!(
        job.request.series().metric_id.as_str(),
        "cpu.utilization.total"
    );
    assert_eq!(job.session_id, *harness.session.session_id());
    let answer = dgxtop_core::overview_history::OverviewAnswer::Bars(
        dgxtop_core::overview_history::OverviewBars {
            epoch: 0,
            settled_through_utc_ns: 0,
            bars: Vec::new(),
            kept_on_disk: false,
            store_issue: None,
        },
    );
    harness.control(ControlCompletion::History {
        request_id: job.request_id,
        session_id: job.session_id.clone(),
        session_generation: job.session_generation + 1,
        result: Ok(answer),
    });
    harness.settle();
    assert_eq!(
        harness
            .ports
            .diagnostics
            .counter(names::STALE_RESULT_DROPPED_TOTAL, "history"),
        1
    );
}

// ----- construction ----------------------------------------------------------------

#[test]
fn invalid_setups_and_session_limits_are_rejected() {
    let duplicate = HarnessOptions {
        adapters: vec![manifest("procfs", 100), manifest("procfs", 100)],
        ..HarnessOptions::default()
    };
    let (ports, _) = harness_ports(&duplicate);
    assert!(matches!(
        Manager::new(harness_setup(&duplicate), ports),
        Err(ManagerError::InvalidSetup(_))
    ));
    let options = HarnessOptions::default();
    let mut setup = harness_setup(&options);
    setup.boot_id = boot_id("another-boot");
    let (ports, _) = harness_ports(&options);
    assert!(matches!(
        Manager::new(setup, ports),
        Err(ManagerError::InvalidSetup(_))
    ));
    let mut restart = HarnessOptions::default();
    restart.effective.store.max_series = 100;
    let (ports, _) = harness_ports(&restart);
    assert!(Manager::new(harness_setup(&restart), ports).is_err());
    let mut harness = Harness::new(HarnessOptions::default());
    let _second = harness.manager.open_session().unwrap();
    assert_eq!(
        harness.manager.open_session().unwrap_err(),
        ManagerError::SessionLimit { max: 2 }
    );
}

mod process_history;
