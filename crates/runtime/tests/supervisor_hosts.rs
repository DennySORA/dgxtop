//! Supervisor tests with REAL child processes (AT14, AT24, AT39 and the
//! IPC lifecycle of AT33).
//!
//! This binary doubles as the adapter host it supervises (the "test binary
//! as host" technique). It cannot use libtest: libtest prints
//! `running N tests` to stdout before any test body runs, which would corrupt
//! the IPC stream. `main` therefore dispatches on argv:
//!
//! * `--runtime-test-host <mode> --adapter <id> --host-generation <n>` (plus
//!   `DGXTOP_HOST_NONCE`): run as an adapter host in a fault mode;
//! * `--signal-probe`: install the termination flag and wait for a signal;
//! * otherwise: run every case sequentially (arguments not starting with `-`
//!   filter case names). Sequential execution keeps thread/fd counts exact.
//!
//! Only processes created by these tests are ever signalled or reaped.

// A test binary: panics are the failure signal and `unwrap` keeps the cases
// readable (libtest's `allow-*-in-tests` does not apply without libtest).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dgxtop_core::budget::{ByteLedger, MIB, Partition, QueueGate};
use dgxtop_core::clock::{TimeProvider, ms_to_ns};
use dgxtop_core::errors::InfraError;
use dgxtop_core::observation::{
    AdapterManifest, AdapterOperation, BatchStatus, CatalogDelta, DiscoverRequest, HelloTag,
    HostHello, IPC_PROTOCOL_VERSION, Isolation, ObservationBatch, ReadPlan,
};
use dgxtop_core::ports::{
    AdapterFailure, AdapterFailureKind, AdapterJob, Dispatch, DispatchError, IngestionCompletion,
    IngressBatch, TelemetryPort,
};
use dgxtop_core::quality::Namespace;
use dgxtop_core::testing::{MemoryLogger, ObservationBuilder, adapter_id, metric_key, read_group};
use dgxtop_core::view::{HostState, HostStatus};
use dgxtop_runtime::clock::BootClock;
use dgxtop_runtime::host::{host_generation_from_args, host_nonce_from_env};
use dgxtop_runtime::ipc::{FrameDecoder, encode_frame, read_frame_blocking};
use dgxtop_runtime::supervisor::{
    ChildControl, CircuitBreaker, ExitSummary, HostLauncher, LaunchRequest, LaunchedHost,
    SignalDelivery, StandardLauncher, StopOutcome, StopStage, backoff_seed,
};
use dgxtop_runtime::{
    HostLaunch, HostSpec, Supervisor, SupervisorConfig, SupervisorHandle, install_termination_flag,
    run_adapter_host,
};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use uuid::Uuid;

const HOST_MODE_FLAG: &str = "--runtime-test-host";
const ADAPTER_FLAG: &str = "--adapter";
const SIGNAL_PROBE_FLAG: &str = "--signal-probe";
const WAIT: Duration = Duration::from_secs(15);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let Some(mode) = flag_value(&args, HOST_MODE_FLAG) {
        return run_test_host(&mode, &args);
    }
    if args.iter().any(|arg| arg == SIGNAL_PROBE_FLAG) {
        return run_signal_probe();
    }
    run_suite(&args)
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

// ===================================================================
// Child side: fault-mode adapter hosts
// ===================================================================

fn run_test_host(mode: &str, args: &[String]) -> ExitCode {
    let adapter = flag_value(args, ADAPTER_FLAG).unwrap_or_else(|| "child".to_owned());
    let generation = host_generation_from_args(args).expect("host generation argument");
    let nonce = host_nonce_from_env().expect("host nonce");
    let clock: Arc<dyn TimeProvider> = Arc::new(BootClock::new().expect("boot clock"));
    match mode {
        "crash_after_hello" => {
            write_hello(&adapter, generation, &nonce);
            ExitCode::from(3)
        }
        "wrong_nonce" => {
            write_hello(&adapter, generation, "not-the-parent-nonce");
            wait_for_stdin_eof();
            ExitCode::SUCCESS
        }
        "garbage_frames" => {
            write_hello(&adapter, generation, &nonce);
            read_one_request();
            write_raw(&[0, 0, 0, 9]);
            write_raw(b"{\"type\":}");
            wait_for_stdin_eof();
            ExitCode::SUCCESS
        }
        "oversize_frame" => {
            write_hello(&adapter, generation, &nonce);
            read_one_request();
            write_raw(&u32::MAX.to_be_bytes());
            wait_for_stdin_eof();
            ExitCode::SUCCESS
        }
        _ => run_fake_adapter_host(mode, &adapter, generation, nonce, clock),
    }
}

