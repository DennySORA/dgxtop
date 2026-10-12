//! Launching adapter hosts and controlling the managed child.
//!
//! Production hosts are children of the same trusted binary (no shell, no
//! network): `program args… --host-generation <n>` with the nonce in
//! `DGXTOP_HOST_NONCE`, stdin/stdout as the IPC pipes and stderr as a
//! separate diagnostics pipe. Every other descriptor is close-on-exec. The
//! child gets its own process group so terminal signals reach only the
//! parent, which owns the lifecycle. `--replay` adapters run in-process on a
//! dedicated thread behind real pipes, so they use the exact same framing
//! and state machine — but a thread can only be quarantined, never killed.

use std::fmt;
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::InfraError;
use dgxtop_core::identity::AdapterId;
use dgxtop_core::ports::TelemetryPort;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::host::{
    HOST_GENERATION_ARG, HOST_NONCE_ENV, HostExit, HostOptions, run_adapter_host_with,
};
use crate::ipc::IpcLimits;

/// Builds a fresh adapter for an in-process (replay) host.
pub type AdapterFactory = Arc<dyn Fn() -> Box<dyn TelemetryPort> + Send + Sync>;

/// How a host incarnation is started.
#[derive(Clone)]
pub enum HostLaunch {
    /// Child process of a trusted binary; the supervisor appends
    /// `--host-generation <n>` and sets `DGXTOP_HOST_NONCE`.
    ChildProcess { program: PathBuf, args: Vec<String> },
    /// Dedicated thread (only for `--replay`); cannot be killed on timeout.
    InProcess(AdapterFactory),
}

impl fmt::Debug for HostLaunch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChildProcess { program, args } => formatter
                .debug_struct("ChildProcess")
                .field("program", program)
                .field("args", args)
                .finish(),
            Self::InProcess(_) => formatter.write_str("InProcess(..)"),
        }
    }
}

impl HostLaunch {
    pub const fn is_in_process(&self) -> bool {
        matches!(self, Self::InProcess(_))
    }
}

/// One supervised backend.
#[derive(Debug, Clone)]
pub struct HostSpec {
    pub adapter_id: AdapterId,
    pub launch: HostLaunch,
}

/// Everything a launcher needs for one incarnation.
pub struct LaunchRequest<'a> {
    pub adapter_id: &'a AdapterId,
    pub launch: &'a HostLaunch,
    pub generation: u32,
    pub nonce: &'a str,
    pub clock: &'a Arc<dyn TimeProvider>,
    pub limits: IpcLimits,
}

/// Parent-side ends of a started host. Every descriptor is non-blocking.
pub struct LaunchedHost {
    pub control: Box<dyn ChildControl>,
    /// Parent writes requests here (child stdin).
    pub requests: OwnedFd,
    /// Parent reads frames here (child stdout).
    pub replies: OwnedFd,
    /// Parent drains diagnostics here (child stderr).
    pub diagnostics: Option<OwnedFd>,
}

/// Starts host incarnations (injectable for fault tests).
pub trait HostLauncher: Send + Sync {
    fn launch(&self, request: &LaunchRequest<'_>) -> Result<LaunchedHost, InfraError>;
}

/// Result of asking a child to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalDelivery {
    Sent,
    AlreadyExited,
    /// The host is a thread; it cannot be signalled.
    Unsupported,
}

/// How a host incarnation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitSummary {
    Exited(i32),
    Signaled(i32),
    /// In-process host thread result.
    Thread(String),
    Unknown,
}

impl fmt::Display for ExitSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exited(code) => write!(formatter, "exit code {code}"),
            Self::Signaled(signal) => write!(formatter, "signal {signal}"),
            Self::Thread(outcome) => write!(formatter, "thread {outcome}"),
            Self::Unknown => formatter.write_str("unknown exit"),
        }
    }
}

/// Control over exactly one managed host. Signals only ever target that
/// host, and only while it is unreaped (so its PID cannot be reused).
pub trait ChildControl: Send {
    fn pid(&self) -> Option<i32>;
    /// SIGTERM.
    fn terminate(&mut self) -> Result<SignalDelivery, InfraError>;
    /// SIGKILL.
    fn kill(&mut self) -> Result<SignalDelivery, InfraError>;
    /// Non-blocking reap (`waitpid(WNOHANG)` for processes).
    fn try_reap(&mut self) -> Result<Option<ExitSummary>, InfraError>;
}

