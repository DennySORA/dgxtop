//! pidfd action adapter against DISPOSABLE children spawned by these tests
//! (`sleep`). No other process is ever signalled: PID 1 and our own PID are
//! refused by policy before any system call, and every child is killed and
//! reaped by its guard even when an assertion fails.

// Test-only crate: a broken fixture must panic loudly, also in helpers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use dgxtop_adapters::process_action::{
    HostProcessFiles, LinuxPidfd, MAX_HELD_HANDLES, PidfdActionAdapter, PidfdReadiness,
    PidfdSyscalls, ProcessFiles, parse_stat,
};
use dgxtop_core::action::{
    ActionError, CapabilityContext, DurableReceipt, ExitObservation, Signal, SignalOutcome,
};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{BootId, HostId, Pid, PidNamespace, ProcessKey, SessionId};
use dgxtop_core::ports::ProcessActionPort;
use dgxtop_core::testing::FakeClock;
use nix::errno::Errno;
use uuid::Uuid;

// ---------------------------------------------------------------- fixtures

/// A child this test owns; always killed (SIGKILL to OUR child) and reaped.
struct Disposable {
    child: Child,
}

impl Disposable {
    fn sleep(seconds: &str) -> Self {
        let child = Command::new("sleep")
            .arg(seconds)
            .spawn()
            .expect("spawn disposable sleep");
        Self { child }
    }

    fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).unwrap()
    }

    fn is_alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Waits until the child has exec'd `sleep` (comm changes after exec).
    fn wait_for_exec(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let comm =
                std::fs::read_to_string(format!("/proc/{}/comm", self.pid())).unwrap_or_default();
            if comm.trim() == "sleep" {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("child never exec'd sleep");
    }
}

