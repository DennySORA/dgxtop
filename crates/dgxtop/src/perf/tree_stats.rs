//! Process-tree resource sampling for the benches.
//!
//! The performance gates are defined over the parent *plus every adapter
//! host*, including quarantined ones, so the composition root samples
//! `[own pid] + SupervisorHandle::managed_pids()`. A value that cannot be
//! read is `None` with a reason, never 0; totals only add what was read and
//! `complete` says whether anything was missing.

use std::fs;
use std::path::{Path, PathBuf};

use dgxtop_core::text::sanitize_message;

/// Upper bound on pids per sample (parent + at most 8 hosts in practice).
pub const MAX_SAMPLED_PIDS: usize = 64;
/// Upper bound on directory entries counted per pid.
const MAX_COUNTED_ENTRIES: usize = 1 << 16;
/// Upper bound on bytes read from one pseudo file.
const MAX_PROC_FILE_BYTES: usize = 64 * 1024;
/// `utime` is field 14 of `/proc/<pid>/stat`; after the `)` that ends the
/// command name the first field (state) is field 3.
const STAT_UTIME_INDEX: usize = 14 - 3;

/// Readings of one process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidSample {
    pub pid: i32,
    pub rss_bytes: Option<u64>,
    pub threads: Option<u64>,
    pub open_fds: Option<u64>,
    /// utime + stime + cutime + cstime in clock ticks (`CLK_TCK`).
    pub cpu_ticks: Option<u64>,
    /// Why one or more readings are missing.
    pub missing: Vec<String>,
}

impl PidSample {
    fn empty(pid: i32) -> Self {
        Self {
            pid,
            rss_bytes: None,
            threads: None,
            open_fds: None,
            cpu_ticks: None,
            missing: Vec::new(),
        }
    }

    fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Sum over the sampled tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSample {
    pub rss_bytes: u64,
    pub threads: u64,
    pub open_fds: u64,
    pub cpu_ticks: u64,
    pub per_pid: Vec<PidSample>,
    /// Every requested pid was fully read.
    pub complete: bool,
}

/// Namespace for process-tree sampling.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessTreeStats;

impl ProcessTreeStats {
    /// Samples `pids` from `/proc`.
    pub fn sample(pids: &[i32]) -> TreeSample {
        Self::sample_at(Path::new("/proc"), pids)
    }

    /// Samples `pids` from a proc tree rooted at `proc_root` (tests).
    pub fn sample_at(proc_root: &Path, pids: &[i32]) -> TreeSample {
        let mut per_pid: Vec<PidSample> = pids
            .iter()
            .take(MAX_SAMPLED_PIDS)
            .map(|pid| sample_pid(proc_root, *pid))
            .collect();
        if pids.len() > MAX_SAMPLED_PIDS
            && let Some(last) = per_pid.last_mut()
        {
            last.missing.push(format!(
                "only the first {MAX_SAMPLED_PIDS} pids were sampled"
            ));
        }
        let sum = |pick: fn(&PidSample) -> Option<u64>| {
            per_pid
                .iter()
                .filter_map(pick)
                .fold(0u64, u64::saturating_add)
        };
        TreeSample {
            rss_bytes: sum(|sample| sample.rss_bytes),
            threads: sum(|sample| sample.threads),
            open_fds: sum(|sample| sample.open_fds),
            cpu_ticks: sum(|sample| sample.cpu_ticks),
            complete: per_pid.iter().all(PidSample::is_complete),
            per_pid,
        }
    }
}

fn sample_pid(proc_root: &Path, pid: i32) -> PidSample {
    let mut sample = PidSample::empty(pid);
    if pid <= 0 {
        sample.missing.push(format!("invalid pid {pid}"));
        return sample;
    }
    let directory = proc_root.join(pid.to_string());
    read_status(&directory, &mut sample);
    read_stat(&directory, &mut sample);
    match count_entries(&directory.join("fd")) {
        Ok(count) => sample.open_fds = Some(count),
        Err(reason) => sample.missing.push(format!("fd: {reason}")),
    }
    if sample.threads.is_none()
        && let Ok(count) = count_entries(&directory.join("task"))
    {
        sample.threads = Some(count);
        sample
            .missing
            .retain(|reason| !reason.starts_with("threads:"));
    }
    sample
}

fn read_status(directory: &Path, sample: &mut PidSample) {
    let text = match read_bounded(&directory.join("status")) {
        Ok(text) => text,
        Err(reason) => {
            sample.missing.push(format!("status: {reason}"));
            return;
        }
    };
    match status_value(&text, "VmRSS:").and_then(|kib| kib.checked_mul(1024)) {
        Some(bytes) => sample.rss_bytes = Some(bytes),
        None => sample
            .missing
            .push("rss: no VmRSS line (zombie or kernel thread)".into()),
    }
    match status_value(&text, "Threads:") {
        Some(threads) => sample.threads = Some(threads),
        None => sample.missing.push("threads: no Threads line".into()),
    }
}

/// First number after `key` in `/proc/<pid>/status` (units stripped).
fn status_value(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|number| number.parse().ok())
}

fn read_stat(directory: &Path, sample: &mut PidSample) {
    match read_bounded(&directory.join("stat")).and_then(|text| parse_cpu_ticks(&text)) {
        Ok(ticks) => sample.cpu_ticks = Some(ticks),
        Err(reason) => sample.missing.push(format!("stat: {reason}")),
    }
}

