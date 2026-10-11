//! `process` read group (paged, not catalogued): scans `/proc/[0-9]*` in PID
//! order, at most [`MAX_SCANNED_PROCESSES`] processes, and emits ONE record
//! per ProcessKey (D-79): `process.cpu.ticks` (utime + stime from one
//! read: a lossless decode of two same-unit counters) carrying the rest of
//! the `stat` as [`ProcessStat`] tags: RSS (pages × page size, bytes), the
//! owner of `/proc/<pid>` (read once per process), parent, state, threads
//! and the reaped children's ticks.
//!
//! Convention for the process collector: the process's display name
//! ([`command`](super::command), D-65) and its command line (D-73) travel
//! in `provenance.channel` and `/proc/<pid>` in `provenance.device_path`.
//! A process that vanishes mid-scan is skipped, not reported as an error.

use std::collections::{HashMap, HashSet};
use std::io;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{EntityId, Pid, PidNamespace, ProcessKey, Scope};
use dgxtop_core::metric::{ids, native};
use dgxtop_core::naming::{self, entity, groups};
use dgxtop_core::observation::{Observation, ReadPlan, SourceDescriptor};
use dgxtop_core::process_stat::{self, ProcessStat};
use dgxtop_core::quality::{RawKind, ReadStatus};
use dgxtop_core::text::sanitize_command_name;

use super::catalog::{CatalogConfig, KeyContext, SourceSpec, descriptor_for};
use super::command::{Named, command_text, name_process, needs_command_line};
use super::locator::DRIVER;
use super::mechanics::{ReadSnapshot, invariant, io_failure, joined_error_code};
use super::parse::{Field, PidStat, parse_pid_stat};
use super::records::{
    DerivationFailure, FIELD_ABSENT, PARSE_ERROR, ReadWindow, u64_outcome_record,
};
use crate::common::SystemFiles;

/// Upper bound of processes scanned per cycle.
pub(crate) const MAX_SCANNED_PROCESSES: usize = 4096;
/// The deadline is checked every this many processes.
const DEADLINE_CHECK_INTERVAL: usize = 64;
/// `ESRCH`: the process exited between open and read.
const ESRCH: i32 = 3;

/// Everything one process scan needs.
pub(crate) struct ProcessScan<'a> {
    pub plan: &'a ReadPlan,
    pub files: &'a dyn SystemFiles,
    pub clock: &'a dyn TimeProvider,
    pub config: &'a CatalogConfig,
    pub keys: &'a KeyContext,
    pub pid_namespace: &'a PidNamespace,
    pub page_size: Option<u64>,
}

enum ProcessOutcome {
    Seen(Box<SeenProcess>),
    Vanished,
    Denied,
    Unreadable,
    Unparsable,
}

/// Counts of processes that could not be reported.
#[derive(Default)]
struct ScanTally {
    denied: usize,
    unreadable: usize,
    unparsable: usize,
}

/// Display names of the scanned processes (D-65). A process's command line
/// is read when it is first seen with a `comm` that says too little, and
/// again only when its `comm` changes (a program setting its title); only
/// the processes of the last scan are kept.
#[derive(Debug, Default)]
pub(crate) struct ProcessNames {
    by_process: HashMap<(i32, u64), (String, Named, String)>,
}

impl ProcessNames {
    /// The display name and the command line of each of `processes`, in
    /// order; one whose command line names no program takes its parent's
    /// name.
    fn resolve(
        &mut self,
        files: &dyn SystemFiles,
        processes: &[SeenProcess],
    ) -> Vec<(String, String)> {
        let named: Vec<(Named, String)> = processes
            .iter()
            .map(|process| self.named(files, process))
            .collect();
        let kept: HashSet<(i32, u64)> = processes.iter().map(SeenProcess::cache_key).collect();
        self.by_process.retain(|key, _| kept.contains(key));
        let by_pid: HashMap<i64, &Named> = processes
            .iter()
            .zip(&named)
            .map(|(process, (named, _))| (process.stat.pid, named))
            .collect();
        processes
            .iter()
            .zip(&named)
            .map(|(process, (named, command))| {
                let name = match named {
                    Named::Name(name) => name.clone(),
                    Named::Inherit { fallback } => process
                        .stat
                        .ppid
                        .and_then(|ppid| by_pid.get(&ppid))
                        .and_then(|parent| match parent {
                            Named::Name(name) => Some(name.clone()),
                            Named::Inherit { .. } => None,
                        })
                        .unwrap_or_else(|| fallback.clone()),
                };
                (name, command.clone())
            })
            .collect()
    }