fn run_fake_adapter_host(
    mode: &str,
    adapter: &str,
    generation: u32,
    nonce: String,
    clock: Arc<dyn TimeProvider>,
) -> ExitCode {
    if mode == "ignore_sigterm" {
        ignore_sigterm();
    }
    let behavior = match mode {
        "healthy" => Behavior::Healthy,
        "hang_on_read" | "ignore_sigterm" => Behavior::Sleep(Duration::from_secs(30)),
        "slow_once" => Behavior::SleepOnce(Duration::from_millis(900)),
        "slow_read" => Behavior::Sleep(Duration::from_millis(150)),
        "stderr_chatter" => Behavior::Chatter,
        other => panic!("unknown test host mode {other}"),
    };
    let fake = ChildAdapter {
        manifest: child_manifest(adapter),
        behavior,
        reads: AtomicU32::new(0),
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let exit = run_adapter_host(
        Box::new(fake),
        generation,
        nonce,
        clock,
        stdin.lock(),
        stdout.lock(),
    );
    ExitCode::from(u8::try_from(exit.exit_code()).unwrap_or(1))
}

#[allow(unsafe_code)]
fn ignore_sigterm() {
    // SAFETY: SIG_IGN installs no handler code; the disposition change is
    // exactly what this fault mode needs (a child that survives SIGTERM).
    unsafe { nix::sys::signal::signal(Signal::SIGTERM, nix::sys::signal::SigHandler::SigIgn) }
        .expect("ignore SIGTERM");
}

fn child_manifest(adapter: &str) -> AdapterManifest {
    AdapterManifest {
        api_version: 1,
        adapter_id: adapter_id(adapter),
        implementation: "ChildTestAdapter".into(),
        isolation: Isolation::PersistentProcess,
        operations: vec![
            AdapterOperation::Discover,
            AdapterOperation::Read,
            AdapterOperation::Shutdown,
        ],
        max_inflight: 1,
        runtime_library: None,
        minimum_sample_ms: 100,
        source_limit: 16,
        sample_namespace: Namespace::Live,
    }
}

fn write_hello(adapter: &str, generation: u32, nonce: &str) {
    let hello = HostHello {
        kind: HelloTag::Hello,
        protocol_version: IPC_PROTOCOL_VERSION,
        adapter_id: adapter_id(adapter),
        generation,
        nonce: nonce.to_owned(),
        manifest: child_manifest(adapter),
    };
    write_raw(&encode_frame(&hello, 1 << 20).unwrap());
}

fn write_raw(bytes: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes).unwrap();
    stdout.flush().unwrap();
}

fn read_one_request() {
    let mut decoder = FrameDecoder::new(64 * 1024).unwrap();
    let mut scratch = [0u8; 4096];
    let mut stdin = std::io::stdin().lock();
    let _ = read_frame_blocking(&mut stdin, &mut decoder, &mut scratch);
}

fn wait_for_stdin_eof() {
    let mut sink = [0u8; 4096];
    let mut stdin = std::io::stdin().lock();
    while matches!(stdin.read(&mut sink), Ok(read) if read > 0) {}
}

#[derive(Clone, Copy)]
enum Behavior {
    Healthy,
    Sleep(Duration),
    SleepOnce(Duration),
    Chatter,
}

struct ChildAdapter {
    manifest: AdapterManifest,
    behavior: Behavior,
    reads: AtomicU32,
}

impl TelemetryPort for ChildAdapter {
    fn manifest(&self) -> &AdapterManifest {
        &self.manifest
    }

    fn discover(&mut self, request: &DiscoverRequest) -> CatalogDelta {
        CatalogDelta {
            request_id: Some(request.request_id),
            adapter_id: Some(request.adapter_id.clone()),
            generation: request.previous_generation + 1,
            complete: true,
            ..CatalogDelta::default()
        }
    }

    fn read(&mut self, plan: &ReadPlan) -> ObservationBatch {
        let count = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        match self.behavior {
            Behavior::Healthy => {}
            Behavior::Sleep(delay) => std::thread::sleep(delay),
            Behavior::SleepOnce(delay) if count == 1 => std::thread::sleep(delay),
            Behavior::SleepOnce(_) => {}
            Behavior::Chatter => {
                let mut stderr = std::io::stderr().lock();
                let _ = writeln!(stderr, "chatter read {count} \u{1b}[31mred\u{1b}[0m");
            }
        }
        child_batch(plan, f64::from(count))
    }

    fn shutdown(&mut self) {}
}

fn child_batch(plan: &ReadPlan, value: f64) -> ObservationBatch {
    let records = plan
        .source_slots
        .iter()
        .map(|slot| {
            ObservationBuilder::new(
                metric_key("gpu0", "gpu.temperature", "device"),
                plan.adapter_id.as_str(),
                &format!("{}:slot{slot}", plan.adapter_id),
            )
            .f64(value)
            .cycle(plan.cycle_id)
            .build()
        })
        .collect();
    ObservationBatch {
        protocol_version: IPC_PROTOCOL_VERSION,
        adapter_id: plan.adapter_id.clone(),
        host_generation: plan.host_generation,
        catalog_revision: plan.catalog_revision,
        config_revision: plan.config_revision,
        request_id: plan.request_id,
        cycle_id: plan.cycle_id,
        read_group: plan.read_group.clone(),
        started_boot_ns: 0,
        finished_boot_ns: 0,
        records,
        status: BatchStatus::Ok,
        error_code: None,
    }
}

