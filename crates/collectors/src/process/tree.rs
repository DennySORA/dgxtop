//! The process tree (D-77, D-79): every scanned process's parent, CPU time
//! and resident memory, taken from the records of the whole scan before
//! the table keeps only its `max_processes` rows, so a GPU process's
//! descendants at any depth count even where the table drops them.

use std::collections::{HashMap, HashSet};

/// One process of the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TreeNode {
    pub parent: i32,
    /// Its own CPU time (`utime + stime`), ticks; `None` when its read
    /// failed.
    pub own_ticks: Option<u64>,
    /// Its reaped children's (`cutime + cstime`), ticks; `None` when the
    /// adapter could not read it.
    pub reaped_ticks: Option<u64>,
    pub rss_bytes: Option<u64>,
}

/// What a process's descendants use, as counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DescendantUsage {
    /// Descendants running now.
    pub processes: u64,
    /// CPU time, clock ticks, of the descendants running now and of those
    /// reaped since it started (the kernel adds a reaped child's time to
    /// its parent's): it only grows while the descendants stay in the tree.
    /// `None` when one's own time is unknown: a total that left it out
    /// would drop and the next would count its whole time as new. The same
    /// holds for an unknown reaped time, the root's included.
    pub cpu_ticks: Option<u64>,
    /// Resident memory of the descendants running now; `None` when one's
    /// is unknown.
    pub rss_bytes: Option<u64>,
}

/// The processes by PID, and each parent's children.
#[derive(Debug, Default)]
pub(crate) struct ProcessTree {
    nodes: HashMap<i32, TreeNode>,
    children: HashMap<i32, Vec<i32>>,
}

impl ProcessTree {
    /// The tree of `processes`; of two with one PID (a PID reused within
    /// the inputs) the later one counts.
    pub(crate) fn of(processes: impl IntoIterator<Item = (i32, TreeNode)>) -> Self {
        let nodes: HashMap<i32, TreeNode> = processes.into_iter().collect();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for (pid, node) in &nodes {
            children.entry(node.parent).or_default().push(*pid);
        }
        Self { nodes, children }
    }

    /// What the descendants of `pid` use: those running and, through each
    /// one's reaped time and `pid`'s own, those reaped since. `None` when
    /// `pid` is not in the tree.
    pub(crate) fn usage_of(&self, pid: i32) -> Option<DescendantUsage> {
        let root = self.nodes.get(&pid)?;
        let mut usage = DescendantUsage {
            processes: 0,
            cpu_ticks: root.reaped_ticks,
            rss_bytes: Some(0),
        };
        let mut seen = HashSet::from([pid]);
        let mut pending: Vec<i32> = self.children_of(pid).to_vec();
        while let Some(child) = pending.pop() {
            // A PID reused while the list was read could close a loop.
            if !seen.insert(child) {
                continue;
            }
            let Some(node) = self.nodes.get(&child) else {
                continue;
            };
            usage.processes += 1;
            usage.cpu_ticks = usage
                .cpu_ticks
                .zip(node.own_ticks.zip(node.reaped_ticks))
                .map(|(sum, (own, reaped))| sum.saturating_add(own).saturating_add(reaped));
            usage.rss_bytes = usage
                .rss_bytes
                .zip(node.rss_bytes)
                .map(|(sum, bytes)| sum.saturating_add(bytes));
            pending.extend_from_slice(self.children_of(child));
        }
        Some(usage)
    }

    fn children_of(&self, pid: i32) -> &[i32] {
        self.children.get(&pid).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(parent: i32, own_ticks: u64, reaped_ticks: u64, rss_bytes: Option<u64>) -> TreeNode {
        TreeNode {
            parent,
            own_ticks: Some(own_ticks),
            reaped_ticks: Some(reaped_ticks),
            rss_bytes,
        }
    }

    /// Descendants at any depth count, with the root's and each one's
    /// reaped time; other branches do not.
    #[test]
    fn descendants_at_any_depth_are_summed() {
        let tree = ProcessTree::of([
            (1, node(0, 5, 0, Some(1))),
            (10, node(1, 100, 7, Some(1_000))),
            (11, node(10, 20, 3, Some(200))),
            (12, node(11, 4, 0, Some(40))),
            (20, node(1, 999, 999, Some(9_999))),
        ]);
        assert_eq!(
            tree.usage_of(10),
            Some(DescendantUsage {
                processes: 2,
                cpu_ticks: Some(7 + 20 + 3 + 4),
                rss_bytes: Some(240),
            })
        );
        assert_eq!(
            tree.usage_of(12),
            Some(DescendantUsage {
                processes: 0,
                cpu_ticks: Some(0),
                rss_bytes: Some(0),
            })
        );
        assert_eq!(tree.usage_of(99), None, "not in the tree");
    }

    #[test]
    fn an_unknown_cpu_time_makes_the_sum_unknown() {
        let mut unread = node(10, 0, 0, Some(1));
        unread.own_ticks = None;
        let tree = ProcessTree::of([(10, node(1, 0, 4, Some(1))), (11, unread)]);
        let usage = tree.usage_of(10).unwrap();
        assert_eq!(usage.processes, 1);
        assert_eq!(usage.cpu_ticks, None);
        assert_eq!(usage.rss_bytes, Some(1));
        let mut unreaped = node(10, 0, 0, Some(1));
        unreaped.reaped_ticks = None;
        let tree = ProcessTree::of([(10, node(1, 0, 4, Some(1))), (11, unreaped)]);
        assert_eq!(tree.usage_of(10).unwrap().cpu_ticks, None);
        let mut root = node(1, 0, 0, Some(1));
        root.reaped_ticks = None;
        let tree = ProcessTree::of([(10, root)]);
        assert_eq!(tree.usage_of(10).unwrap().cpu_ticks, None, "the root's");
    }

    #[test]
    fn an_unknown_memory_makes_the_sum_unknown() {
        let tree = ProcessTree::of([(10, node(1, 0, 0, None)), (11, node(10, 1, 0, None))]);
        let usage = tree.usage_of(10).unwrap();
        assert_eq!(usage.processes, 1);
        assert_eq!(usage.rss_bytes, None);
    }

    /// A loop (PIDs reused while the scan ran) is walked once.
    #[test]
    fn a_loop_ends() {
        let tree = ProcessTree::of([(10, node(11, 1, 0, Some(1))), (11, node(10, 2, 0, Some(1)))]);
        let usage = tree.usage_of(10).unwrap();
        assert_eq!(usage.processes, 1);
        assert_eq!(usage.cpu_ticks, Some(2));
    }
}