    fn named(&mut self, files: &dyn SystemFiles, process: &SeenProcess) -> (Named, String) {
        let key = process.cache_key();
        if let Some((comm, named, command)) = self.by_process.get(&key)
            && *comm == process.stat.comm
        {
            return (named.clone(), command.clone());
        }
        let (named, command) = derive_name(files, process.pid, &process.stat.comm);
        self.by_process.insert(
            key,
            (process.stat.comm.clone(), named.clone(), command.clone()),
        );
        (named, command)
    }
}

/// The name of process `pid` (its `comm`, or what its command line and
/// working folder say when `comm` says too little) and its command line as
/// shown (D-73), read once per process.
fn derive_name(files: &dyn SystemFiles, pid: Pid, comm: &str) -> (Named, String) {
    let argv: Vec<String> = files
        .read_to_string(&format!("/proc/{pid}/cmdline"))
        .map(|text| {
            text.split('\0')
                .filter(|word| !word.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let command = command_text(&argv, MAX_COMMAND_BYTES);
    if !needs_command_line(comm) {
        return (Named::Name(sanitize_command_name(comm)), command);
    }
    let named = name_process(comm, &argv, || {
        files
            .read_link(&format!("/proc/{pid}/cwd"))
            .ok()
            .and_then(|path| path.to_str().map(str::to_owned))
    });
    (named, command)
}

/// Room for a command line in a channel beside a name of at most
/// `MAX_NAME_CHARS` characters (4 bytes each at most) and the separator.
const MAX_COMMAND_BYTES: usize = naming::MAX_CHANNEL_BYTES
    - 4 * super::command::MAX_NAME_CHARS
    - naming::PROCESS_COMMAND_SEPARATOR.len_utf8();

/// Owners of the scanned processes. The owner of `/proc/<pid>` is read
/// when a process is first seen, not every cycle: one instance of a PID
/// keeps its owner. Only the processes of the last scan are kept.
#[derive(Debug, Default)]
pub(crate) struct ProcessOwners {
    by_process: HashMap<(i32, u64), u32>,
}

impl ProcessOwners {
    fn get(&self, key: (i32, u64)) -> Option<u32> {
        self.by_process.get(&key).copied()
    }

    /// Keeps the owners of `processes`, those read now included.
    fn keep(&mut self, processes: &[SeenProcess]) {
        self.by_process = processes
            .iter()
            .filter_map(|process| Some((process.cache_key(), *process.uid.as_ref().ok()?)))
            .collect();
    }
}

/// What the process scan remembers between cycles: names and owners.
#[derive(Debug, Default)]
pub(crate) struct ProcessMemo {
    names: ProcessNames,
    owners: ProcessOwners,
}

/// Scans processes in PID order; problems are summarized in the final
/// page's error code.
pub(crate) fn scan(scan: &ProcessScan<'_>, memo: &mut ProcessMemo) -> ReadSnapshot {
    let started_boot_ns = scan.clock.now_boot_ns();
    let pids = match list_pids(scan.files) {
        Ok(pids) => pids,
        Err(error) => {
            return ReadSnapshot {
                records: Vec::new(),
                started_boot_ns,
                finished_boot_ns: scan.clock.now_boot_ns(),
                final_error: Some(format!("PROC_UNREADABLE:{}", io_failure(&error).1)),
            };
        }
    };
    let mut codes = Vec::new();
    if pids.len() > MAX_SCANNED_PROCESSES {
        codes.push("PROCESS_SCAN_TRUNCATED".to_owned());
    }
    let mut seen = Vec::new();
    let mut tally = ScanTally::default();
    for (index, pid) in pids.iter().take(MAX_SCANNED_PROCESSES).enumerate() {
        if index % DEADLINE_CHECK_INTERVAL == 0
            && scan.clock.now_boot_ns() > scan.plan.deadline_boot_ns
        {
            codes.push("PROCESS_SCAN_DEADLINE".to_owned());
            break;
        }
        match read_process(scan, &memo.owners, *pid) {
            ProcessOutcome::Seen(process) => seen.push(*process),
            ProcessOutcome::Vanished => {}
            ProcessOutcome::Denied => tally.denied += 1,
            ProcessOutcome::Unreadable => tally.unreadable += 1,
            ProcessOutcome::Unparsable => tally.unparsable += 1,
        }
    }
    codes.extend(tally_codes(&tally));
    memo.owners.keep(&seen);
    let display_names = memo.names.resolve(scan.files, &seen);
    let records = seen
        .iter()
        .zip(&display_names)
        .filter_map(|(process, (name, command))| process.record(scan, name, command))
        .collect();
    ReadSnapshot {
        records,
        started_boot_ns,
        finished_boot_ns: scan.clock.now_boot_ns(),
        final_error: joined_error_code(&codes),
    }
}

fn tally_codes(tally: &ScanTally) -> Vec<String> {
    let mut codes = Vec::new();
    if tally.denied > 0 {
        codes.push("PROCESS_STAT_PERMISSION_DENIED".to_owned());
    }
    if tally.unreadable > 0 {
        codes.push("PROCESS_STAT_UNREADABLE".to_owned());
    }
    if tally.unparsable > 0 {
        codes.push("PROCESS_STAT_UNPARSABLE".to_owned());
    }
    codes
}

/// Numeric `/proc` entries as valid PIDs, sorted ascending.
fn list_pids(files: &dyn SystemFiles) -> io::Result<Vec<Pid>> {
    let mut pids: Vec<Pid> = files
        .read_dir("/proc")?
        .into_iter()
        .filter(|entry| {
            !entry.name.is_empty() && entry.name.bytes().all(|byte| byte.is_ascii_digit())
        })
        .filter_map(|entry| entry.name.parse::<i64>().ok())
        .filter_map(|pid| Pid::new(pid).ok())
        .collect();
    pids.sort_unstable();
    Ok(pids)
}

fn read_process(scan: &ProcessScan<'_>, owners: &ProcessOwners, pid: Pid) -> ProcessOutcome {
    let started_boot_ns = scan.clock.now_boot_ns();
    let text = match scan.files.read_to_string(&format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(error) => return failed_scan(&error),
    };
    let Some(stat) = parse_pid_stat(&text).filter(|stat| stat.pid == i64::from(pid.get())) else {
        return ProcessOutcome::Unparsable;
    };
    let uid = match owners.get((pid.get(), stat.start_time_ticks)) {
        Some(uid) => Ok(uid),
        None => scan.files.owner_uid(&format!("/proc/{pid}")),
    };
    if matches!(&uid, Err(error) if is_vanished(error)) {
        return ProcessOutcome::Vanished;
    }
    let window = ReadWindow {
        started_boot_ns,
        finished_boot_ns: scan.clock.now_boot_ns().max(started_boot_ns),
    };
    let Some(identity) = ProcessIdentity::new(scan, pid, &stat) else {
        return ProcessOutcome::Unparsable;
    };
    ProcessOutcome::Seen(Box::new(SeenProcess {
        pid,
        stat,
        uid,
        window,
        identity,
    }))
}

/// One process as read, before its record is built: it carries its
/// display name, which may come from its parent.
struct SeenProcess {
    pid: Pid,
    stat: PidStat,
    uid: io::Result<u32>,
    window: ReadWindow,
    identity: ProcessIdentity,
}

impl SeenProcess {
    /// One instance of a PID: names and owners are cached per start time.
    fn cache_key(&self) -> (i32, u64) {
        (self.pid.get(), self.stat.start_time_ticks)
    }

    /// Its one record (D-79): the CPU ticks as the value, the rest of the
    /// `stat` and the owner as tags, the name and command line (D-73) in
    /// the channel.
    fn record(&self, scan: &ProcessScan<'_>, name: &str, command: &str) -> Option<Observation> {
        let channel = if command.is_empty() {
            name.to_owned()
        } else {
            format!("{name}{}{command}", naming::PROCESS_COMMAND_SEPARATOR)
        };
        let descriptor = self.identity.descriptor(scan, &channel)?;
        let mut record =
            u64_outcome_record(scan.plan, &descriptor, cpu_ticks(&self.stat), self.window)?;
        let rss = rss_bytes(&self.stat, scan.page_size);
        if let Err(code) = rss {
            record.reason_codes.push(code.to_owned());
        }
        if self.uid.is_err() {
            record.reason_codes.push(OWNER_UNREADABLE.to_owned());
        }
        let stat = ProcessStat {
            rss_bytes: rss.ok(),
            uid: self.uid.as_ref().ok().copied(),
            ppid: self.stat.ppid,
            state: self.stat.state,
            threads: self.stat.num_threads,
            reaped_ticks: reaped_ticks(&self.stat),
        };
        record.reason_codes.extend(stat.reason_codes());
        Some(record)
    }
}

fn is_vanished(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(ESRCH)
}

fn failed_scan(error: &io::Error) -> ProcessOutcome {
    if is_vanished(error) {
        ProcessOutcome::Vanished
    } else if error.kind() == io::ErrorKind::PermissionDenied {
        ProcessOutcome::Denied
    } else {
        ProcessOutcome::Unreadable
    }
}

/// Identity-derived fields of one process's record.
struct ProcessIdentity {
    entity_id: EntityId,
    scope: Scope,
    device_id: String,
    device_path: String,
}

/// The source of the record: its CPU ticks, the value it carries.
const CPU_TICKS_CHANNEL: &str = "cpu.ticks";

impl ProcessIdentity {
    fn new(scan: &ProcessScan<'_>, pid: Pid, stat: &PidStat) -> Option<Self> {
        let key = ProcessKey {
            host_id: scan.keys.host_id.clone(),
            boot_id: scan.keys.boot_id.clone(),
            pid_namespace: scan.pid_namespace.clone(),
            pid,
            start_time_ticks: stat.start_time_ticks,
        };
        Some(Self {
            entity_id: key.entity_id().ok()?,
            scope: invariant(Scope::new(entity::SCOPE_PROCESS), entity::SCOPE_PROCESS),
            device_id: format!("pid{pid}.{}", stat.start_time_ticks),
            device_path: format!("/proc/{pid}"),
        })
    }

    fn descriptor(&self, scan: &ProcessScan<'_>, channel: &str) -> Option<SourceDescriptor> {
        let origin = format!("procfs:{}/stat:{}", self.device_path, self.device_id);
        let spec = SourceSpec {
            device_id: &self.device_id,
            channel: CPU_TICKS_CHANNEL,
            entity: &self.entity_id,
            scope: &self.scope,
            metric_id: ids::PROCESS_CPU_TICKS,
            group: groups::PROCESS,
            native_unit: native::TICKS,
            raw_kind: RawKind::Counter,
            driver: DRIVER,
            device_path: &self.device_path,
            origin: &origin,
            update_interval_ms: None,
            label: None,
            rank: 0,
        };
        let mut descriptor = descriptor_for(scan.config, scan.keys, &spec)?;
        // Each ProcessKey is its own incarnation (the start time is part of
        // the source id), so the generation never needs bumping.
        descriptor.generation = 1;
        descriptor.channel = channel.to_owned();
        Some(descriptor)
    }
}

/// Reason code of an owner that could not be read (the process stays).
const OWNER_UNREADABLE: &str = "OWNER_UNREADABLE";

/// utime + stime: two same-unit counters of one read, summed exactly.
fn cpu_ticks(stat: &PidStat) -> Result<u64, DerivationFailure> {
    match (stat.utime, stat.stime) {
        (Field::Value(user), Field::Value(system)) => user
            .checked_add(system)
            .ok_or((ReadStatus::Error, "CPU_TICKS_OVERFLOW")),
        (Field::Absent, _) | (_, Field::Absent) => Err((ReadStatus::Unsupported, FIELD_ABSENT)),
        _ => Err((ReadStatus::Error, PARSE_ERROR)),
    }
}

/// The reaped children's time (`cutime + cstime`); `None` when either
/// column is missing or unparsable, so it is left out, never sent as zero.
fn reaped_ticks(stat: &PidStat) -> Option<u64> {
    match (stat.cutime, stat.cstime) {
        (Field::Value(user), Field::Value(system)) => Some(user.saturating_add(system)),
        _ => None,
    }
}

/// Resident pages × page size, without overflow and never guessed; the
/// error is the reason code the record carries instead.
fn rss_bytes(stat: &PidStat, page_size: Option<u64>) -> Result<u64, &'static str> {
    let page_size = page_size.ok_or(process_stat::PAGE_SIZE_UNKNOWN)?;
    match stat.rss_pages {
        Field::Value(pages) => pages
            .checked_mul(page_size)
            .ok_or(process_stat::RSS_BYTES_OVERFLOW),
        Field::Absent => Err(process_stat::RSS_FIELD_ABSENT),
        Field::Invalid => Err(process_stat::RSS_PARSE_ERROR),
    }
}