/// utime + stime + cutime + cstime. The command name may contain spaces and
/// parentheses, so fields are counted from the last `)`.
fn parse_cpu_ticks(text: &str) -> Result<u64, String> {
    let close = text
        .rfind(')')
        .ok_or_else(|| "missing command terminator".to_owned())?;
    let fields: Vec<&str> = text[close + 1..].split_whitespace().collect();
    let tick = |offset: usize| -> Result<u64, String> {
        let field = fields
            .get(STAT_UTIME_INDEX + offset)
            .ok_or_else(|| "truncated stat line".to_owned())?;
        // cutime/cstime are signed in the kernel ABI; negative is corrupt.
        let value: i64 = field
            .parse()
            .map_err(|_| format!("non-numeric tick field {offset}"))?;
        u64::try_from(value).map_err(|_| format!("negative tick field {offset}"))
    };
    (0..4).try_fold(
        0u64,
        |total, offset| Ok(total.saturating_add(tick(offset)?)),
    )
}

fn count_entries(directory: &Path) -> Result<u64, String> {
    let entries = fs::read_dir(directory).map_err(|error| sanitize_message(&error.to_string()))?;
    let count = entries.take(MAX_COUNTED_ENTRIES).count();
    u64::try_from(count).map_err(|_| "entry count overflow".to_owned())
}

fn read_bounded(path: &PathBuf) -> Result<String, String> {
    use std::io::Read;
    let file = fs::File::open(path).map_err(|error| sanitize_message(&error.to_string()))?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(MAX_PROC_FILE_BYTES).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|error| sanitize_message(&error.to_string()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FakeProc(PathBuf);

    impl FakeProc {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "dgxtop-runtime-proc-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn process(&self, pid: i32, status: &str, stat: &str, fds: usize) {
            let directory = self.0.join(pid.to_string());
            fs::create_dir_all(directory.join("fd")).unwrap();
            fs::write(directory.join("status"), status).unwrap();
            fs::write(directory.join("stat"), stat).unwrap();
            for index in 0..fds {
                fs::write(directory.join("fd").join(index.to_string()), "").unwrap();
            }
        }
    }

    impl Drop for FakeProc {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_or_invalid_pids_are_reported_not_zeroed() {
        let pid = i32::try_from(std::process::id()).unwrap();
        let own = ProcessTreeStats::sample(&[pid]);
        let sample = ProcessTreeStats::sample(&[pid, i32::MAX, 0, -4]);
        assert!(!sample.complete);
        assert_eq!(sample.per_pid.len(), 4);
        for missing in &sample.per_pid[1..] {
            assert!(missing.rss_bytes.is_none());
            assert!(missing.cpu_ticks.is_none());
            assert!(!missing.missing.is_empty());
        }
        // Totals only include what was read (RSS may move a little).
        assert!(sample.rss_bytes >= own.rss_bytes / 2);
    }

    #[test]
    fn fake_tree_is_parsed_exactly() {
        let proc = FakeProc::new();
        proc.process(
            10,
            "Name:\tworker\nVmRSS:\t    2048 kB\nThreads:\t7\n",
            "10 (evil ) name) S 1 1 1 0 -1 4194560 0 0 0 0 10 20 3 4 20 0 7 0\n",
            3,
        );
        proc.process(
            11,
            "Name:\tzombie\nThreads:\t1\n",
            "11 (z) Z 1 1 1 0 -1 0 0 0 0 0 1 1 0 0 20 0 1 0\n",
            0,
        );
        let sample = ProcessTreeStats::sample_at(&proc.0, &[10, 11]);
        let worker = &sample.per_pid[0];
        assert_eq!(worker.rss_bytes, Some(2 * 1024 * 1024));
        assert_eq!(worker.threads, Some(7));
        assert_eq!(worker.open_fds, Some(3));
        assert_eq!(worker.cpu_ticks, Some(37));
        assert!(worker.missing.is_empty());
        let zombie = &sample.per_pid[1];
        assert_eq!(zombie.rss_bytes, None);
        assert!(zombie.missing.iter().any(|reason| reason.contains("VmRSS")));
        assert_eq!(sample.rss_bytes, 2 * 1024 * 1024);
        assert_eq!(sample.threads, 8);
        assert_eq!(sample.cpu_ticks, 39);
        assert!(!sample.complete);
    }

    #[test]
    fn corrupt_stat_lines_are_rejected() {
        assert!(parse_cpu_ticks("1 (x) S 1").is_err());
        assert!(parse_cpu_ticks("no parenthesis").is_err());
        assert!(parse_cpu_ticks("1 (x) S 1 1 1 0 -1 0 0 0 0 0 -5 1 0 0").is_err());
        assert!(parse_cpu_ticks("1 (x) S 1 1 1 0 -1 0 0 0 0 0 a 1 0 0").is_err());
        assert_eq!(
            parse_cpu_ticks("1 (x) S 1 1 1 0 -1 0 0 0 0 0 5 6 7 8 20").unwrap(),
            26
        );
    }

    #[test]
    fn sample_size_is_bounded() {
        let pid = i32::try_from(std::process::id()).unwrap();
        let pids = vec![pid; MAX_SAMPLED_PIDS + 10];
        let sample = ProcessTreeStats::sample(&pids);
        assert_eq!(sample.per_pid.len(), MAX_SAMPLED_PIDS);
        assert!(!sample.complete);
    }
}