/// Launches child processes and in-process replay hosts.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandardLauncher;

impl HostLauncher for StandardLauncher {
    fn launch(&self, request: &LaunchRequest<'_>) -> Result<LaunchedHost, InfraError> {
        match request.launch {
            HostLaunch::ChildProcess { program, args } => spawn_child(program, args, request),
            HostLaunch::InProcess(factory) => spawn_in_process(factory, request),
        }
    }
}

fn spawn_child(
    program: &PathBuf,
    args: &[String],
    request: &LaunchRequest<'_>,
) -> Result<LaunchedHost, InfraError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .arg(HOST_GENERATION_ARG)
        .arg(request.generation.to_string())
        .env(HOST_NONCE_ENV, request.nonce)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| InfraError::from_io("supervisor.spawn", &error))?;
    match child_pipes(&mut child) {
        Ok((requests, replies, diagnostics)) => Ok(LaunchedHost {
            control: Box::new(ProcessChild {
                child,
                reaped: false,
            }),
            requests,
            replies,
            diagnostics: Some(diagnostics),
        }),
        Err(error) => {
            // The child exists but is unusable: it is ours, so stop it here.
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

fn child_pipes(child: &mut Child) -> Result<(OwnedFd, OwnedFd, OwnedFd), InfraError> {
    let missing = || InfraError::Unavailable("child pipe missing".into());
    let requests = OwnedFd::from(child.stdin.take().ok_or_else(missing)?);
    let replies = OwnedFd::from(child.stdout.take().ok_or_else(missing)?);
    let diagnostics = OwnedFd::from(child.stderr.take().ok_or_else(missing)?);
    for fd in [&requests, &replies, &diagnostics] {
        set_nonblocking(fd)?;
    }
    Ok((requests, replies, diagnostics))
}

/// Sets `O_NONBLOCK` on the parent's end only (the child's end is a
/// different open file description).
pub(crate) fn set_nonblocking(fd: &OwnedFd) -> Result<(), InfraError> {
    let bits = fcntl(fd.as_fd(), FcntlArg::F_GETFL)
        .map_err(|errno| InfraError::Unavailable(format!("F_GETFL failed: {errno}")))?;
    let flags = OFlag::from_bits_retain(bits) | OFlag::O_NONBLOCK;
    fcntl(fd.as_fd(), FcntlArg::F_SETFL(flags))
        .map(|_| ())
        .map_err(|errno| InfraError::Unavailable(format!("F_SETFL failed: {errno}")))
}

/// Close-on-exec pipe so no other child inherits it.
pub(crate) fn private_pipe() -> Result<(OwnedFd, OwnedFd), InfraError> {
    crate::pipe::pipe2(OFlag::O_CLOEXEC)
        .map_err(|errno| InfraError::Unavailable(format!("pipe2 failed: {errno}")))
}

struct ProcessChild {
    child: Child,
    reaped: bool,
}

impl ProcessChild {
    fn raw_pid(&self) -> Result<Pid, InfraError> {
        i32::try_from(self.child.id())
            .map(Pid::from_raw)
            .map_err(|_| InfraError::Unavailable("child pid exceeds i32".into()))
    }
}

impl ChildControl for ProcessChild {
    fn pid(&self) -> Option<i32> {
        i32::try_from(self.child.id()).ok()
    }

    fn terminate(&mut self) -> Result<SignalDelivery, InfraError> {
        if self.reaped {
            return Ok(SignalDelivery::AlreadyExited);
        }
        // Unreaped children keep their PID reserved, so this cannot hit an
        // unrelated process.
        match kill(self.raw_pid()?, Signal::SIGTERM) {
            Ok(()) => Ok(SignalDelivery::Sent),
            Err(nix::errno::Errno::ESRCH) => Ok(SignalDelivery::AlreadyExited),
            Err(errno) => Err(InfraError::Unavailable(format!("SIGTERM failed: {errno}"))),
        }
    }

    fn kill(&mut self) -> Result<SignalDelivery, InfraError> {
        if self.reaped {
            return Ok(SignalDelivery::AlreadyExited);
        }
        self.child
            .kill()
            .map(|()| SignalDelivery::Sent)
            .map_err(|error| InfraError::from_io("supervisor.sigkill", &error))
    }

    fn try_reap(&mut self) -> Result<Option<ExitSummary>, InfraError> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.reaped = true;
                Ok(Some(summarize(status)))
            }
            Ok(None) => Ok(None),
            Err(error) => Err(InfraError::from_io("supervisor.waitpid", &error)),
        }
    }
}