impl Drop for Disposable {
    fn drop(&mut self) {
        // `kill` is a no-op once the child was reaped, so no stray signal can
        // reach a recycled PID.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn euid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

fn running_as_root() -> bool {
    if euid() == 0 {
        eprintln!("skipped: needs a non-root test process");
        return true;
    }
    false
}

fn boot_id() -> BootId {
    let text = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    BootId::new(text.trim()).unwrap()
}

fn namespace_of(pid: i32) -> PidNamespace {
    let target = std::fs::read_link(format!("/proc/{pid}/ns/pid")).unwrap();
    PidNamespace::new(target.to_string_lossy().into_owned()).unwrap()
}

fn key_of(pid: i32) -> ProcessKey {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    ProcessKey {
        host_id: HostId::new("test-host").unwrap(),
        boot_id: boot_id(),
        pid_namespace: namespace_of(pid),
        pid: Pid::new(i64::from(pid)).unwrap(),
        start_time_ticks: parse_stat(&stat).unwrap().start_time_ticks,
    }
}

fn context() -> CapabilityContext {
    let own = i32::try_from(std::process::id()).unwrap();
    CapabilityContext {
        host_id: HostId::new("test-host").unwrap(),
        boot_id: boot_id(),
        pid_namespace: namespace_of(own),
        session_id: SessionId::new("session-1").unwrap(),
        acting_uid: euid(),
        any_user: false,
        policy_revision: 1,
        actions_enabled: true,
        read_only: false,
        allow_force: false,
        protected_pids: Vec::new(),
    }
}

fn adapter() -> (PidfdActionAdapter, Arc<FakeClock>) {
    let clock = FakeClock::new(5_000_000_000);
    let shared: Arc<dyn TimeProvider> = clock.clone();
    (PidfdActionAdapter::new(shared), clock)
}

fn receipt(clock: &FakeClock) -> DurableReceipt {
    DurableReceipt {
        request_id: Uuid::new_v4(),
        intent_id: Uuid::new_v4(),
        committed_boot_ns: clock.now_boot_ns(),
    }
}

fn wait_for_exit_observation(
    adapter: &mut PidfdActionAdapter,
    handle: dgxtop_core::action::ActionHandle,
) -> ExitObservation {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let observation = adapter.observe_exit(handle);
        if observation != ExitObservation::Running || Instant::now() > deadline {
            return observation;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Real pidfd opening/polling, but a scripted send result: lets errno
/// classification be tested without delivering any signal.
struct ScriptedSend {
    result: Result<(), Errno>,
    sends: Arc<AtomicU64>,
}

impl PidfdSyscalls for ScriptedSend {
    fn open(&self, pid: i32) -> Result<OwnedFd, Errno> {
        LinuxPidfd.open(pid)
    }

    fn send_signal(&self, _pidfd: BorrowedFd<'_>, _signal: i32) -> Result<(), Errno> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        self.result
    }

    fn readiness(&self, pidfd: BorrowedFd<'_>) -> Result<PidfdReadiness, Errno> {
        LinuxPidfd.readiness(pidfd)
    }
}

/// A kernel without pidfd support.
struct NoPidfdKernel(Errno);

impl PidfdSyscalls for NoPidfdKernel {
    fn open(&self, _pid: i32) -> Result<OwnedFd, Errno> {
        Err(self.0)
    }

    fn send_signal(&self, _pidfd: BorrowedFd<'_>, _signal: i32) -> Result<(), Errno> {
        panic!("no signal may be sent without a pidfd");
    }

    fn readiness(&self, _pidfd: BorrowedFd<'_>) -> Result<PidfdReadiness, Errno> {
        Err(Errno::ENOSYS)
    }
}

/// Host `/proc` with injectable faults.
#[derive(Clone, Default)]
struct ScriptedFiles {
    stat_override: Arc<Mutex<Option<String>>>,
    status_fails: bool,
    fdinfo_override: Option<String>,
    /// The PID namespace the target's own `/proc/<pid>/ns/pid` names.
    namespace_override: Option<String>,
}

impl ProcessFiles for ScriptedFiles {
    fn stat(&self, pid: i32) -> std::io::Result<String> {
        if let Some(text) = self
            .stat_override
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Ok(text);
        }
        HostProcessFiles.stat(pid)
    }

    fn status(&self, pid: i32) -> std::io::Result<String> {
        if self.status_fails {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        HostProcessFiles.status(pid)
    }

    fn pid_namespace(&self, pid: i32) -> std::io::Result<String> {
        match &self.namespace_override {
            Some(namespace) => Ok(namespace.clone()),
            None => HostProcessFiles.pid_namespace(pid),
        }
    }

    fn descriptor_info(&self, fd: RawFd) -> std::io::Result<String> {
        match &self.fdinfo_override {
            Some(text) => Ok(text.clone()),
            None => HostProcessFiles.descriptor_info(fd),
        }
    }
}

fn scripted_adapter(
    syscalls: Box<dyn PidfdSyscalls>,
    files: ScriptedFiles,
) -> (PidfdActionAdapter, Arc<FakeClock>) {
    let clock = FakeClock::new(5_000_000_000);
    let shared: Arc<dyn TimeProvider> = clock.clone();
    (
        PidfdActionAdapter::with_parts(shared, syscalls, Box::new(files)),
        clock,
    )
}

// ---------------------------------------------------------------- AT10

#[test]
fn prepare_send_sigterm_and_observe_exit() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("30");
    child.wait_for_exec();
    let (mut adapter, clock) = adapter();
    let key = key_of(child.pid());
    let prepared = adapter.prepare(&key, Signal::Sigterm, &context()).unwrap();
    assert_eq!(prepared.verified.key, key);
    assert_eq!(prepared.verified.uid, euid());
    assert_eq!(prepared.verified.basename, "sleep");
    assert!(prepared.display_summary.contains("SIGTERM"));
    assert!(
        prepared
            .display_summary
            .contains(&format!("pid {}", child.pid()))
    );
    assert_eq!(adapter.held_count(), 1);
    assert_eq!(
        adapter.observe_exit(prepared.handle),
        ExitObservation::Running
    );
    assert!(child.is_alive(), "prepare never signals");

    clock.advance_ms(10);
    let outcome = adapter.send(prepared.handle, Signal::Sigterm, &receipt(&clock));
    assert_eq!(outcome, SignalOutcome::Sent);
    assert_eq!(
        wait_for_exit_observation(&mut adapter, prepared.handle),
        ExitObservation::ExitObserved,
        "signal_sent and exit_observed are separate observations"
    );
    let status = child.child.wait().unwrap();
    assert_eq!(status.signal(), Some(Signal::Sigterm.number()));
    // At most one signal per handle.
    assert!(matches!(
        adapter.send(prepared.handle, Signal::Sigterm, &receipt(&clock)),
        SignalOutcome::NotSent { .. }
    ));
    adapter.release(prepared.handle);
    assert_eq!(adapter.held_count(), 0);
}

#[test]
fn wrong_start_time_is_identity_changed_and_child_is_untouched() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("30");
    let (mut adapter, _clock) = adapter();
    let mut key = key_of(child.pid());
    key.start_time_ticks += 1;
    assert_eq!(
        adapter.prepare(&key, Signal::Sigterm, &context()),
        Err(ActionError::IdentityChanged)
    );
    assert_eq!(adapter.held_count(), 0, "descriptor closed on rejection");
    std::thread::sleep(Duration::from_millis(50));
    assert!(child.is_alive(), "zero signals on identity mismatch");
}

#[test]
fn exited_child_is_reported_exited_at_prepare() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("0.2");
    let key = key_of(child.pid());
    child.child.wait().unwrap();
    let (mut adapter, _clock) = adapter();
    assert_eq!(
        adapter.prepare(&key, Signal::Sigterm, &context()),
        Err(ActionError::Exited)
    );
    assert_eq!(adapter.held_count(), 0);
}

#[test]
fn send_after_the_process_exited_reports_exited() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("0.5");
    child.wait_for_exec();
    let (mut adapter, clock) = adapter();
    let prepared = adapter
        .prepare(&key_of(child.pid()), Signal::Sigterm, &context())
        .unwrap();
    let status = child.child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "the child exited on its own");
    assert_eq!(
        adapter.observe_exit(prepared.handle),
        ExitObservation::ExitObserved
    );
    assert_eq!(
        adapter.send(prepared.handle, Signal::Sigterm, &receipt(&clock)),
        SignalOutcome::Exited
    );
}

