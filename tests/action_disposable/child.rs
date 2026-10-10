//! Disposable children spawned by the suite itself, plus the procfs facts the
//! suite reads independently of the product to build expected identities.
//!
//! Safety rule of the suite: the product may only ever signal a
//! [`DisposableChild`]. Every child is killed and reaped when its guard drops,
//! even when a test fails half way.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Lifetime of a child nobody signals: long enough for any test, short
/// enough that a child orphaned by a killed test run still goes away.
const CHILD_LIFETIME_SECONDS: &str = "60";
const POLL: Duration = Duration::from_millis(5);
/// Bit of SIGTERM in the `SigIgn:` mask of `/proc/<pid>/status`.
const SIGTERM_MASK: u64 = 1 << (libc::SIGTERM - 1);

/// A child process owned by this test; killed and reaped on drop.
pub struct DisposableChild {
    child: Child,
}

impl DisposableChild {
    /// `sleep 60`: dies on the first SIGTERM.
    pub fn sleeping() -> Self {
        let child = Command::new("sleep")
            .arg(CHILD_LIFETIME_SECONDS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the disposable `sleep` child");
        Self { child }
    }

    /// `sleep 60` with SIGTERM ignored (the disposition survives `exec`), so
    /// a delivered SIGTERM leaves the process running.
    pub fn ignoring_sigterm() -> Self {
        let child = Command::new("sh")
            .args([
                "-c",
                &format!("trap '' TERM; exec sleep {CHILD_LIFETIME_SECONDS}"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the SIGTERM-ignoring child");
        let disposable = Self { child };
        // A SIGTERM sent before the trap is installed would kill the shell,
        // so the child only counts as ready once `sleep` runs with TERM ignored.
        let pid = disposable.pid();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !(command_name(pid).as_deref() == Some("sleep") && ignores_sigterm(pid)) {
            assert!(
                Instant::now() < deadline,
                "child {pid} never reached `sleep` with SIGTERM ignored"
            );
            thread::sleep(POLL);
        }
        disposable
    }

    pub fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).expect("child pid fits in i32")
    }

    /// True while the child has not exited (it is not reaped by this call
    /// unless it already exited).
    pub fn is_running(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("query the disposable child")
            .is_none()
    }

    /// Waits (bounded) for the child to exit and reaps it.
    pub fn wait_for_exit(&mut self, within: Duration) -> ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().expect("query the disposable child") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "child {} still running after {within:?}",
                self.pid()
            );
            thread::sleep(POLL);
        }
    }

    /// Ends the child from the test itself (never through the product) so
    /// its process is gone before the product acts on it.
    pub fn end_from_the_test(&mut self) -> ExitStatus {
        self.child.kill().expect("kill our own child");
        let status = self.child.wait().expect("reap our own child");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        status
    }
}

impl Drop for DisposableChild {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Identity facts of a live process, parsed here independently of the
/// product's own procfs parsers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFacts {
    pub pid: i32,
    /// Field 22 of `/proc/<pid>/stat` (clock ticks since boot).
    pub start_time_ticks: u64,
    /// First field of the `Uid:` line of `/proc/<pid>/status`.
    pub real_uid: u32,
}

impl ProcessFacts {
    pub fn read(pid: i32) -> io::Result<Self> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let start_time_ticks = start_time_ticks(&stat).ok_or_else(|| invalid("stat"))?;
        let real_uid = real_uid(&status).ok_or_else(|| invalid("status"))?;
        Ok(Self {
            pid,
            start_time_ticks,
            real_uid,
        })
    }
}

/// `pid:[N]` of a process this user may inspect.
pub fn pid_namespace(pid: i32) -> String {
    let link = std::fs::read_link(format!("/proc/{pid}/ns/pid")).expect("read pid namespace");
    link.to_str().expect("namespace link is UTF-8").to_owned()
}

/// A long-running user-space process of another user (for example a root
/// daemon) that is visible in this PID namespace; only ever used as a target
/// that must be refused.
pub fn foreign_user_process(own_uid: u32) -> Option<ProcessFacts> {
    let mut pids: Vec<i32> = std::fs::read_dir("/proc")
        .expect("list /proc")
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| *pid > 1)
        .collect();
    pids.sort_unstable();
    pids.into_iter().find_map(|pid| {
        let facts = ProcessFacts::read(pid).ok()?;
        // Kernel threads have an empty command line; skip them.
        let has_command_line = std::fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|command_line| !command_line.is_empty());
        (facts.real_uid != own_uid && has_command_line).then_some(facts)
    })
}

fn start_time_ticks(stat: &str) -> Option<u64> {
    // The command name may contain spaces and parentheses: count fields after
    // the last ')'; field 3 (state) is the first one there, field 22 the 20th.
    let after_name = stat.get(stat.rfind(')')?.checked_add(1)?..)?;
    after_name.split_ascii_whitespace().nth(19)?.parse().ok()
}

fn real_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_ascii_whitespace()
        .next()?
        .parse()
        .ok()
}

fn command_name(pid: i32) -> Option<String> {
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(name.trim_end().to_owned())
}

fn ignores_sigterm(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|status| {
            let mask = status
                .lines()
                .find_map(|line| line.strip_prefix("SigIgn:"))?;
            u64::from_str_radix(mask.trim(), 16).ok()
        })
        .is_some_and(|mask| mask & SIGTERM_MASK != 0)
}

fn invalid(file: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("unparseable {file}"))
}
