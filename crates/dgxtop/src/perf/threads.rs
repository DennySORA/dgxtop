//! CPU time of the calling thread (`/proc/thread-self/stat`). The REF bench
//! runs its measuring threads inside the measured process; `PERFORMANCE.md`
//! requires the measurer's own cost to stay out of the dgxtop CPU figure
//! (and be reported), so those threads account for themselves here.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// `utime` is field 14 of a stat line; fields are counted after the `)` that
/// ends the command name (which may itself contain spaces and parentheses),
/// where the first field is field 3.
const UTIME_AFTER_NAME: usize = 14 - 3;

/// `utime + stime` in clock ticks from one `/proc/.../stat` line.
pub fn parse_thread_cpu_ticks(stat: &str) -> Option<u64> {
    let close = stat.rfind(')')?;
    let mut fields = stat
        .get(close + 1..)?
        .split_whitespace()
        .skip(UTIME_AFTER_NAME);
    let utime: u64 = fields.next()?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    utime.checked_add(stime)
}

/// Command name and `utime + stime` of one `/proc/.../stat` line.
pub fn parse_named_cpu_ticks(stat: &str) -> Option<(String, u64)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_owned();
    Some((name, parse_thread_cpu_ticks(stat)?))
}

/// One thread of this process with its CPU ticks so far.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ThreadTicks {
    pub tid: u32,
    pub name: String,
    pub ticks: u64,
}

/// Every thread of this process (`/proc/self/task`); unreadable threads
/// (exited meanwhile) are skipped.
pub fn process_thread_ticks() -> Vec<ThreadTicks> {
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let tid = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let (name, ticks) = parse_named_cpu_ticks(&stat)?;
            Some(ThreadTicks { tid, name, ticks })
        })
        .collect()
}

/// CPU ticks used so far by the calling thread.
pub fn current_thread_cpu_ticks() -> Option<u64> {
    std::fs::read_to_string("/proc/thread-self/stat")
        .ok()
        .and_then(|text| parse_thread_cpu_ticks(&text))
}

/// CPU used by harness threads, each publishing its own running total.
#[derive(Debug, Clone, Default)]
pub struct HarnessCpu {
    slots: Arc<std::sync::Mutex<Vec<Arc<AtomicU64>>>>,
}

impl HarnessCpu {
    /// A slot for one harness thread; it calls [`ThreadCpuSlot::refresh`]
    /// after each unit of work.
    pub fn register(&self) -> ThreadCpuSlot {
        let slot = Arc::new(AtomicU64::new(current_thread_cpu_ticks().unwrap_or(0)));
        if let Ok(mut slots) = self.slots.lock() {
            slots.push(Arc::clone(&slot));
        }
        ThreadCpuSlot { ticks: slot }
    }

    /// Sum of the latest totals of every registered thread.
    pub fn total_ticks(&self) -> u64 {
        self.slots.lock().map_or(0, |slots| {
            slots
                .iter()
                .map(|slot| slot.load(Ordering::Acquire))
                .fold(0, u64::saturating_add)
        })
    }
}

/// The running CPU total of one harness thread.
#[derive(Debug)]
pub struct ThreadCpuSlot {
    ticks: Arc<AtomicU64>,
}

impl ThreadCpuSlot {
    /// Publishes the calling thread's CPU total (call from the owning thread).
    pub fn refresh(&self) {
        if let Some(ticks) = current_thread_cpu_ticks() {
            self.ticks.store(ticks, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_lines_with_odd_names_are_parsed() {
        assert_eq!(
            parse_thread_cpu_ticks(
                "10 (evil ) name) S 1 1 1 0 -1 4194560 0 0 0 0 10 20 3 4 20 0 7 0"
            ),
            Some(30)
        );
        assert_eq!(parse_thread_cpu_ticks("1 (x) S 1 1"), None);
        assert_eq!(parse_thread_cpu_ticks("no parenthesis"), None);
        assert_eq!(
            parse_thread_cpu_ticks("1 (x) S 1 1 1 0 -1 0 0 0 0 0 a 2"),
            None
        );
    }

    #[test]
    fn thread_names_and_ticks_are_parsed() {
        assert_eq!(
            parse_named_cpu_ticks(
                "10 (dgxtop-manager) S 1 1 1 0 -1 4194560 0 0 0 0 10 20 3 4 20 0 7 0"
            ),
            Some(("dgxtop-manager".to_owned(), 30))
        );
        assert_eq!(parse_named_cpu_ticks("10 no-name S 1"), None);
        let threads = process_thread_ticks();
        assert!(!threads.is_empty(), "this test runs on at least one thread");
    }
}