#[test]
fn pid_one_and_own_pid_are_protected() {
    let (mut adapter, _clock) = adapter();
    let mut context = context();
    context.acting_uid = context.acting_uid.max(1);
    let own = i32::try_from(std::process::id()).unwrap();
    for pid in [1, own] {
        let mut key = key_of(own);
        key.pid = Pid::new(i64::from(pid)).unwrap();
        assert!(matches!(
            adapter.prepare(&key, Signal::Sigterm, &context),
            Err(ActionError::Protected(_))
        ));
    }
    assert_eq!(adapter.held_count(), 0);
}

#[test]
fn managed_pids_are_protected() {
    let mut child = Disposable::sleep("30");
    let (mut adapter, _clock) = adapter();
    let mut context = context();
    context.acting_uid = context.acting_uid.max(1);
    context.protected_pids = vec![child.pid()];
    assert!(matches!(
        adapter.prepare(&key_of(child.pid()), Signal::Sigterm, &context),
        Err(ActionError::Protected(_))
    ));
    assert!(child.is_alive());
}

#[test]
fn root_session_is_refused() {
    let mut child = Disposable::sleep("30");
    let (mut adapter, _clock) = adapter();
    let mut context = context();
    context.acting_uid = 0;
    assert_eq!(
        adapter.prepare(&key_of(child.pid()), Signal::Sigterm, &context),
        Err(ActionError::RootSession)
    );
    assert!(child.is_alive());
}