fn run_signal_probe() -> ExitCode {
    let flag = install_termination_flag().expect("install termination flag");
    write_raw(b"ready\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if flag.load(Ordering::SeqCst) {
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    ExitCode::from(1)
}

// ===================================================================
// Parent side: test runner and helpers
// ===================================================================

type Case = (&'static str, fn());

const CASES: &[Case] = &[
    (
        "healthy_round_trip_with_a_child_process",
        healthy_round_trip_with_a_child_process,
    ),
    (
        "hung_host_times_out_and_is_restarted_with_sigterm",
        hung_host_times_out_and_is_restarted_with_sigterm,
    ),
    (
        "sigterm_ignoring_host_is_killed_with_sigkill",
        sigterm_ignoring_host_is_killed_with_sigkill,
    ),
    (
        "other_host_keeps_serving_while_one_hangs",
        other_host_keeps_serving_while_one_hangs,
    ),
    (
        "late_reply_is_discarded_and_host_recovers",
        late_reply_is_discarded_and_host_recovers,
    ),
    (
        "circuit_backoff_follows_the_deterministic_schedule",
        circuit_backoff_follows_the_deterministic_schedule,
    ),
    (
        "unreaped_child_is_quarantined_and_never_respawned",
        unreaped_child_is_quarantined_and_never_respawned,
    ),
    (
        "identical_reads_cause_one_host_request",
        identical_reads_cause_one_host_request,
    ),
    (
        "threads_and_fds_do_not_grow_with_requests",
        threads_and_fds_do_not_grow_with_requests,
    ),
    (
        "shutdown_is_bounded_with_stubborn_hosts",
        shutdown_is_bounded_with_stubborn_hosts,
    ),
    (
        "protocol_violations_restart_the_host",
        protocol_violations_restart_the_host,
    ),
    (
        "wrong_nonce_hello_is_rejected",
        wrong_nonce_hello_is_rejected,
    ),
    (
        "child_stderr_is_forwarded_sanitized",
        child_stderr_is_forwarded_sanitized,
    ),
    (
        "termination_signals_set_the_flag",
        termination_signals_set_the_flag,
    ),
];

/// What the command line selects, read like libtest's flags: the cases run
/// only with `--ignored` or `--include-ignored` (real child processes:
/// end-to-end, opt-in); `--skip NAME` and bare words filter by name.
struct Selection<'a> {
    opted_in: bool,
    filters: Vec<&'a str>,
    skipped: Vec<&'a str>,
}

impl<'a> Selection<'a> {
    fn parse(args: &'a [String]) -> Self {
        let mut selection = Self {
            opted_in: false,
            filters: Vec::new(),
            skipped: Vec::new(),
        };
        let mut words = args.iter().skip(1).map(String::as_str);
        while let Some(word) = words.next() {
            match word {
                "--ignored" | "--include-ignored" => selection.opted_in = true,
                "--skip" => selection.skipped.extend(words.next()),
                flag if flag.starts_with('-') => {}
                filter => selection.filters.push(filter),
            }
        }
        selection
    }

    fn selects(&self, name: &str) -> bool {
        (self.filters.is_empty() || self.filters.iter().any(|filter| name.contains(filter)))
            && !self.skipped.iter().any(|skip| name.contains(skip))
    }
}

fn run_suite(args: &[String]) -> ExitCode {
    let selection = Selection::parse(args);
    if !selection.opted_in {
        println!(
            "\nrunning 0 supervisor host tests\n\ntest result: ok. 0 passed; 0 failed; {} ignored (end-to-end, opt-in: --ignored)",
            CASES.len()
        );
        return ExitCode::SUCCESS;
    }
    let selected: Vec<&Case> = CASES
        .iter()
        .filter(|(name, _)| selection.selects(name))
        .collect();
    println!("\nrunning {} supervisor host tests", selected.len());
    let suite_started = Instant::now();
    let mut failed = Vec::new();
    for (name, case) in selected.iter().copied() {
        let started = Instant::now();
        let passed = catch_unwind(AssertUnwindSafe(case)).is_ok();
        println!(
            "test {name} ... {} ({} ms)",
            if passed { "ok" } else { "FAILED" },
            started.elapsed().as_millis()
        );
        if !passed {
            failed.push(*name);
        }
    }
    let passed = selected.len() - failed.len();
    println!(
        "\ntest result: {}. {passed} passed; {} failed; finished in {:.2}s",
        if failed.is_empty() { "ok" } else { "FAILED" },
        failed.len(),
        suite_started.elapsed().as_secs_f64()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        println!("failures: {failed:?}");
        ExitCode::from(101)
    }
}

fn host_spec(adapter: &str, mode: &str) -> HostSpec {
    HostSpec {
        adapter_id: adapter_id(adapter),
        launch: HostLaunch::ChildProcess {
            program: current_exe(),
            args: vec![
                HOST_MODE_FLAG.to_owned(),
                mode.to_owned(),
                ADAPTER_FLAG.to_owned(),
                adapter.to_owned(),
            ],
        },
    }
}

fn current_exe() -> PathBuf {
    std::env::current_exe().expect("current test binary")
}

fn fast_config() -> SupervisorConfig {
    SupervisorConfig {
        child_grace_ms: 100,
        child_term_ms: 200,
        child_reap_ms: 1000,
        backoff_base_ms: 100,
        max_backoff_ms: 400,
        hello_timeout_ms: 10_000,
        probe_deadline_ms: 5_000,
        ..SupervisorConfig::default()
    }
}

struct Harness {
    handle: SupervisorHandle,
    completions: Receiver<IngestionCompletion>,
    ledger: ByteLedger,
    logger: Arc<MemoryLogger>,
    clock: Arc<dyn TimeProvider>,
}

fn start(config: SupervisorConfig, specs: Vec<HostSpec>) -> Harness {
    start_with(config, specs, Arc::new(StandardLauncher))
}

fn start_with(
    config: SupervisorConfig,
    specs: Vec<HostSpec>,
    launcher: Arc<dyn HostLauncher>,
) -> Harness {
    let ledger = ByteLedger::for_memory_limit_mib(256);
    let gate = QueueGate::new("ingress", 32, 8 * MIB, ledger.clone(), Partition::Ingress);
    let (sender, completions) = crossbeam_channel::bounded(1024);
    let clock: Arc<dyn TimeProvider> = Arc::new(BootClock::new().unwrap());
    let logger = Arc::new(MemoryLogger::default());
    let handle = Supervisor::start_with_launcher(
        config,
        specs,
        ledger.clone(),
        gate,
        sender,
        Arc::clone(&clock),
        logger.clone(),
        launcher,
    )
    .unwrap();
    Harness {
        handle,
        completions,
        ledger,
        logger,
        clock,
    }
}

impl Harness {
    fn wait_for(
        &self,
        what: &str,
        mut matches: impl FnMut(&IngestionCompletion) -> bool,
    ) -> IngestionCompletion {
        let deadline = Instant::now() + WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.completions.recv_timeout(remaining) {
                Ok(completion) if matches(&completion) => return completion,
                Ok(_) => {}
                Err(_) => panic!(
                    "timed out waiting for {what}; statuses: {:?}",
                    self.handle.statuses()
                ),
            }
        }
    }

    fn wait_status(&self, what: &str, mut matches: impl FnMut(&HostStatus) -> bool) -> HostStatus {
        match self.wait_for(what, |completion| {
            matches!(completion, IngestionCompletion::HostStatus(status) if matches(status))
        }) {
            IngestionCompletion::HostStatus(status) => status,
            _ => unreachable!(),
        }
    }

    fn wait_state(&self, adapter: &str, state: HostState) -> HostStatus {
        self.wait_status(&format!("{adapter} {state:?}"), |status| {
            status.adapter_id == adapter && status.state == state
        })
    }

    /// Waits until every adapter reported `Healthy`. Statuses of different
    /// hosts arrive in no fixed order, and `wait_for` drops what it skips.
    fn wait_all_healthy(&self, adapters: &[&str]) {
        let mut pending = adapters.to_vec();
        self.wait_for(&format!("{adapters:?} Healthy"), |completion| {
            if let IngestionCompletion::HostStatus(status) = completion
                && status.state == HostState::Healthy
            {
                pending.retain(|adapter| status.adapter_id != *adapter);
            }
            pending.is_empty()
        });
    }

    fn plan(&self, adapter: &str, generation: u32, cycle: u64, deadline_ms: u64) -> ReadPlan {
        ReadPlan {
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id(adapter),
            host_generation: generation,
            catalog_revision: 1,
            config_revision: 1,
            cycle_id: cycle,
            read_group: read_group("fast"),
            source_slots: vec![1, 2],
            deadline_boot_ns: self.clock.now_boot_ns() + ms_to_ns(deadline_ms),
            epoch: 1,
            page: 0,
        }
    }

    fn discover(&self, adapter: &str, generation: u32) -> DiscoverRequest {
        DiscoverRequest {
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id(adapter),
            host_generation: generation,
            catalog_revision: 1,
            config_revision: 1,
            deadline_boot_ns: self.clock.now_boot_ns() + ms_to_ns(5_000),
            previous_generation: 0,
            page: 0,
        }
    }

    fn batch(&self, request_id: Uuid) -> Result<Arc<IngressBatch>, AdapterFailure> {
        match self.wait_for("batch", |completion| {
            matches!(completion, IngestionCompletion::Batch { request_id: id, .. } if *id == request_id)
        }) {
            IngestionCompletion::Batch { result, .. } => result,
            _ => unreachable!(),
        }
    }

    fn dispatch(&self, job: AdapterJob) {
        let deadline = Instant::now() + WAIT;
        loop {
            match self.handle.try_dispatch(job.clone()) {
                Ok(()) => return,
                Err(DispatchError::Busy) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("dispatch failed: {error:?}"),
            }
        }
    }

    fn wait_detail(&self, adapter: &str, fragment: &str) -> HostStatus {
        self.wait_status(&format!("{adapter} detail {fragment:?}"), |status| {
            status.adapter_id == adapter
                && status
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains(fragment))
        })
    }

    fn wait_log(&self, adapter: &str, fragment: &str) {
        let deadline = Instant::now() + WAIT;
        while !self
            .logger
            .records()
            .iter()
            .any(|(_, _, message)| message.starts_with(adapter) && message.contains(fragment))
        {
            assert!(
                Instant::now() < deadline,
                "{adapter} never logged {fragment:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    stat[close + 1..].split_whitespace().nth(1)?.parse().ok()
}

fn own_pid() -> i32 {
    i32::try_from(std::process::id()).unwrap()
}

fn count_entries(path: &str) -> usize {
    std::fs::read_dir(path).unwrap().count()
}

// ===================================================================
// Cases
// ===================================================================

fn healthy_round_trip_with_a_child_process() {
    let harness = start(fast_config(), vec![host_spec("child", "healthy")]);
    let healthy = harness.wait_state("child", HostState::Healthy);
    let pid = healthy.pid.expect("child pid");
    assert_eq!(parent_of(pid), Some(own_pid()), "host is our managed child");
    assert_eq!(harness.handle.managed_pids(), vec![pid]);
    let discover = harness.discover("child", 1);
    harness.dispatch(AdapterJob::Discover(discover.clone()));
    let catalog = harness.wait_for("catalog", |completion| {
        matches!(completion, IngestionCompletion::Catalog { request_id, .. } if *request_id == discover.request_id)
    });
    assert!(
        matches!(catalog, IngestionCompletion::Catalog { result: Ok(ref delta), .. } if delta.complete)
    );
    let plan = harness.plan("child", 1, 1, 400);
    harness.dispatch(AdapterJob::Read(plan.clone()));
    let batch = harness.batch(plan.request_id).unwrap();
    assert_eq!(batch.records.len(), 2);
    assert!(batch.accounted_bytes() > 0);
    assert!(harness.ledger.used(Partition::Ingress) >= batch.accounted_bytes());
    drop(batch);
    let released_by = Instant::now() + WAIT;
    while harness.ledger.used(Partition::Ingress) != 0 {
        assert!(Instant::now() < released_by, "ingress bytes never released");
        std::thread::sleep(Duration::from_millis(5));
    }
    let report = harness.handle.shutdown(Duration::from_secs(3));
    assert!(report.completed_within_deadline);
    assert_eq!(
        report.hosts[0].outcome,
        StopOutcome::Reaped {
            stage: StopStage::Protocol,
            exit: ExitSummary::Exited(0)
        }
    );
    assert!(harness.handle.managed_pids().is_empty());
}

fn hung_host_times_out_and_is_restarted_with_sigterm() {
    let harness = start(fast_config(), vec![host_spec("hung", "hang_on_read")]);
    let first = harness.wait_state("hung", HostState::Healthy);
    let plan = harness.plan("hung", 1, 1, 300);
    harness.dispatch(AdapterJob::Read(plan.clone()));
    let started = Instant::now();
    let failure = harness.batch(plan.request_id).unwrap_err();
    assert_eq!(failure.kind, AdapterFailureKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
    // Continued silence reaches the threshold; protocol shutdown cannot be
    // served by a host stuck in its driver call, so SIGTERM ends it.
    harness.wait_detail("hung", "stopped after sigterm");
    let probing = harness.wait_status("half-open generation 2", |status| status.generation == 2);
    assert_eq!(probing.restarts_total, 1);
    let healthy = harness.wait_state("hung", HostState::Healthy);
    assert_eq!(healthy.generation, 2);
    assert_ne!(healthy.pid, first.pid);
    assert_eq!(healthy.consecutive_failures, 0);
    assert!(healthy.timeouts_total >= 1);
    harness.handle.shutdown(Duration::from_secs(3));
}

fn sigterm_ignoring_host_is_killed_with_sigkill() {
    let harness = start(fast_config(), vec![host_spec("stubborn", "ignore_sigterm")]);
    harness.wait_state("stubborn", HostState::Healthy);
    let plan = harness.plan("stubborn", 1, 1, 200);
    harness.dispatch(AdapterJob::Read(plan.clone()));
    assert_eq!(
        harness.batch(plan.request_id).unwrap_err().kind,
        AdapterFailureKind::TimedOut
    );
    let killed = harness.wait_status("stop outcome", |status| {
        status
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("stopped after sigkill: signal 9"))
    });
    assert_eq!(killed.generation, 1);
    harness.wait_status("generation 2", |status| status.generation == 2);
    harness.handle.shutdown(Duration::from_secs(3));
}

fn other_host_keeps_serving_while_one_hangs() {
    let harness = start(
        fast_config(),
        vec![
            host_spec("nvml", "hang_on_read"),
            host_spec("procfs", "healthy"),
        ],
    );
    harness.wait_all_healthy(&["nvml", "procfs"]);
    let hung = harness.plan("nvml", 1, 1, 3_000);
    harness.dispatch(AdapterJob::Read(hung.clone()));
    for cycle in 1..=10 {
        let plan = harness.plan("procfs", 1, cycle, 1_500);
        let started = Instant::now();
        harness.dispatch(AdapterJob::Read(plan.clone()));
        let batch = harness
            .batch(plan.request_id)
            .expect("healthy host keeps serving");
        assert_eq!(batch.records.len(), 2);
        assert!(started.elapsed() < Duration::from_millis(1_500));
        // The hung backend stays busy; nothing is queued behind it.
        assert_eq!(
            harness.handle.try_dispatch(AdapterJob::Read(harness.plan(
                "nvml",
                1,
                100 + cycle,
                500
            ))),
            Err(DispatchError::Busy)
        );
    }
    assert_eq!(
        harness.batch(hung.request_id).unwrap_err().kind,
        AdapterFailureKind::TimedOut
    );
    let procfs = harness
        .handle
        .statuses()
        .into_iter()
        .find(|status| status.adapter_id == "procfs")
        .unwrap();
    assert_eq!(procfs.state, HostState::Healthy);
    assert_eq!(procfs.restarts_total, 0);
    harness.handle.shutdown(Duration::from_secs(3));
}

fn late_reply_is_discarded_and_host_recovers() {
    // Each deadline-length of silence after a timeout is another failure;
    // a threshold of 5 keeps the restart (at 5 x 300 ms) well after the
    // late reply (900 ms) so the test observes the discard, not a restart.
    let config = SupervisorConfig {
        failure_threshold: 5,
        ..fast_config()
    };
    let harness = start(config, vec![host_spec("slow", "slow_once")]);
    harness.wait_state("slow", HostState::Healthy);
    let plan = harness.plan("slow", 1, 1, 300);
    harness.dispatch(AdapterJob::Read(plan.clone()));
    assert_eq!(
        harness.batch(plan.request_id).unwrap_err().kind,
        AdapterFailureKind::TimedOut
    );
    let recovered = harness.plan("slow", 1, 2, 2_000);
    harness.dispatch(AdapterJob::Read(recovered.clone()));
    let batch = harness.batch(recovered.request_id).unwrap();
    assert_eq!(
        batch.request_id, recovered.request_id,
        "never the late reply"
    );
    let status = harness.wait_state("slow", HostState::Healthy);
    assert_eq!(status.generation, 1, "a late reply is not a restart");
    harness.handle.shutdown(Duration::from_secs(3));
}

fn circuit_backoff_follows_the_deterministic_schedule() {
    let config = SupervisorConfig {
        failure_threshold: 1,
        ..fast_config()
    };
    let mut expected = CircuitBreaker::new(1, 100, 400, 100, backoff_seed("crashy"));
    let harness = start(config, vec![host_spec("crashy", "crash_after_hello")]);
    for attempt in 0..3 {
        let open = harness.wait_status("circuit open", |status| {
            status.state == HostState::CircuitOpen && status.backoff_until_boot_ns.is_some()
        });
        let detail = open.detail.unwrap_or_default();
        let delay: u64 = detail
            .strip_prefix("circuit open for ")
            .and_then(|rest| rest.split(' ').next())
            .and_then(|number| number.parse().ok())
            .unwrap_or_else(|| panic!("unexpected detail {detail}"));
        assert_eq!(delay, expected.next_delay_ms(), "attempt {attempt}");
        let nominal = 100u64 << attempt.min(2);
        assert!(delay >= nominal - nominal / 10 && delay <= nominal + nominal / 10);
    }
    let restarts = harness.handle.statuses()[0].restarts_total;
    assert!(restarts >= 2, "each half-open probe uses a new generation");
    harness.handle.shutdown(Duration::from_secs(3));
}

/// Launcher whose control never reports a reap (an unkillable child stand-in).
struct NeverReapedLauncher {
    pids: Arc<Mutex<Vec<i32>>>,
}

struct NeverReaped {
    inner: Box<dyn ChildControl>,
}

impl ChildControl for NeverReaped {
    fn pid(&self) -> Option<i32> {
        self.inner.pid()
    }

    fn terminate(&mut self) -> Result<SignalDelivery, InfraError> {
        self.inner.terminate()
    }

    fn kill(&mut self) -> Result<SignalDelivery, InfraError> {
        self.inner.kill()
    }

    fn try_reap(&mut self) -> Result<Option<dgxtop_runtime::supervisor::ExitSummary>, InfraError> {
        Ok(None)
    }
}

impl HostLauncher for NeverReapedLauncher {
    fn launch(&self, request: &LaunchRequest<'_>) -> Result<LaunchedHost, InfraError> {
        let mut launched = StandardLauncher.launch(request)?;
        if request.adapter_id.as_str() == "stuck" {
            if let Some(pid) = launched.control.pid() {
                self.pids.lock().unwrap().push(pid);
            }
            launched.control = Box::new(NeverReaped {
                inner: launched.control,
            });
        }
        Ok(launched)
    }
}

fn reap_test_children(pids: &[i32]) {
    for pid in pids {
        // These are children created by this test; reaping them here only
        // cleans up the zombies the fake control refused to collect.
        let _ = nix::sys::wait::waitpid(Pid::from_raw(*pid), None);
    }
}

fn unreaped_child_is_quarantined_and_never_respawned() {
    let pids = Arc::new(Mutex::new(Vec::new()));
    let config = SupervisorConfig {
        failure_threshold: 1,
        max_hosts: 2,
        child_reap_ms: 200,
        ..fast_config()
    };
    let harness = start_with(
        config,
        vec![
            host_spec("stuck", "hang_on_read"),
            host_spec("procfs", "healthy"),
        ],
        Arc::new(NeverReapedLauncher {
            pids: Arc::clone(&pids),
        }),
    );
    harness.wait_all_healthy(&["stuck", "procfs"]);
    let plan = harness.plan("stuck", 1, 1, 200);
    harness.dispatch(AdapterJob::Read(plan.clone()));
    assert_eq!(
        harness.batch(plan.request_id).unwrap_err().kind,
        AdapterFailureKind::TimedOut
    );
    let quarantined = harness.wait_state("stuck", HostState::Quarantined);
    let stuck_pid = quarantined.pid.expect("quarantined pid stays visible");
    assert_eq!(pids.lock().unwrap().as_slice(), &[stuck_pid]);
    // Still counted: live + quarantined hosts.
    let managed = harness.handle.managed_pids();
    assert_eq!(managed.len(), 2);
    assert!(managed.contains(&stuck_pid));
    std::thread::sleep(Duration::from_millis(600));
    let stuck = harness.handle.statuses()[0].clone();
    assert_eq!(stuck.state, HostState::Quarantined);
    assert_eq!(stuck.generation, 1, "never respawned");
    assert_eq!(pids.lock().unwrap().len(), 1, "no replacement child");
    assert!(
        stuck
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("not reaped")),
        "{stuck:?}"
    );
    // The other backend is unaffected.
    let plan = harness.plan("procfs", 1, 2, 1_000);
    harness.dispatch(AdapterJob::Read(plan.clone()));
    assert!(harness.batch(plan.request_id).is_ok());
    let begin = Instant::now();
    let report = harness.handle.shutdown(Duration::from_secs(2));
    assert!(begin.elapsed() < Duration::from_millis(2_500));
    assert_eq!(
        report.unreaped(),
        vec![(adapter_id("stuck"), Some(stuck_pid))]
    );
    reap_test_children(&pids.lock().unwrap());
}

fn identical_reads_cause_one_host_request() {
    let harness = start(fast_config(), vec![host_spec("dedup", "slow_read")]);
    harness.wait_state("dedup", HostState::Healthy);
    let plan = harness.plan("dedup", 1, 7, 3_000);
    let mut duplicate = plan.clone();
    duplicate.request_id = Uuid::new_v4();
    harness.dispatch(AdapterJob::Read(plan.clone()));
    harness
        .handle
        .try_dispatch(AdapterJob::Read(duplicate.clone()))
        .unwrap();
    let first = harness.batch(plan.request_id).unwrap();
    let joined = harness.batch(duplicate.request_id).unwrap();
    assert!(Arc::ptr_eq(&first, &joined));
    // The child numbers its reads: the shared batch is read #1.
    let value = first.records[0].value.unwrap().to_f64_lossy();
    assert!((value - 1.0).abs() < f64::EPSILON);
    let next = harness.plan("dedup", 1, 8, 3_000);
    harness.dispatch(AdapterJob::Read(next.clone()));
    let second = harness.batch(next.request_id).unwrap();
    let value = second.records[0].value.unwrap().to_f64_lossy();
    assert!(
        (value - 2.0).abs() < f64::EPSILON,
        "exactly one host read per key"
    );
    harness.handle.shutdown(Duration::from_secs(3));
}

fn threads_and_fds_do_not_grow_with_requests() {
    let harness = start(fast_config(), vec![host_spec("steady", "healthy")]);
    harness.wait_state("steady", HostState::Healthy);
    // One warm-up request so lazily created state exists before counting.
    let warmup = harness.plan("steady", 1, 0, 2_000);
    harness.dispatch(AdapterJob::Read(warmup.clone()));
    assert!(harness.batch(warmup.request_id).is_ok());
    let threads_before = count_entries("/proc/self/task");
    let fds_before = count_entries("/proc/self/fd");
    for cycle in 1..=200 {
        let plan = harness.plan("steady", 1, cycle, 2_000);
        harness.dispatch(AdapterJob::Read(plan.clone()));
        assert!(harness.batch(plan.request_id).is_ok());
    }
    let threads_after = count_entries("/proc/self/task");
    let fds_after = count_entries("/proc/self/fd");
    assert!(
        threads_after <= threads_before,
        "threads grew {threads_before} -> {threads_after}"
    );
    assert!(
        fds_after <= fds_before,
        "fds grew {fds_before} -> {fds_after}"
    );
    harness.handle.shutdown(Duration::from_secs(3));
}

fn shutdown_is_bounded_with_stubborn_hosts() {
    // Default production timings (200/200/1000 ms) are scaled down to fit.
    let config = SupervisorConfig {
        child_grace_ms: 200,
        child_term_ms: 200,
        child_reap_ms: 1000,
        ..fast_config()
    };
    let harness = start(config, vec![host_spec("stubborn", "ignore_sigterm")]);
    harness.wait_state("stubborn", HostState::Healthy);
    let plan = harness.plan("stubborn", 1, 1, 10_000);
    harness.dispatch(AdapterJob::Read(plan.clone()));
    std::thread::sleep(Duration::from_millis(50));
    let begin = Instant::now();
    let report = harness.handle.shutdown(Duration::from_millis(400));
    let elapsed = begin.elapsed();
    assert!(
        elapsed < Duration::from_millis(700),
        "shutdown took {elapsed:?}"
    );
    assert!(report.completed_within_deadline, "{report:?}");
    assert!(matches!(
        report.hosts[0].outcome,
        StopOutcome::Reaped {
            stage: StopStage::Kill,
            ..
        }
    ));
    assert_eq!(
        harness.batch(plan.request_id).unwrap_err().kind,
        AdapterFailureKind::Shutdown
    );

    let pids = Arc::new(Mutex::new(Vec::new()));
    let stuck = start_with(
        fast_config(),
        vec![host_spec("stuck", "hang_on_read")],
        Arc::new(NeverReapedLauncher {
            pids: Arc::clone(&pids),
        }),
    );
    stuck.wait_state("stuck", HostState::Healthy);
    let begin = Instant::now();
    let report = stuck.handle.shutdown(Duration::from_millis(300));
    assert!(begin.elapsed() < Duration::from_millis(600));
    assert_eq!(report.unreaped().len(), 1, "{report:?}");
    reap_test_children(&pids.lock().unwrap());
}

fn protocol_violations_restart_the_host() {
    for mode in ["garbage_frames", "oversize_frame"] {
        let harness = start(fast_config(), vec![host_spec("broken", mode)]);
        harness.wait_state("broken", HostState::Healthy);
        let plan = harness.plan("broken", 1, 1, 2_000);
        harness.dispatch(AdapterJob::Read(plan.clone()));
        let failure = harness.batch(plan.request_id).unwrap_err();
        assert_eq!(failure.kind, AdapterFailureKind::Protocol, "{mode}");
        harness.wait_log("broken", "protocol error");
        let restarted = harness.wait_status("restart", |status| status.generation == 2);
        assert_eq!(restarted.restarts_total, 1, "{mode}");
        harness.handle.shutdown(Duration::from_secs(3));
    }
}

fn wrong_nonce_hello_is_rejected() {
    let harness = start(fast_config(), vec![host_spec("impostor", "wrong_nonce")]);
    let rejected = harness.wait_status("rejected hello", |status| {
        status
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("nonce"))
    });
    assert_ne!(rejected.state, HostState::Healthy);
    harness.wait_log("impostor", "protocol error");
    assert!(
        harness
            .handle
            .statuses()
            .iter()
            .all(|status| status.state != HostState::Healthy)
    );
    harness.handle.shutdown(Duration::from_secs(3));
}

fn child_stderr_is_forwarded_sanitized() {
    let harness = start(fast_config(), vec![host_spec("chatty", "stderr_chatter")]);
    harness.wait_state("chatty", HostState::Healthy);
    let plan = harness.plan("chatty", 1, 1, 2_000);
    harness.dispatch(AdapterJob::Read(plan.clone()));
    assert!(harness.batch(plan.request_id).is_ok());
    let deadline = Instant::now() + WAIT;
    loop {
        let forwarded = harness
            .logger
            .records()
            .into_iter()
            .any(|(_, target, message)| {
                target == "adapter_host" && message == "chatty: chatter read 1 red"
            });
        if forwarded {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stderr line not forwarded: {:?}",
            harness.logger.records()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    harness.handle.shutdown(Duration::from_secs(3));
}

fn termination_signals_set_the_flag() {
    for signal in [Signal::SIGTERM, Signal::SIGHUP, Signal::SIGINT] {
        let mut child = Command::new(current_exe())
            .arg(SIGNAL_PROBE_FLAG)
            .stdout(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        // Signal only the probe process this test created.
        kill(Pid::from_raw(i32::try_from(child.id()).unwrap()), signal).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "probe did not exit");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(status.code(), Some(0), "{signal} did not set the flag");
    }
}
