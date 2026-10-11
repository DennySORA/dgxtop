//! The process tree (D-77): every process's parent and CPU time from its
//! `stat`, read once per probe, so a probed process's descendants at any
//! depth can be summed. A tree whose list cannot be read is no tree: its
//! readings say `None` rather than zero descendants.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::process_probe::ChildUsage;

use super::super::parse::{Field, parse_pid_stat};
use crate::common::SystemFiles;

/// Processes a tree reads at most.
const MAX_TREE_PROCESSES: usize = 65_536;

/// One process of the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Node {
    parent: i64,
    /// Its own CPU time (`utime + stime`), ticks.
    own_ticks: u64,
    /// Its reaped children's (`cutime + cstime`), ticks.
    reaped_ticks: u64,
    rss_pages: Option<u64>,
}

/// The processes by PID, and each parent's children.
#[derive(Debug, Default)]
pub(super) struct ProcessTree {
    nodes: BTreeMap<i64, Node>,
    children: BTreeMap<i64, Vec<i64>>,
}

impl ProcessTree {
    /// Reads `/proc`; `None` when it cannot be listed.
    pub(super) fn scan(files: &dyn SystemFiles) -> Option<Self> {
        let entries = files.read_dir("/proc").ok()?;
        let mut tree = Self::default();
        let pids = entries
            .iter()
            .filter(|entry| entry.is_dir)
            .filter_map(|entry| entry.name.parse::<i64>().ok())
            .filter(|pid| *pid > 0)
            .take(MAX_TREE_PROCESSES);
        for pid in pids {
            // A process that ended since the listing is simply not there.
            let Some(stat) = files
                .read_to_string(&format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|text| parse_pid_stat(&text))
                .filter(|stat| stat.pid == pid)
            else {
                continue;
            };
            let Some(parent) = stat.ppid else {
                continue;
            };
            tree.nodes.insert(
                pid,
                Node {
                    parent,
                    own_ticks: sum(stat.utime, stat.stime),
                    reaped_ticks: sum(stat.cutime, stat.cstime),
                    rss_pages: value(stat.rss_pages),
                },
            );
            tree.children.entry(parent).or_default().push(pid);
        }
        Some(tree)
    }

    /// What the descendants of `pid` use: those running and, through each
    /// one's reaped time and `pid`'s own, those reaped since.
    pub(super) fn usage_of(&self, pid: i64, page_size: Option<u64>) -> ChildUsage {
        let mut processes = 0;
        let mut cpu_ticks = self.nodes.get(&pid).map_or(0, |node| node.reaped_ticks);
        let mut rss_pages = Some(0u64);
        let mut seen = BTreeSet::from([pid]);
        let mut pending: Vec<i64> = self.children_of(pid).to_vec();
        while let Some(child) = pending.pop() {
            // A PID reused while the list was read could close a loop.
            if !seen.insert(child) {
                continue;
            }
            let Some(node) = self.nodes.get(&child) else {
                continue;
            };
            processes += 1;
            cpu_ticks = cpu_ticks
                .saturating_add(node.own_ticks)
                .saturating_add(node.reaped_ticks);
            rss_pages = rss_pages
                .zip(node.rss_pages)
                .map(|(sum, pages)| sum.saturating_add(pages));
            pending.extend_from_slice(self.children_of(child));
        }
        ChildUsage {
            processes,
            cpu_ticks,
            rss_bytes: rss_pages
                .zip(page_size)
                .map(|(pages, size)| pages.saturating_mul(size)),
        }
    }

    fn children_of(&self, pid: i64) -> &[i64] {
        self.children.get(&pid).map_or(&[], Vec::as_slice)
    }
}

fn value(field: Field<u64>) -> Option<u64> {
    match field {
        Field::Value(value) => Some(value),
        Field::Absent | Field::Invalid => None,
    }
}

/// Two tick counters; a missing one counts as zero, as a kernel without it
/// has spent none there.
fn sum(first: Field<u64>, second: Field<u64>) -> u64 {
    value(first)
        .unwrap_or(0)
        .saturating_add(value(second).unwrap_or(0))
}