#[test]
fn foreign_namespace_host_or_boot_is_refused() {
    let mut child = Disposable::sleep("30");
    let (mut adapter, _clock) = adapter();
    let mut context = context();
    context.acting_uid = context.acting_uid.max(1);
    let key = key_of(child.pid());
    let mut other_namespace = key.clone();
    other_namespace.pid_namespace = PidNamespace::new("pid:[1]").unwrap();
    let mut other_host = key.clone();
    other_host.host_id = HostId::new("another-host").unwrap();
    let mut other_boot = key.clone();
    other_boot.boot_id = BootId::new("another-boot").unwrap();
    for foreign in [other_namespace, other_host, other_boot] {
        assert_eq!(
            adapter.prepare(&foreign, Signal::Sigterm, &context),
            Err(ActionError::NamespaceMismatch)
        );
    }
    assert!(child.is_alive());
}

/// A target living in a PID namespace of its own (a container) whose PID
/// this namespace numbers: refused, unless `--any-user`, which reaches and
/// signals it as htop does from the host (D-127).
#[test]
fn a_container_process_is_reached_only_with_any_user() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("30");
    child.wait_for_exec();
    let files = ScriptedFiles {
        namespace_override: Some("pid:[4026532999]".to_owned()),
        ..ScriptedFiles::default()
    };
    let (mut adapter, clock) = scripted_adapter(Box::new(LinuxPidfd), files);
    let mut context = context();
    context.acting_uid = context.acting_uid.max(1);
    let key = key_of(child.pid());
    assert_eq!(
        adapter.prepare(&key, Signal::Sigterm, &context),
        Err(ActionError::NamespaceMismatch)
    );
    context.any_user = true;
    let prepared = adapter.prepare(&key, Signal::Sigterm, &context).unwrap();
    assert_eq!(prepared.verified.key, key);
    clock.advance_ms(10);
    assert_eq!(
        adapter.send(prepared.handle, Signal::Sigterm, &receipt(&clock)),
        SignalOutcome::Sent
    );
    let status = child.child.wait().unwrap();
    assert_eq!(status.signal(), Some(Signal::Sigterm.number()));
}

#[test]
fn read_only_and_disabled_sessions_are_refused() {
    let mut child = Disposable::sleep("30");
    let (mut adapter, _clock) = adapter();
    let key = key_of(child.pid());
    let mut read_only = context();
    read_only.read_only = true;
    let mut disabled = context();
    disabled.actions_enabled = false;
    for context in [read_only, disabled] {
        assert!(matches!(
            adapter.prepare(&key, Signal::Sigterm, &context),
            Err(ActionError::Disabled(_))
        ));
    }
    assert!(child.is_alive());
}

#[test]
fn sigkill_requires_allow_force() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("30");
    let (mut adapter, _clock) = adapter();
    let key = key_of(child.pid());
    assert_eq!(
        adapter.prepare(&key, Signal::Sigkill, &context()),
        Err(ActionError::ForceNotAllowed)
    );
    let mut forced = context();
    forced.allow_force = true;
    let prepared = adapter.prepare(&key, Signal::Sigkill, &forced).unwrap();
    adapter.release(prepared.handle);
    assert!(child.is_alive(), "prepare alone never signals");
}

#[test]
fn other_users_process_is_uid_mismatch() {
    let mut child = Disposable::sleep("30");
    let (mut adapter, _clock) = adapter();
    let mut context = context();
    context.acting_uid = euid().wrapping_add(1).max(1);
    assert_eq!(
        adapter.prepare(&key_of(child.pid()), Signal::Sigterm, &context),
        Err(ActionError::UidMismatch)
    );
    assert_eq!(adapter.held_count(), 0);
    assert!(child.is_alive());
}

#[test]
fn held_handles_are_bounded() {
    if running_as_root() {
        return;
    }
    let child = Disposable::sleep("30");
    let (mut adapter, _clock) = adapter();
    let key = key_of(child.pid());
    let handles: Vec<_> = (0..MAX_HELD_HANDLES)
        .map(|_| {
            adapter
                .prepare(&key, Signal::Sigterm, &context())
                .unwrap()
                .handle
        })
        .collect();
    assert!(matches!(
        adapter.prepare(&key, Signal::Sigterm, &context()),
        Err(ActionError::Unavailable(_))
    ));
    adapter.release(handles[0]);
    let again = adapter.prepare(&key, Signal::Sigterm, &context()).unwrap();
    assert!(!handles.contains(&again.handle), "handles are never reused");
    for handle in handles.iter().skip(1).chain([&again.handle]) {
        adapter.release(*handle);
    }
    assert_eq!(adapter.held_count(), 0);
}

