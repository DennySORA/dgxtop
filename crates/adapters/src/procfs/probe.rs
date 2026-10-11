//! The process probe (D-76, D-77): what the process history follows of the
//! few GPU processes it tracks, read from `/proc/<pid>/{stat,io,status}`,
//! `/proc/<pid>/fd` every [`OPEN_FILES_EVERY_NS`],
//! once per process `/proc/<pid>/{cmdline,cwd,exe,cgroup}` and the parent's
//! `comm`, and `/proc/vmstat`. It never scans `/proc`: the process table
//! sums the descendants from the scan it makes anyway (D-79). Read-only,
//! like every telemetry adapter; a PID whose start time no longer matches
//! the key is another process and reads as gone.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use dgxtop_core::clock::{NANOS_PER_SECOND, TimeProvider};
use dgxtop_core::identity::{PidNamespace, ProcessKey};
use dgxtop_core::ports::ProcessProbe;
use dgxtop_core::process_history::RunDetails;
use dgxtop_core::process_probe::{
    ExitStatus, IoCounters, MAX_PROBE_TEXT_BYTES, MAX_PROBED_PROCESSES, ProbeLiveness,
    ProbeReading, ProbeReport, ProbeTarget,
};
use dgxtop_core::text::sanitize_display;

use super::command::{command_text, read_argv};
use super::host::PID_NAMESPACE_PATH;
use super::mechanics::namespace_link;
use super::parse::{PidStat, parse_pid_stat};
use crate::common::SystemFiles;

const VMSTAT_PATH: &str = "/proc/vmstat";
const OOM_KILL_FIELD: &str = "oom_kill";
const ZOMBIE: char = 'Z';
const KIB: u64 = 1024;
/// The unified (v2) hierarchy's line of `/proc/<pid>/cgroup`.
const UNIFIED_CGROUP: &str = "0::";
/// How long an open-file count stands before `fd` is listed again.
pub(crate) const OPEN_FILES_EVERY_NS: u64 = 10 * NANOS_PER_SECOND;

/// Probes processes through `files` (the host's `/proc` in production).
pub struct ProcfsProcessProbe {
    files: Arc<dyn SystemFiles>,
    clock: Arc<dyn TimeProvider>,
    /// Ours, read on the first probe; a key of another namespace is not
    /// a process this probe can see.
    namespace: Option<Option<PidNamespace>>,
    open_files: OpenFileCounts,
}

impl ProcfsProcessProbe {
    pub fn new(files: Arc<dyn SystemFiles>, clock: Arc<dyn TimeProvider>) -> Self {
        Self {
            files,
            clock,
            namespace: None,
            open_files: OpenFileCounts::default(),
        }
    }

    fn own_namespace(&mut self) -> Option<&PidNamespace> {
        let files = &*self.files;
        self.namespace
            .get_or_insert_with(|| {
                namespace_link(files, PID_NAMESPACE_PATH)
                    .and_then(|namespace| PidNamespace::new(namespace).ok())
            })
            .as_ref()
    }

    fn read(&self, target: &ProbeTarget, visible: bool) -> ProbeReading {
        let key = &target.key;
        if !visible {
            return ProbeReading::gone(key.clone());
        }
        let Some(stat) = self.stat_of(key) else {
            return ProbeReading::gone(key.clone());
        };
        let pid = key.pid;
        let io = self
            .files
            .read_to_string(&format!("/proc/{pid}/io"))
            .ok()
            .and_then(|text| parse_io(&text));
        let liveness = liveness_of(&stat, io.is_some());
        let mut reading = ProbeReading {
            liveness,
            io,
            ..ProbeReading::gone(key.clone())
        };
        if liveness != ProbeLiveness::Running {
            return reading;
        }
        if target.with_text {
            reading.command = self.command_of(key);
            reading.cwd = self.cwd_of(key);
            reading.details = Some(self.details_of(key, &stat));
        }
        reading.threads = stat.num_threads;
        reading.swap_bytes = self
            .files
            .read_to_string(&format!("/proc/{pid}/status"))
            .ok()
            .and_then(|text| field_value(&text, "VmSwap:"))
            .map(|kib| kib.saturating_mul(KIB));
        reading
    }

    /// Its parent, program, control group and niceness.
    fn details_of(&self, key: &ProcessKey, stat: &PidStat) -> RunDetails {
        let parent_pid = stat
            .ppid
            .and_then(|ppid| i32::try_from(ppid).ok())
            .filter(|ppid| *ppid > 0);
        let parent_name = parent_pid
            .and_then(|ppid| {
                self.files
                    .read_to_string(&format!("/proc/{ppid}/comm"))
                    .ok()
            })
            .map(|comm| sanitize_display(comm.trim_end(), MAX_PROBE_TEXT_BYTES))
            .unwrap_or_default();
        let pid = key.pid;
        RunDetails {
            parent_pid,
            parent_name,
            executable: self
                .files
                .read_link(&format!("/proc/{pid}/exe"))
                .ok()
                .and_then(|path| path_text(&path))
                .unwrap_or_default(),
            cgroup: self
                .files
                .read_to_string(&format!("/proc/{pid}/cgroup"))
                .ok()
                .and_then(|text| cgroup_path(&text))
                .unwrap_or_default(),
            nice: stat.nice.and_then(|nice| i8::try_from(nice).ok()),
        }
    }