fn summarize(status: ExitStatus) -> ExitSummary {
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitSummary::Exited(code),
        (None, Some(signal)) => ExitSummary::Signaled(signal),
        (None, None) => ExitSummary::Unknown,
    }
}

fn spawn_in_process(
    factory: &AdapterFactory,
    request: &LaunchRequest<'_>,
) -> Result<LaunchedHost, InfraError> {
    let (request_reader, request_writer) = private_pipe()?;
    let (reply_reader, reply_writer) = private_pipe()?;
    let (diagnostics_reader, diagnostics_writer) = private_pipe()?;
    for fd in [&request_writer, &reply_reader, &diagnostics_reader] {
        set_nonblocking(fd)?;
    }
    let factory = Arc::clone(factory);
    let options = HostOptions {
        generation: request.generation,
        nonce: request.nonce.to_owned(),
        limits: request.limits,
    };
    let clock = Arc::clone(request.clock);
    let thread = std::thread::Builder::new()
        .name(format!("host-{}", request.adapter_id))
        .spawn(move || {
            let adapter = factory();
            let mut diagnostics = File::from(diagnostics_writer);
            run_adapter_host_with(
                adapter,
                options,
                clock,
                File::from(request_reader),
                File::from(reply_writer),
                &mut diagnostics,
            )
        })
        .map_err(|error| InfraError::from_io("supervisor.spawn_thread", &error))?;
    Ok(LaunchedHost {
        control: Box::new(ThreadControl {
            thread: Some(thread),
            exit: None,
        }),
        requests: request_writer,
        replies: reply_reader,
        diagnostics: Some(diagnostics_reader),
    })
}

struct ThreadControl {
    thread: Option<JoinHandle<HostExit>>,
    exit: Option<ExitSummary>,
}

impl ThreadControl {
    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl ChildControl for ThreadControl {
    fn pid(&self) -> Option<i32> {
        None
    }

    fn terminate(&mut self) -> Result<SignalDelivery, InfraError> {
        Ok(if self.finished() {
            SignalDelivery::AlreadyExited
        } else {
            SignalDelivery::Unsupported
        })
    }

    fn kill(&mut self) -> Result<SignalDelivery, InfraError> {
        self.terminate()
    }

    fn try_reap(&mut self) -> Result<Option<ExitSummary>, InfraError> {
        if let Some(exit) = &self.exit {
            return Ok(Some(exit.clone()));
        }
        let Some(thread) = self.thread.take_if(|thread| thread.is_finished()) else {
            return Ok(None);
        };
        let exit = match thread.join() {
            Ok(outcome) => ExitSummary::Thread(format!("{outcome:?}")),
            Err(_) => ExitSummary::Thread("panicked".into()),
        };
        self.exit = Some(exit.clone());
        Ok(Some(exit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_program_is_a_launch_error() {
        let launch = HostLaunch::ChildProcess {
            program: PathBuf::from("/nonexistent/dgxtop-host"),
            args: Vec::new(),
        };
        let adapter = dgxtop_core::testing::adapter_id("ghost");
        let clock: Arc<dyn TimeProvider> = dgxtop_core::testing::FakeClock::new(0);
        let request = LaunchRequest {
            adapter_id: &adapter,
            launch: &launch,
            generation: 1,
            nonce: "n",
            clock: &clock,
            limits: IpcLimits::default(),
        };
        assert!(StandardLauncher.launch(&request).is_err());
        assert!(format!("{launch:?}").contains("ChildProcess"));
    }
}