#[test]
fn leaked_handles_expire_after_the_hold_limit() {
    if running_as_root() {
        return;
    }
    let child = Disposable::sleep("30");
    let (mut adapter, clock) = adapter();
    let key = key_of(child.pid());
    for _ in 0..MAX_HELD_HANDLES {
        adapter.prepare(&key, Signal::Sigterm, &context()).unwrap();
    }
    clock.advance_ms(dgxtop_adapters::process_action::MAX_HOLD_MS);
    assert!(adapter.prepare(&key, Signal::Sigterm, &context()).is_ok());
    assert_eq!(adapter.held_count(), 1);
}

#[test]
fn double_release_is_safe_and_unknown_handles_fail_closed() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("30");
    let (mut adapter, clock) = adapter();
    let prepared = adapter
        .prepare(&key_of(child.pid()), Signal::Sigterm, &context())
        .unwrap();
    adapter.release(prepared.handle);
    adapter.release(prepared.handle);
    assert_eq!(adapter.held_count(), 0);
    assert!(matches!(
        adapter.send(prepared.handle, Signal::Sigterm, &receipt(&clock)),
        SignalOutcome::Unavailable { .. }
    ));
    assert_eq!(
        adapter.observe_exit(prepared.handle),
        ExitObservation::Unknown
    );
    assert!(child.is_alive(), "released handles never signal");
}

#[test]
fn send_refuses_a_different_signal_or_a_stale_receipt() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("30");
    let (mut adapter, clock) = adapter();
    let mut forced = context();
    forced.allow_force = true;
    let early = receipt(&clock);
    clock.advance_ms(5);
    let prepared = adapter
        .prepare(&key_of(child.pid()), Signal::Sigterm, &forced)
        .unwrap();
    assert!(matches!(
        adapter.send(prepared.handle, Signal::Sigkill, &receipt(&clock)),
        SignalOutcome::NotSent { .. }
    ));
    assert!(matches!(
        adapter.send(prepared.handle, Signal::Sigterm, &early),
        SignalOutcome::NotSent { .. }
    ));
    std::thread::sleep(Duration::from_millis(50));
    assert!(child.is_alive());
    adapter.release(prepared.handle);
}

// ---------------------------------------------------------------- fault injection

#[test]
fn kernels_without_pidfd_are_unsupported() {
    let mut child = Disposable::sleep("30");
    for errno in [Errno::ENOSYS, Errno::EINVAL] {
        let (mut adapter, _clock) =
            scripted_adapter(Box::new(NoPidfdKernel(errno)), ScriptedFiles::default());
        let mut context = context();
        context.acting_uid = context.acting_uid.max(1);
        assert_eq!(
            adapter.prepare(&key_of(child.pid()), Signal::Sigterm, &context),
            Err(ActionError::PidfdUnsupported)
        );
    }
    assert!(child.is_alive());
}

#[test]
fn send_errnos_are_classified_without_pid_fallback() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("30");
    child.wait_for_exec();
    for (errno, expected) in [
        (Errno::EPERM, SignalOutcome::PermissionDenied),
        (Errno::ESRCH, SignalOutcome::Exited),
        (Errno::EBADF, SignalOutcome::Error { errno: libc::EBADF }),
    ] {
        let sends = Arc::new(AtomicU64::new(0));
        let syscalls = ScriptedSend {
            result: Err(errno),
            sends: Arc::clone(&sends),
        };
        let (mut adapter, clock) = scripted_adapter(Box::new(syscalls), ScriptedFiles::default());
        let prepared = adapter
            .prepare(&key_of(child.pid()), Signal::Sigterm, &context())
            .unwrap();
        assert_eq!(
            adapter.send(prepared.handle, Signal::Sigterm, &receipt(&clock)),
            expected
        );
        // Exactly one attempt, no retry and no PID-based fallback.
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        assert!(matches!(
            adapter.send(prepared.handle, Signal::Sigterm, &receipt(&clock)),
            SignalOutcome::NotSent { .. }
        ));
        assert_eq!(sends.load(Ordering::SeqCst), 1);
    }
    assert!(child.is_alive(), "scripted sends deliver nothing");
}