    /// The `stat` of the process `key` names; `None` when the PID is free
    /// or another process's.
    fn stat_of(&self, key: &ProcessKey) -> Option<PidStat> {
        let text = self
            .files
            .read_to_string(&format!("/proc/{}/stat", key.pid))
            .ok()?;
        parse_pid_stat(&text).filter(|stat| {
            stat.pid == i64::from(key.pid.get()) && stat.start_time_ticks == key.start_time_ticks
        })
    }

    fn command_of(&self, key: &ProcessKey) -> Option<String> {
        let argv = read_argv(self.files.as_ref(), key.pid)?;
        let command = command_text(&argv, MAX_PROBE_TEXT_BYTES);
        (!command.is_empty()).then_some(command)
    }

    fn cwd_of(&self, key: &ProcessKey) -> Option<String> {
        let path = self
            .files
            .read_link(&format!("/proc/{}/cwd", key.pid))
            .ok()?;
        path_text(&path)
    }
}

impl ProcessProbe for ProcfsProcessProbe {
    fn probe(&mut self, targets: &[ProbeTarget]) -> ProbeReport {
        let own = self.own_namespace().cloned();
        let targets = &targets[..targets.len().min(MAX_PROBED_PROCESSES)];
        let visible = |target: &ProbeTarget| own.as_ref() == Some(&target.key.pid_namespace);
        let mut readings: Vec<ProbeReading> = targets
            .iter()
            .map(|target| self.read(target, visible(target)))
            .collect();
        let now_ns = self.clock.now_boot_ns();
        self.open_files.keep(&readings);
        for reading in readings
            .iter_mut()
            .filter(|reading| reading.liveness == ProbeLiveness::Running)
        {
            reading.open_files = self.open_files.count(&*self.files, &reading.key, now_ns);
        }
        ProbeReport {
            readings,
            oom_kills: oom_kills(&*self.files),
            read_boot_ns: self.clock.now_boot_ns(),
        }
    }
}

/// The open-file counts of the probed processes: listing `fd` costs one
/// entry per descriptor and a training job may hold thousands, so a count
/// stands for [`OPEN_FILES_EVERY_NS`] before the folder is listed again.
#[derive(Debug, Default)]
struct OpenFileCounts {
    /// The count (`None`: unreadable, another user's) and when it was read.
    by_process: HashMap<ProcessKey, (Option<u64>, u64)>,
}

impl OpenFileCounts {
    fn count(&mut self, files: &dyn SystemFiles, key: &ProcessKey, now_ns: u64) -> Option<u64> {
        if let Some((count, at)) = self.by_process.get(key)
            && now_ns.saturating_sub(*at) < OPEN_FILES_EVERY_NS
        {
            return *count;
        }
        let count = files
            .read_dir(&format!("/proc/{}/fd", key.pid))
            .ok()
            .map(|entries| u64::try_from(entries.len()).unwrap_or(u64::MAX));
        self.by_process.insert(key.clone(), (count, now_ns));
        count
    }

    /// Forgets the processes no longer probed or no longer running.
    fn keep(&mut self, readings: &[ProbeReading]) {
        self.by_process.retain(|key, _| {
            readings
                .iter()
                .any(|reading| reading.key == *key && reading.liveness == ProbeLiveness::Running)
        });
    }
}

/// A zombie's wait status counts only where the kernel let dgxtop read the
/// process's `io` (the same permission), since it shows zero otherwise.
fn liveness_of(stat: &PidStat, permitted: bool) -> ProbeLiveness {
    if stat.state != Some(ZOMBIE) {
        return ProbeLiveness::Running;
    }
    let status = stat
        .exit_code
        .filter(|_| permitted)
        .map(ExitStatus::from_wait_status);
    ProbeLiveness::Zombie(status)
}

/// `read_bytes` and `write_bytes` of `/proc/<pid>/io`; `None` unless both
/// are there.
fn parse_io(text: &str) -> Option<IoCounters> {
    Some(IoCounters {
        read_bytes: field_value(text, "read_bytes:")?,
        write_bytes: field_value(text, "write_bytes:")?,
    })
}

fn oom_kills(files: &dyn SystemFiles) -> Option<u64> {
    let text = files.read_to_string(VMSTAT_PATH).ok()?;
    field_value(&text, OOM_KILL_FIELD)
}

/// The number after `name` on the line that starts with it as a word.
fn field_value(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == name).then(|| words.next()?.parse().ok())?
    })
}

/// The path of the unified hierarchy's line, else of the first line that
/// names one (`/user.slice/user-1000.slice/session-3.scope`).
fn cgroup_path(text: &str) -> Option<String> {
    let path_of = |line: &str| line.splitn(3, ':').nth(2).map(str::to_owned);
    let path = text
        .lines()
        .find(|line| line.starts_with(UNIFIED_CGROUP))
        .and_then(path_of)
        .or_else(|| text.lines().find_map(path_of))?;
    let cleaned = sanitize_display(path.trim(), MAX_PROBE_TEXT_BYTES);
    (!cleaned.is_empty()).then_some(cleaned)
}

/// A path as shown: cleaned of control characters and bounded.
fn path_text(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let cleaned = sanitize_display(text, MAX_PROBE_TEXT_BYTES);
    (!cleaned.is_empty()).then_some(cleaned)
}

#[cfg(test)]
mod tests;
