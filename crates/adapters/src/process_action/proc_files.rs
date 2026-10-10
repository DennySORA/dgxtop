//! `/proc` facts used to re-verify a process identity while its pidfd is
//! held: start time and comm (`stat`), uids (`status`), PID namespace
//! (`ns/pid`) and the pidfd's target (`/proc/self/fdinfo/<fd>`). Parsing is
//! pure; file access goes through [`ProcessFiles`] so failure paths are
//! testable without touching real processes.

use std::io::{self, Read};
use std::os::fd::RawFd;

/// Upper bound on bytes read from one `/proc` file.
const MAX_PROC_FILE_BYTES: u64 = 64 * 1024;

/// Read-only access to the `/proc` files the verifier needs.
pub trait ProcessFiles: Send {
    fn stat(&self, pid: i32) -> io::Result<String>;
    fn status(&self, pid: i32) -> io::Result<String>;
    /// Target of `/proc/<pid>/ns/pid`, e.g. `pid:[4026531836]`.
    fn pid_namespace(&self, pid: i32) -> io::Result<String>;
    /// Contents of `/proc/self/fdinfo/<fd>`.
    fn descriptor_info(&self, fd: RawFd) -> io::Result<String>;
}

/// The host's `/proc`.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostProcessFiles;

impl ProcessFiles for HostProcessFiles {
    fn stat(&self, pid: i32) -> io::Result<String> {
        read_bounded(&format!("/proc/{pid}/stat"))
    }

    fn status(&self, pid: i32) -> io::Result<String> {
        read_bounded(&format!("/proc/{pid}/status"))
    }

    fn pid_namespace(&self, pid: i32) -> io::Result<String> {
        let target = std::fs::read_link(format!("/proc/{pid}/ns/pid"))?;
        target
            .into_os_string()
            .into_string()
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
    }

    fn descriptor_info(&self, fd: RawFd) -> io::Result<String> {
        read_bounded(&format!("/proc/self/fdinfo/{fd}"))
    }
}

fn read_bounded(path: &str) -> io::Result<String> {
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(MAX_PROC_FILE_BYTES)
        .read_to_string(&mut text)?;
    Ok(text)
}

/// Fields of `/proc/<pid>/stat` the verifier needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatFacts {
    /// Raw command name between the parentheses (untrusted text).
    pub comm: String,
    /// Field 22, clock ticks since boot.
    pub start_time_ticks: u64,
}

/// Parses `stat`. The comm may contain spaces and parentheses, so fields are
/// counted after the LAST `)`; field 22 is the 20th token after it.
pub fn parse_stat(text: &str) -> Option<StatFacts> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close <= open {
        return None;
    }
    let comm = text.get(open.saturating_add(1)..close)?.to_owned();
    let start_time_ticks = text
        .get(close.saturating_add(1)..)?
        .split_ascii_whitespace()
        .nth(19)?
        .parse::<u64>()
        .ok()?;
    Some(StatFacts {
        comm,
        start_time_ticks,
    })
}

/// Real and effective uid from the `Uid:` line of `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessUids {
    pub real: u32,
    pub effective: u32,
}

pub fn parse_uids(status: &str) -> Option<ProcessUids> {
    let line = status.lines().find(|line| line.starts_with("Uid:"))?;
    let mut fields = line.trim_start_matches("Uid:").split_ascii_whitespace();
    Some(ProcessUids {
        real: fields.next()?.parse().ok()?,
        effective: fields.next()?.parse().ok()?,
    })
}

/// What a pidfd's `fdinfo` says about its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidfdTarget {
    /// The process with this PID (in our namespace) is the target.
    Pid(i64),
    /// `Pid: -1`: the target has exited.
    Exited,
    /// `Pid: 0`: the target is not visible in our PID namespace.
    OutsideNamespace,
}

pub fn parse_pidfd_target(info: &str) -> Option<PidfdTarget> {
    let line = info.lines().find(|line| line.starts_with("Pid:"))?;
    let pid = line.trim_start_matches("Pid:").trim().parse::<i64>().ok()?;
    Some(match pid {
        -1 => PidfdTarget::Exited,
        0 => PidfdTarget::OutsideNamespace,
        positive if positive > 0 => PidfdTarget::Pid(positive),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "4242 (my (odd) proc) S 1 4242 4242 0 -1 4194560 100 0 0 0 5 3 0 0 20 0 1 0 90100 1000000 200 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";

    #[test]
    fn stat_parses_after_last_parenthesis() {
        let facts = parse_stat(STAT).unwrap();
        assert_eq!(facts.comm, "my (odd) proc");
        assert_eq!(facts.start_time_ticks, 90_100);
    }

    #[test]
    fn malformed_stat_is_rejected() {
        assert_eq!(parse_stat(""), None);
        assert_eq!(parse_stat("1 (x) S 1 2"), None);
        assert_eq!(parse_stat(") 1 (x"), None);
        let bad_start = STAT.replace("90100", "nan");
        assert_eq!(parse_stat(&bad_start), None);
    }

    #[test]
    fn uids_come_from_the_uid_line() {
        let status = "Name:\tsleep\nUid:\t1000\t1001\t1002\t1003\nGid:\t5\t5\t5\t5\n";
        assert_eq!(
            parse_uids(status),
            Some(ProcessUids {
                real: 1000,
                effective: 1001
            })
        );
        assert_eq!(parse_uids("Name:\tx\n"), None);
        assert_eq!(parse_uids("Uid:\tabc\t1\n"), None);
    }

    #[test]
    fn fdinfo_target_distinguishes_exit_and_namespace() {
        assert_eq!(
            parse_pidfd_target("pos:\t0\nflags:\t02000002\nPid:\t77\nNSpid:\t77\n"),
            Some(PidfdTarget::Pid(77))
        );
        assert_eq!(parse_pidfd_target("Pid:\t-1\n"), Some(PidfdTarget::Exited));
        assert_eq!(
            parse_pidfd_target("Pid:\t0\n"),
            Some(PidfdTarget::OutsideNamespace)
        );
        assert_eq!(parse_pidfd_target("pos:\t0\n"), None);
        assert_eq!(parse_pidfd_target("Pid:\t-7\n"), None);
    }

    #[test]
    fn host_files_read_own_process() {
        let pid = i32::try_from(std::process::id()).unwrap();
        let files = HostProcessFiles;
        let facts = parse_stat(&files.stat(pid).unwrap()).unwrap();
        assert!(facts.start_time_ticks > 0);
        assert!(parse_uids(&files.status(pid).unwrap()).is_some());
        assert!(files.pid_namespace(pid).unwrap().starts_with("pid:["));
        assert!(files.stat(i32::MAX).is_err());
    }
}
