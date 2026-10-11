//! The process probe (D-76): what the process history follows of the few
//! GPU processes it tracks, read from `/proc/<pid>/{stat,io,cmdline,cwd}`
//! and `/proc/vmstat`. Read-only, like every telemetry adapter; a PID whose
//! start time no longer matches the key is another process and reads as
//! gone.

use std::path::Path;
use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{PidNamespace, ProcessKey};
use dgxtop_core::ports::ProcessProbe;
use dgxtop_core::process_probe::{
    ExitStatus, IoCounters, MAX_PROBE_TEXT_BYTES, MAX_PROBED_PROCESSES, ProbeLiveness,
    ProbeReading, ProbeReport, ProbeTarget,
};
use dgxtop_core::text::sanitize_display;

use super::command::command_text;
use super::host::PID_NAMESPACE_PATH;
use super::mechanics::namespace_link;
use super::parse::{PidStat, parse_pid_stat};
use crate::common::SystemFiles;

const VMSTAT_PATH: &str = "/proc/vmstat";
const OOM_KILL_FIELD: &str = "oom_kill";
const ZOMBIE: char = 'Z';

/// Probes processes through `files` (the host's `/proc` in production).
pub struct ProcfsProcessProbe {
    files: Arc<dyn SystemFiles>,
    clock: Arc<dyn TimeProvider>,
    /// Ours, read on the first probe; a key of another namespace is not
    /// a process this probe can see.
    namespace: Option<Option<PidNamespace>>,
}

impl ProcfsProcessProbe {
    pub fn new(files: Arc<dyn SystemFiles>, clock: Arc<dyn TimeProvider>) -> Self {
        Self {
            files,
            clock,
            namespace: None,
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
        let gone = || ProbeReading {
            key: key.clone(),
            liveness: ProbeLiveness::Gone,
            command: None,
            cwd: None,
            io: None,
        };
        if !visible {
            return gone();
        }
        let Some(stat) = self.stat_of(key) else {
            return gone();
        };
        let pid = key.pid;
        let io = self
            .files
            .read_to_string(&format!("/proc/{pid}/io"))
            .ok()
            .and_then(|text| parse_io(&text));
        let liveness = liveness_of(&stat, io.is_some());
        let wants_text = target.with_text && liveness == ProbeLiveness::Running;
        ProbeReading {
            key: key.clone(),
            liveness,
            command: wants_text.then(|| self.command_of(key)).flatten(),
            cwd: wants_text.then(|| self.cwd_of(key)).flatten(),
            io,
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
        let text = self
            .files
            .read_to_string(&format!("/proc/{}/cmdline", key.pid))
            .ok()?;
        let argv: Vec<String> = text
            .split('\0')
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect();
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
        let readings = targets
            .iter()
            .take(MAX_PROBED_PROCESSES)
            .map(|target| {
                let visible = own.as_ref() == Some(&target.key.pid_namespace);
                self.read(target, visible)
            })
            .collect();
        ProbeReport {
            readings,
            oom_kills: oom_kills(&*self.files),
            read_boot_ns: self.clock.now_boot_ns(),
        }
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

/// A path as shown: cleaned of control characters and bounded.
fn path_text(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let cleaned = sanitize_display(text, MAX_PROBE_TEXT_BYTES);
    (!cleaned.is_empty()).then_some(cleaned)
}

#[cfg(test)]
mod tests;