#[test]
fn identity_change_before_send_is_refused_without_a_send() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("30");
    let sends = Arc::new(AtomicU64::new(0));
    let files = ScriptedFiles::default();
    let syscalls = ScriptedSend {
        result: Ok(()),
        sends: Arc::clone(&sends),
    };
    let (mut adapter, clock) = scripted_adapter(Box::new(syscalls), files.clone());
    let key = key_of(child.pid());
    let prepared = adapter.prepare(&key, Signal::Sigterm, &context()).unwrap();
    // Same PID, later start time: what a recycled PID looks like in /proc.
    let reused = format!(
        "{} (sleep) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {} 0 0",
        child.pid(),
        key.start_time_ticks + 7
    );
    assert_eq!(
        parse_stat(&reused).unwrap().start_time_ticks,
        key.start_time_ticks + 7
    );
    *files.stat_override.lock().unwrap() = Some(reused);
    assert_eq!(
        adapter.send(prepared.handle, Signal::Sigterm, &receipt(&clock)),
        SignalOutcome::Changed
    );
    assert_eq!(sends.load(Ordering::SeqCst), 0);
    assert!(child.is_alive());
}

#[test]
fn verified_command_name_stays_raw_for_the_audit_and_the_summary_is_clean() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("30");
    child.wait_for_exec();
    let files = ScriptedFiles::default();
    let (mut adapter, _clock) = scripted_adapter(Box::new(LinuxPidfd), files.clone());
    let key = key_of(child.pid());
    // Same identity, hostile command name (what a renamed thread can show).
    let hostile = format!(
        "{} (evil\u{1b}[2J/x) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {} 0 0",
        child.pid(),
        key.start_time_ticks
    );
    *files.stat_override.lock().unwrap() = Some(hostile);
    let prepared = adapter.prepare(&key, Signal::Sigterm, &context()).unwrap();
    assert_eq!(prepared.verified.basename, "evil\u{1b}[2J/x");
    assert!(!prepared.display_summary.contains('\u{1b}'));
    assert!(prepared.display_summary.contains("evil"));
    adapter.release(prepared.handle);
    assert!(child.is_alive());
}

#[test]
fn proc_read_failure_rejects_and_closes_the_descriptor() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("30");
    let files = ScriptedFiles {
        status_fails: true,
        ..ScriptedFiles::default()
    };
    let (mut adapter, _clock) = scripted_adapter(Box::new(LinuxPidfd), files);
    assert!(matches!(
        adapter.prepare(&key_of(child.pid()), Signal::Sigterm, &context()),
        Err(ActionError::Unavailable(_))
    ));
    assert_eq!(adapter.held_count(), 0);
    assert!(child.is_alive());
}

#[test]
fn pidfd_naming_another_target_is_rejected() {
    if running_as_root() {
        return;
    }
    let mut child = Disposable::sleep("30");
    for (fdinfo, expected) in [
        ("Pid:\t-1\n", ActionError::Exited),
        ("Pid:\t0\n", ActionError::NamespaceMismatch),
        ("Pid:\t999999\n", ActionError::IdentityChanged),
    ] {
        let files = ScriptedFiles {
            fdinfo_override: Some(fdinfo.to_owned()),
            ..ScriptedFiles::default()
        };
        let (mut adapter, _clock) = scripted_adapter(Box::new(LinuxPidfd), files);
        assert_eq!(
            adapter.prepare(&key_of(child.pid()), Signal::Sigterm, &context()),
            Err(expected)
        );
        assert_eq!(adapter.held_count(), 0);
    }
    assert!(child.is_alive());
}
