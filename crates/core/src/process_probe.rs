//! What the process history reads of the GPU processes it follows (D-76),
//! beside the process table: the full command line and working folder, the
//! bytes a process read and wrote, and how it ended. Read by a
//! [`crate::ports::ProcessProbe`] for a few processes at a time, never for
//! every process on the host.

use serde::{Deserialize, Serialize};

use crate::identity::ProcessKey;

/// At most this many processes are probed at once.
pub const MAX_PROBED_PROCESSES: usize = 64;

/// A probed command line or working folder keeps at most this many bytes
/// (cut with `…`).
pub const MAX_PROBE_TEXT_BYTES: usize = 4096;

/// One process to probe; `with_text` asks for its command line and
/// working folder too, which the history reads once per process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeTarget {
    pub key: ProcessKey,
    pub with_text: bool,
}

/// How a process the kernel still lists as a zombie ended, from the wait
/// status in its `stat` (only readable for one's own processes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitStatus {
    /// It returned or called `exit` with this code.
    Exited(u8),
    /// A signal ended it (`SIGKILL` is 9, `SIGTERM` 15).
    Signaled(u8),
}

impl ExitStatus {
    /// Decodes a `waitpid` status word. The kernel shows zero to a reader
    /// that may not trace the process, so a zero means a clean exit only
    /// where the reader may.
    pub const fn from_wait_status(status: u32) -> Self {
        let signal = (status & 0x7f) as u8;
        if signal == 0 {
            Self::Exited(((status >> 8) & 0xff) as u8)
        } else {
            Self::Signaled(signal)
        }
    }
}

/// Whether the probed process still runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeLiveness {
    Running,
    /// Ended, not yet reaped by its parent; the status when readable.
    Zombie(Option<ExitStatus>),
    /// No such process (any more), or its PID belongs to a newer process.
    Gone,
}

/// Bytes a process made the storage layer read and write since it started
/// (`/proc/<pid>/io` `read_bytes` / `write_bytes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IoCounters {
    pub read_bytes: u64,
    pub write_bytes: u64,
}

/// What was read of one process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReading {
    pub key: ProcessKey,
    pub liveness: ProbeLiveness,
    /// The command line, cleaned and bounded; `None` unless asked for or
    /// when unreadable.
    pub command: Option<String>,
    /// The working folder, cleaned and bounded; `None` unless asked for or
    /// when unreadable.
    pub cwd: Option<String>,
    /// `None` when the kernel does not let dgxtop read them.
    pub io: Option<IoCounters>,
}

/// One probe of a set of processes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProbeReport {
    pub readings: Vec<ProbeReading>,
    /// The kernel's count of OOM kills since boot (`/proc/vmstat`
    /// `oom_kill`); `None` when the kernel does not report it.
    pub oom_kills: Option<u64>,
    pub read_boot_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_status_decodes_exit_codes_and_signals() {
        assert_eq!(ExitStatus::from_wait_status(0), ExitStatus::Exited(0));
        assert_eq!(ExitStatus::from_wait_status(9), ExitStatus::Signaled(9));
        assert_eq!(
            ExitStatus::from_wait_status(15 | 0x80),
            ExitStatus::Signaled(15),
            "the core-dump bit is not part of the signal"
        );
        assert_eq!(ExitStatus::from_wait_status(1 << 8), ExitStatus::Exited(1));
        assert_eq!(
            ExitStatus::from_wait_status(255 << 8),
            ExitStatus::Exited(255)
        );
    }
}
