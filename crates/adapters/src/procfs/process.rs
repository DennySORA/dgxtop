//! `process` read group (paged, not catalogued): scans `/proc/[0-9]*` in PID
//! order, at most [`MAX_SCANNED_PROCESSES`] processes, and emits per
//! ProcessKey `process.cpu.ticks` (utime + stime from one read: a lossless
//! decode of two same-unit counters), `process.rss` (pages × page size,
//! bytes) and `process.uid` (owner of `/proc/<pid>`, static).
//!
//! Convention for the process collector: the process's display name
//! ([`command`](super::command), D-65) travels in `provenance.channel` and
//! `/proc/<pid>` in `provenance.device_path`. A process that vanishes
//! mid-scan is skipped, not reported as an error.

use std::collections::{HashMap, HashSet};
use std::io;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{EntityId, Pid, PidNamespace, ProcessKey, Scope};
use dgxtop_core::metric::{Scalar, ids, native};
use dgxtop_core::naming::{self, entity, groups};
use dgxtop_core::observation::{Observation, ReadPlan, SourceDescriptor};
use dgxtop_core::quality::{Quality, RawKind, ReadStatus};
use dgxtop_core::text::sanitize_command_name;

use super::catalog::{CatalogConfig, KeyContext, SourceSpec, descriptor_for};
use super::command::{Named, command_text, name_process, needs_command_line};
use super::locator::DRIVER;
use super::mechanics::{ReadSnapshot, invariant, io_failure, joined_error_code};
use super::parse::{Field, PidStat, parse_pid_stat};
use super::records::{
    DerivationFailure, FIELD_ABSENT, PARSE_ERROR, ReadWindow, io_failed_record, ok_record,
    u64_outcome_record,
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

/// Scans processes in PID order; problems are summarized in the final
/// page's error code.
pub(crate) fn scan(scan: &ProcessScan<'_>, names: &mut ProcessNames) -> ReadSnapshot {
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
        match read_process(scan, *pid) {
            ProcessOutcome::Seen(process) => seen.push(*process),
            ProcessOutcome::Vanished => {}
            ProcessOutcome::Denied => tally.denied += 1,
            ProcessOutcome::Unreadable => tally.unreadable += 1,
            ProcessOutcome::Unparsable => tally.unparsable += 1,
        }
    }
    codes.extend(tally_codes(&tally));
    let display_names = names.resolve(scan.files, &seen);
    let records = seen
        .iter()
        .zip(&display_names)
        .flat_map(|(process, (name, command))| process.records(scan, name, command))
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

fn read_process(scan: &ProcessScan<'_>, pid: Pid) -> ProcessOutcome {
    let started_boot_ns = scan.clock.now_boot_ns();
    let text = match scan.files.read_to_string(&format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(error) => return failed_scan(&error),
    };
    let Some(stat) = parse_pid_stat(&text).filter(|stat| stat.pid == i64::from(pid.get())) else {
        return ProcessOutcome::Unparsable;
    };
    let uid = scan.files.owner_uid(&format!("/proc/{pid}"));
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

/// One process as read, before its records are built: they carry its
/// display name, which may come from its parent.
struct SeenProcess {
    pid: Pid,
    stat: PidStat,
    uid: io::Result<u32>,
    window: ReadWindow,
    identity: ProcessIdentity,
}

impl SeenProcess {
    /// One instance of a PID: names are cached per start time.
    fn cache_key(&self) -> (i32, u64) {
        (self.pid.get(), self.stat.start_time_ticks)
    }

    /// Its records; the uid record's channel also carries the command line
    /// (D-73).
    fn records(&self, scan: &ProcessScan<'_>, name: &str, command: &str) -> Vec<Observation> {
        let identity = &self.identity;
        let with_command = if command.is_empty() {
            name.to_owned()
        } else {
            format!("{name}{}{command}", naming::PROCESS_COMMAND_SEPARATOR)
        };
        [
            identity.cpu_ticks_record(scan, &self.stat, self.window, name),
            identity.rss_record(scan, &self.stat, self.window, name),
            identity.uid_record(scan, &self.uid, self.window, &with_command),
        ]
        .into_iter()
        .flatten()
        .collect()
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

/// Identity-derived fields shared by the three records of one process.
struct ProcessIdentity {
    entity_id: EntityId,
    scope: Scope,
    device_id: String,
    device_path: String,
}

/// One emitted process metric: short name, metric id, native unit, kind.
struct ProcessMetric {
    short_name: &'static str,
    metric_id: &'static str,
    native_unit: &'static str,
    raw_kind: RawKind,
    origin_file: &'static str,
}

const CPU_TICKS: ProcessMetric = ProcessMetric {
    short_name: "cpu.ticks",
    metric_id: ids::PROCESS_CPU_TICKS,
    native_unit: native::TICKS,
    raw_kind: RawKind::Counter,
    origin_file: "stat",
};
const RSS: ProcessMetric = ProcessMetric {
    short_name: "rss",
    metric_id: ids::PROCESS_RSS,
    native_unit: native::BYTES,
    raw_kind: RawKind::Gauge,
    origin_file: "stat",
};
const UID: ProcessMetric = ProcessMetric {
    short_name: "uid",
    metric_id: ids::PROCESS_UID,
    native_unit: native::COUNT,
    raw_kind: RawKind::Static,
    origin_file: "owner",
};

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

    fn descriptor(
        &self,
        scan: &ProcessScan<'_>,
        metric: &ProcessMetric,
        name: &str,
    ) -> Option<SourceDescriptor> {
        let origin = format!(
            "procfs:{}/{}:{}",
            self.device_path, metric.origin_file, self.device_id
        );
        let spec = SourceSpec {
            device_id: &self.device_id,
            channel: metric.short_name,
            entity: &self.entity_id,
            scope: &self.scope,
            metric_id: metric.metric_id,
            group: groups::PROCESS,
            native_unit: metric.native_unit,
            raw_kind: metric.raw_kind,
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
        descriptor.channel = name.to_owned();
        Some(descriptor)
    }

    fn cpu_ticks_record(
        &self,
        scan: &ProcessScan<'_>,
        stat: &PidStat,
        window: ReadWindow,
        name: &str,
    ) -> Option<Observation> {
        let descriptor = self.descriptor(scan, &CPU_TICKS, name)?;
        u64_outcome_record(scan.plan, &descriptor, cpu_ticks(stat), window)
    }

    fn rss_record(
        &self,
        scan: &ProcessScan<'_>,
        stat: &PidStat,
        window: ReadWindow,
        name: &str,
    ) -> Option<Observation> {
        let descriptor = self.descriptor(scan, &RSS, name)?;
        u64_outcome_record(
            scan.plan,
            &descriptor,
            rss_bytes(stat, scan.page_size),
            window,
        )
    }

    fn uid_record(
        &self,
        scan: &ProcessScan<'_>,
        uid: &io::Result<u32>,
        window: ReadWindow,
        name: &str,
    ) -> Option<Observation> {
        let descriptor = self.descriptor(scan, &UID, name)?;
        match uid {
            Ok(uid) => ok_record(
                scan.plan,
                &descriptor,
                Scalar::from_u64(u64::from(*uid)),
                Quality::Verified,
                window,
                Vec::new(),
            ),
            Err(error) => io_failed_record(scan.plan, &descriptor, error, window),
        }
    }
}

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

/// Resident pages × page size, without overflow and never guessed.
fn rss_bytes(stat: &PidStat, page_size: Option<u64>) -> Result<u64, DerivationFailure> {
    let page_size = page_size.ok_or((ReadStatus::Unsupported, "PAGE_SIZE_UNKNOWN"))?;
    match stat.rss_pages {
        Field::Value(pages) => pages
            .checked_mul(page_size)
            .ok_or((ReadStatus::Error, "RSS_BYTES_OVERFLOW")),
        Field::Absent => Err((ReadStatus::Unsupported, FIELD_ABSENT)),
        Field::Invalid => Err((ReadStatus::Error, PARSE_ERROR)),
    }
}
